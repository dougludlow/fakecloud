//! `AWS::Serverless::Api` / `AWS::Serverless::HttpApi` (explicit, and the
//! implicit `ServerlessRestApi` / `ServerlessHttpApi`) expansion.
//!
//! As the SAM translator does, a REST API is expressed as an OpenAPI (Swagger
//! 2.0) definition -- the template's `DefinitionBody`, or one generated with
//! the stack name as its title -- into which every function `Api` event that
//! targets the API adds its path + method with a Lambda proxy integration,
//! and `Auth` / `Cors` add their authorizers, API-key requirement and
//! preflight `OPTIONS` methods. The `AWS::ApiGateway::RestApi` imports it as
//! its `Body`, followed by a `Deployment` and the `StageName` stage. An HTTP
//! API becomes an `AWS::ApiGatewayV2::Api` with an auto-deploying stage, and
//! each `HttpApi` event an integration + route on it (with its JWT / Lambda
//! authorizer). Each route also gets the `Lambda::Permission` letting API
//! Gateway invoke the function.

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use super::sam_events::{lambda_integration_uri, lambda_permission, sanitize, ApiRoute};

/// A SAM API to expand: its logical id, its properties (Globals merged in),
/// and the resource attributes (`DependsOn`, `Condition`, ...) the native API
/// resource keeps.
pub(super) struct ApiDef {
    pub logical_id: String,
    pub props: Map<String, Value>,
    pub attributes: Map<String, Value>,
}

/// The expansion of one API: native resources plus the `Api.Stage` /
/// `Api.Deployment` reference targets.
pub(super) type ApiExpansion = (Vec<(String, Value)>, Vec<(String, String)>);

fn invalid(api_id: &str, msg: &str) -> String {
    format!(
        "Transform AWS::Serverless-2016-10-31 failed: Resource with id [{api_id}] is invalid. {msg}"
    )
}

fn hash10(v: &Value) -> String {
    let digest = Sha256::digest(v.to_string().as_bytes());
    digest.iter().take(5).map(|b| format!("{b:02x}")).collect()
}

/// SAM's `DefinitionUri`: `s3://bucket/key` or `{Bucket, Key, Version}`.
fn s3_location(uri: &Value) -> Value {
    match uri.as_str().and_then(|s| s.strip_prefix("s3://")) {
        Some(rest) => {
            let (bucket, key) = rest.split_once('/').unwrap_or((rest, ""));
            json!({ "Bucket": bucket, "Key": key })
        }
        None => uri.clone(),
    }
}

/// `/items/{id}` -> `/items/*`, the form a permission's execute-api ARN
/// uses.
fn arn_path(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            if seg.starts_with('{') && seg.ends_with('}') {
                "*"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The `Lambda::Permission` letting API Gateway invoke a route's function,
/// scoped to the API, any stage, the method and the path.
fn route_permission(api_id: &str, route: &ApiRoute) -> Value {
    let method = if route.method == "ANY" {
        "*".to_string()
    } else {
        route.method.clone()
    };
    let resource = match &route.path {
        Some(path) => format!("${{__ApiId__}}/${{__Stage__}}/{method}{}", arn_path(path)),
        None => "${__ApiId__}/${__Stage__}/*".to_string(),
    };
    lambda_permission(
        &route.target,
        "apigateway.amazonaws.com",
        Some(json!({
            "Fn::Sub": [
                format!("arn:${{AWS::Partition}}:execute-api:${{AWS::Region}}:${{AWS::AccountId}}:{resource}"),
                { "__ApiId__": { "Ref": api_id }, "__Stage__": "*" }
            ]
        })),
    )
}

/// The `Lambda::Permission` letting API Gateway call a Lambda authorizer.
fn authorizer_permission(api_id: &str, function_arn: &Value) -> Value {
    json!({
        "Type": "AWS::Lambda::Permission",
        "Properties": {
            "Action": "lambda:InvokeFunction",
            "FunctionName": function_arn,
            "Principal": "apigateway.amazonaws.com",
            "SourceArn": {
                "Fn::Sub": [
                    "arn:${AWS::Partition}:execute-api:${AWS::Region}:${AWS::AccountId}:${__ApiId__}/authorizers/*",
                    { "__ApiId__": { "Ref": api_id } }
                ]
            }
        }
    })
}

fn authorizer_uri(function_arn: &Value) -> Value {
    json!({
        "Fn::Sub": [
            "arn:${AWS::Partition}:apigateway:${AWS::Region}:lambda:path/2015-03-31/functions/${__FunctionArn__}/invocations",
            { "__FunctionArn__": function_arn }
        ]
    })
}

fn string_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

const ALL_METHODS: &[&str] = &["DELETE", "GET", "HEAD", "OPTIONS", "PATCH", "POST", "PUT"];

/// The swagger key for an HTTP method.
fn op_key(method: &str) -> String {
    if method == "ANY" {
        "x-amazon-apigateway-any-method".to_string()
    } else {
        method.to_lowercase()
    }
}

/// The REST API `Auth` block, resolved to swagger security definitions.
struct RestAuth {
    definitions: Map<String, Value>,
    default_authorizer: Option<String>,
    api_key_required: bool,
    /// Authorizer name -> its default scopes.
    scopes: Map<String, Value>,
    preflight_auth: bool,
}

impl RestAuth {
    fn parse(api_id: &str, auth: Option<&Value>) -> Result<(Self, Vec<(String, Value)>), String> {
        let mut out = RestAuth {
            definitions: Map::new(),
            default_authorizer: None,
            api_key_required: false,
            scopes: Map::new(),
            preflight_auth: true,
        };
        let mut extras = Vec::new();
        let Some(auth) = auth else {
            return Ok((out, extras));
        };
        let auth = auth
            .as_object()
            .ok_or_else(|| invalid(api_id, "Type of property 'Auth' is invalid."))?;
        out.default_authorizer = auth
            .get("DefaultAuthorizer")
            .and_then(Value::as_str)
            .map(str::to_string);
        out.api_key_required = auth
            .get("ApiKeyRequired")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        out.preflight_auth = auth
            .get("AddDefaultAuthorizerToCorsPreflight")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        for (name, cfg) in auth
            .get("Authorizers")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            let identity = cfg.get("Identity");
            let header = identity
                .and_then(|i| i.get("Header"))
                .and_then(Value::as_str)
                .unwrap_or("Authorization");
            let ttl = identity.and_then(|i| i.get("ReauthorizeEvery")).cloned();
            let definition = if let Some(pools) = cfg.get("UserPoolArn") {
                let providers = match pools {
                    Value::Array(a) => a.clone(),
                    other => vec![other.clone()],
                };
                json!({
                    "type": "apiKey",
                    "name": header,
                    "in": "header",
                    "x-amazon-apigateway-authtype": "cognito_user_pools",
                    "x-amazon-apigateway-authorizer": {
                        "type": "cognito_user_pools",
                        "providerARNs": providers,
                        "identitySource": format!("method.request.header.{header}"),
                    }
                })
            } else if let Some(function_arn) = cfg.get("FunctionArn") {
                let payload_type = cfg
                    .get("FunctionPayloadType")
                    .and_then(Value::as_str)
                    .unwrap_or("TOKEN");
                let mut authorizer = json!({ "authorizerUri": authorizer_uri(function_arn) });
                if let Some(role) = cfg.get("FunctionInvokeRole") {
                    authorizer["authorizerCredentials"] = role.clone();
                }
                if let Some(ttl) = &ttl {
                    authorizer["authorizerResultTtlInSeconds"] = ttl.clone();
                }
                let definition = if payload_type.eq_ignore_ascii_case("REQUEST") {
                    let mut sources = Vec::new();
                    for (key, prefix) in [
                        ("Headers", "method.request.header."),
                        ("QueryStrings", "method.request.querystring."),
                        ("StageVariables", "stageVariables."),
                        ("Context", "context."),
                    ] {
                        for name in string_list(identity.and_then(|i| i.get(key))) {
                            sources.push(format!("{prefix}{name}"));
                        }
                    }
                    authorizer["type"] = json!("request");
                    authorizer["identitySource"] = json!(sources.join(", "));
                    json!({
                        "type": "apiKey",
                        "name": "Unused",
                        "in": "header",
                        "x-amazon-apigateway-authtype": "custom",
                        "x-amazon-apigateway-authorizer": authorizer,
                    })
                } else {
                    authorizer["type"] = json!("token");
                    if let Some(expr) = identity.and_then(|i| i.get("ValidationExpression")) {
                        authorizer["identityValidationExpression"] = expr.clone();
                    }
                    json!({
                        "type": "apiKey",
                        "name": header,
                        "in": "header",
                        "x-amazon-apigateway-authtype": "custom",
                        "x-amazon-apigateway-authorizer": authorizer,
                    })
                };
                extras.push((
                    format!("{api_id}{}AuthorizerPermission", sanitize(name)),
                    authorizer_permission(api_id, function_arn),
                ));
                definition
            } else {
                return Err(invalid(
                    api_id,
                    &format!("Authorizer {name} must define either UserPoolArn or FunctionArn."),
                ));
            };
            if let Some(scopes) = cfg.get("AuthorizationScopes") {
                out.scopes.insert(name.clone(), scopes.clone());
            }
            out.definitions.insert(name.clone(), definition);
        }
        if let Some(default) = &out.default_authorizer {
            if default != "AWS_IAM" && !out.definitions.contains_key(default) {
                return Err(invalid(
                    api_id,
                    &format!("Unable to set DefaultAuthorizer because '{default}' was not defined in 'Authorizers'."),
                ));
            }
        }
        Ok((out, extras))
    }

    /// The swagger `security` for a method given the event's `Auth`.
    fn security_for(
        &mut self,
        api_id: &str,
        event_auth: Option<&Map<String, Value>>,
    ) -> Result<Option<Value>, String> {
        let authorizer = event_auth
            .and_then(|a| a.get("Authorizer"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.default_authorizer.clone());
        let mut security = Vec::new();
        match authorizer.as_deref() {
            None | Some("NONE") => {}
            Some("AWS_IAM") => {
                self.definitions.insert(
                    "sigv4".to_string(),
                    json!({
                        "type": "apiKey",
                        "name": "Authorization",
                        "in": "header",
                        "x-amazon-apigateway-authtype": "awsSigv4"
                    }),
                );
                security.push(json!({ "sigv4": [] }));
            }
            Some(name) => {
                if !self.definitions.contains_key(name) {
                    return Err(invalid(
                        api_id,
                        &format!("Unable to set Authorizer [{name}] on API method because the related API does not define it in 'Authorizers'."),
                    ));
                }
                let scopes = event_auth
                    .and_then(|a| a.get("AuthorizationScopes"))
                    .or_else(|| self.scopes.get(name))
                    .cloned()
                    .unwrap_or_else(|| json!([]));
                security.push(json!({ name: scopes }));
            }
        }
        let api_key = event_auth
            .and_then(|a| a.get("ApiKeyRequired"))
            .and_then(Value::as_bool)
            .unwrap_or(self.api_key_required);
        if api_key {
            self.definitions.insert(
                "api_key".to_string(),
                json!({ "type": "apiKey", "name": "x-api-key", "in": "header" }),
            );
            security.push(json!({ "api_key": [] }));
        }
        Ok((!security.is_empty()).then_some(Value::Array(security)))
    }
}

/// The REST API `Cors` setting.
struct RestCors {
    allow_origin: Value,
    allow_methods: Option<Value>,
    allow_headers: Option<Value>,
    max_age: Option<Value>,
    allow_credentials: bool,
}

impl RestCors {
    fn parse(api_id: &str, cors: Option<&Value>) -> Result<Option<Self>, String> {
        match cors {
            None => Ok(None),
            Some(Value::String(origin)) => Ok(Some(RestCors {
                allow_origin: json!(origin),
                allow_methods: None,
                allow_headers: None,
                max_age: None,
                allow_credentials: false,
            })),
            Some(Value::Object(cfg)) => Ok(Some(RestCors {
                allow_origin: cfg
                    .get("AllowOrigin")
                    .cloned()
                    .ok_or_else(|| invalid(api_id, "Cors must specify AllowOrigin."))?,
                allow_methods: cfg.get("AllowMethods").cloned(),
                allow_headers: cfg.get("AllowHeaders").cloned(),
                max_age: cfg.get("MaxAge").map(|v| match v {
                    Value::Number(n) => json!(format!("'{n}'")),
                    other => other.clone(),
                }),
                allow_credentials: cfg
                    .get("AllowCredentials")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })),
            Some(_) => Err(invalid(api_id, "Type of property 'Cors' is invalid.")),
        }
    }

    /// The preflight `OPTIONS` operation for a path exposing `methods`.
    fn options_op(&self, methods: &[String]) -> Value {
        let allow_methods = self.allow_methods.clone().unwrap_or_else(|| {
            let mut all: Vec<String> = if methods.iter().any(|m| m == "ANY") {
                ALL_METHODS.iter().map(|m| m.to_string()).collect()
            } else {
                let mut m = methods.to_vec();
                m.push("OPTIONS".to_string());
                m
            };
            all.sort();
            all.dedup();
            json!(format!("'{}'", all.join(",")))
        });
        let mut params = Map::new();
        let mut headers = Map::new();
        let mut add = |name: &str, value: Value| {
            params.insert(format!("method.response.header.{name}"), value);
            headers.insert(name.to_string(), json!({ "type": "string" }));
        };
        add("Access-Control-Allow-Origin", self.allow_origin.clone());
        add("Access-Control-Allow-Methods", allow_methods);
        if let Some(h) = &self.allow_headers {
            add("Access-Control-Allow-Headers", h.clone());
        }
        if let Some(m) = &self.max_age {
            add("Access-Control-Max-Age", m.clone());
        }
        if self.allow_credentials {
            add("Access-Control-Allow-Credentials", json!("'true'"));
        }
        json!({
            "summary": "CORS support",
            "consumes": ["application/json"],
            "produces": ["application/json"],
            "x-amazon-apigateway-integration": {
                "type": "mock",
                "requestTemplates": { "application/json": "{\n  \"statusCode\" : 200\n}\n" },
                "responses": {
                    "default": {
                        "statusCode": "200",
                        "responseParameters": params,
                        "responseTemplates": { "application/json": "{}\n" }
                    }
                }
            },
            "responses": {
                "200": { "description": "Default response for CORS method", "headers": headers }
            }
        })
    }
}

/// Stage-level SAM `Api` properties, moved onto the `AWS::ApiGateway::Stage`.
const REST_STAGE_PROPS: &[&str] = &[
    "Variables",
    "MethodSettings",
    "TracingEnabled",
    "CacheClusterEnabled",
    "CacheClusterSize",
    "AccessLogSetting",
    "CanarySetting",
];

/// SAM `Api` properties with no native `RestApi` counterpart that this
/// expansion does not build (custom domains, gateway responses, models,
/// usage plans) or that only steer the translator.
const REST_DROPPED_PROPS: &[&str] = &[
    "Domain",
    "GatewayResponses",
    "Models",
    "OpenApiVersion",
    "MergeDefinitions",
    "PropagateTags",
    "AlwaysDeploy",
];

/// Expand a REST API and the routes bound for it.
pub(super) fn build_rest_api(
    def: &ApiDef,
    routes: &[&ApiRoute],
    default_stage: Option<&str>,
) -> Result<ApiExpansion, String> {
    let id = def.logical_id.as_str();
    let mut props = def.props.clone();
    let mut out: Vec<(String, Value)> = Vec::new();

    let stage_name = match props.remove("StageName") {
        Some(v) => v,
        None => match default_stage {
            Some(s) => json!(s),
            None => return Err(invalid(id, "Missing required property 'StageName'.")),
        },
    };
    let definition_body = props.remove("DefinitionBody");
    let definition_uri = props.remove("DefinitionUri");
    let cors = RestCors::parse(id, props.remove("Cors").as_ref())?;
    let (mut auth, auth_extras) = RestAuth::parse(id, props.remove("Auth").as_ref())?;
    out.extend(auth_extras);
    let mut stage_props = Map::new();
    for key in REST_STAGE_PROPS {
        if let Some(v) = props.remove(*key) {
            stage_props.insert(key.to_string(), v);
        }
    }
    if let Some(Value::Object(tags)) = props.remove("Tags") {
        stage_props.insert(
            "Tags".to_string(),
            Value::Array(
                tags.into_iter()
                    .map(|(k, v)| json!({ "Key": k, "Value": v }))
                    .collect(),
            ),
        );
    }
    for key in REST_DROPPED_PROPS {
        props.remove(*key);
    }
    if let Some(ep) = props.remove("EndpointConfiguration") {
        let native = match &ep {
            Value::String(t) => json!({ "Types": [t] }),
            Value::Object(o) => {
                let mut native =
                    json!({ "Types": [o.get("Type").cloned().unwrap_or(json!("EDGE"))] });
                if let Some(ids) = o.get("VPCEndpointIds") {
                    native["VpcEndpointIds"] = ids.clone();
                }
                native
            }
            _ => ep.clone(),
        };
        props.insert("EndpointConfiguration".to_string(), native);
    }

    let definition_key = if let Some(uri) = definition_uri {
        // The definition lives in S3 and is imported as-is; events on the
        // API only add their invoke permissions (their path must be in it).
        let location = s3_location(&uri);
        props.insert("BodyS3Location".to_string(), location.clone());
        location
    } else {
        let mut body = definition_body.unwrap_or_else(|| {
            json!({
                "swagger": "2.0",
                "info": { "version": "1.0", "title": { "Ref": "AWS::StackName" } },
                "paths": {}
            })
        });
        let body_obj = body
            .as_object_mut()
            .ok_or_else(|| invalid(id, "DefinitionBody must be a map."))?;
        let mut paths = body_obj
            .remove("paths")
            .and_then(|p| p.as_object().cloned())
            .unwrap_or_default();
        for route in routes {
            let path = route.path.clone().unwrap_or_else(|| "/".to_string());
            let path_obj = paths
                .entry(path.clone())
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or_else(|| invalid(id, &format!("Path '{path}' must be a map.")))?;
            let key = op_key(&route.method);
            let op = path_obj
                .entry(key)
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or_else(|| invalid(id, &format!("Method on path '{path}' must be a map.")))?;
            if op.contains_key("x-amazon-apigateway-integration") {
                return Err(invalid(
                    id,
                    &format!(
                        "Event {} of function {} adds method {} on path {path}, which is already defined.",
                        route.event_name, route.target.function_id, route.method
                    ),
                ));
            }
            op.insert(
                "x-amazon-apigateway-integration".to_string(),
                json!({
                    "type": "aws_proxy",
                    "httpMethod": "POST",
                    "uri": lambda_integration_uri(&route.target),
                }),
            );
            op.entry("responses").or_insert_with(|| json!({}));
            if let Some(security) = auth.security_for(id, route.auth.as_ref())? {
                op.insert("security".to_string(), security);
            }
        }
        if let Some(cors) = &cors {
            let preflight_security = if auth.preflight_auth {
                let default = auth.default_authorizer.clone();
                match default.as_deref() {
                    Some(name) if name != "NONE" => {
                        let mut a = Map::new();
                        a.insert("Authorizer".to_string(), json!(name));
                        a.insert("ApiKeyRequired".to_string(), json!(false));
                        auth.security_for(id, Some(&a))?
                    }
                    _ => None,
                }
            } else {
                None
            };
            for path_item in paths.values_mut() {
                let Some(path_obj) = path_item.as_object_mut() else {
                    continue;
                };
                if path_obj.contains_key("options") {
                    continue;
                }
                let methods: Vec<String> = path_obj
                    .keys()
                    .filter_map(|k| match k.as_str() {
                        "x-amazon-apigateway-any-method" => Some("ANY".to_string()),
                        "get" | "put" | "post" | "delete" | "head" | "patch" => {
                            Some(k.to_uppercase())
                        }
                        _ => None,
                    })
                    .collect();
                let mut op = cors.options_op(&methods);
                if let Some(sec) = &preflight_security {
                    op["security"] = sec.clone();
                }
                path_obj.insert("options".to_string(), op);
            }
        }
        body_obj.insert("paths".to_string(), Value::Object(paths));
        if !auth.definitions.is_empty() {
            let defs = body_obj
                .entry("securityDefinitions")
                .or_insert_with(|| json!({}));
            if let Some(defs) = defs.as_object_mut() {
                for (k, v) in &auth.definitions {
                    defs.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
        }
        props.insert("Body".to_string(), body.clone());
        body
    };

    let mut api = Map::new();
    api.insert("Type".to_string(), json!("AWS::ApiGateway::RestApi"));
    api.insert("Properties".to_string(), Value::Object(props));
    for (k, v) in &def.attributes {
        api.insert(k.clone(), v.clone());
    }
    out.push((id.to_string(), Value::Object(api)));

    let hash = hash10(&json!([definition_key, stage_name]));
    let deployment_id = format!("{id}Deployment{hash}");
    out.push((
        deployment_id.clone(),
        json!({
            "Type": "AWS::ApiGateway::Deployment",
            "Properties": {
                "RestApiId": { "Ref": id },
                "Description": format!("RestApi deployment id: {hash}"),
            }
        }),
    ));
    let stage_id = format!(
        "{id}{}Stage",
        stage_name.as_str().map(sanitize).unwrap_or_default()
    );
    stage_props.insert("RestApiId".to_string(), json!({ "Ref": id }));
    stage_props.insert("DeploymentId".to_string(), json!({ "Ref": deployment_id }));
    stage_props.insert("StageName".to_string(), stage_name);
    out.push((
        stage_id.clone(),
        json!({ "Type": "AWS::ApiGateway::Stage", "Properties": stage_props }),
    ));
    for route in routes {
        out.push((
            format!("{}Permission", route.id_base),
            route_permission(id, route),
        ));
    }
    propagate_condition(def, &mut out);
    let refs = vec![
        (format!("{id}.Stage"), stage_id),
        (format!("{id}.Deployment"), deployment_id),
    ];
    Ok((out, refs))
}

/// SAM `HttpApi` properties with no native `ApiGatewayV2::Api` counterpart
/// that this expansion does not build (custom domains) or that it moves onto
/// the stage.
const HTTP_DROPPED_PROPS: &[&str] = &["Domain", "PropagateTags"];

/// Expand an HTTP API and the routes bound for it.
pub(super) fn build_http_api(def: &ApiDef, routes: &[&ApiRoute]) -> Result<ApiExpansion, String> {
    let id = def.logical_id.as_str();
    let mut props = def.props.clone();
    let mut out: Vec<(String, Value)> = Vec::new();

    let stage_name = props.remove("StageName");
    let mut stage_props = Map::new();
    for (sam_key, native_key) in [
        ("StageVariables", "StageVariables"),
        ("AccessLogSettings", "AccessLogSettings"),
        ("DefaultRouteSettings", "DefaultRouteSettings"),
        ("RouteSettings", "RouteSettings"),
    ] {
        if let Some(v) = props.remove(sam_key) {
            stage_props.insert(native_key.to_string(), v);
        }
    }
    for key in HTTP_DROPPED_PROPS {
        props.remove(*key);
    }
    let auth = props.remove("Auth");
    if let Some(body) = props.remove("DefinitionBody") {
        props.insert("Body".to_string(), body);
    }
    if let Some(uri) = props.remove("DefinitionUri") {
        props.insert("BodyS3Location".to_string(), s3_location(&uri));
    }
    match props.remove("CorsConfiguration") {
        Some(Value::Bool(true)) => {
            props.insert(
                "CorsConfiguration".to_string(),
                json!({ "AllowOrigins": ["*"], "AllowHeaders": ["*"], "AllowMethods": ["*"] }),
            );
        }
        Some(Value::Object(cfg)) => {
            props.insert("CorsConfiguration".to_string(), Value::Object(cfg));
        }
        Some(Value::Bool(false)) | None => {}
        Some(_) => {
            return Err(invalid(
                id,
                "Type of property 'CorsConfiguration' is invalid.",
            ))
        }
    }
    if !props.contains_key("Name")
        && !props.contains_key("Body")
        && !props.contains_key("BodyS3Location")
    {
        props.insert("Name".to_string(), json!({ "Ref": "AWS::StackName" }));
    }
    props.insert("ProtocolType".to_string(), json!("HTTP"));

    let mut api = Map::new();
    api.insert("Type".to_string(), json!("AWS::ApiGatewayV2::Api"));
    api.insert("Properties".to_string(), Value::Object(props));
    for (k, v) in &def.attributes {
        api.insert(k.clone(), v.clone());
    }
    out.push((id.to_string(), Value::Object(api)));
    let api_index = out.len() - 1;

    // --- Authorizers ---
    let auth = auth
        .and_then(|a| a.as_object().cloned())
        .unwrap_or_default();
    let default_authorizer = auth
        .get("DefaultAuthorizer")
        .and_then(Value::as_str)
        .map(str::to_string);
    let iam_enabled = auth
        .get("EnableIamAuthorizer")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // name -> (authorizer logical id, authorization type, default scopes)
    let mut authorizers: Map<String, Value> = Map::new();
    for (name, cfg) in auth
        .get("Authorizers")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let auth_id = format!("{id}{}Authorizer", sanitize(name));
        let mut auth_props = json!({ "ApiId": { "Ref": id }, "Name": name });
        let auth_type = if let Some(jwt) = cfg.get("JwtConfiguration") {
            // SAM spells these keys in lower case; accept the native case too.
            let mut jwt_cfg = Map::new();
            for (sam_key, native_key) in [("issuer", "Issuer"), ("audience", "Audience")] {
                if let Some(v) = jwt.get(sam_key).or_else(|| jwt.get(native_key)) {
                    jwt_cfg.insert(native_key.to_string(), v.clone());
                }
            }
            auth_props["AuthorizerType"] = json!("JWT");
            auth_props["JwtConfiguration"] = Value::Object(jwt_cfg);
            let mut sources = string_list(cfg.get("IdentitySource"));
            if sources.is_empty() {
                sources.push("$request.header.Authorization".to_string());
            }
            auth_props["IdentitySource"] = json!(sources);
            "JWT"
        } else if let Some(function_arn) = cfg.get("FunctionArn") {
            let identity = cfg.get("Identity");
            let mut sources = Vec::new();
            for (key, prefix) in [
                ("Headers", "$request.header."),
                ("QueryStrings", "$request.querystring."),
                ("StageVariables", "$stageVariables."),
                ("Context", "$context."),
            ] {
                for name in string_list(identity.and_then(|i| i.get(key))) {
                    sources.push(format!("{prefix}{name}"));
                }
            }
            auth_props["AuthorizerType"] = json!("REQUEST");
            auth_props["AuthorizerUri"] = authorizer_uri(function_arn);
            auth_props["IdentitySource"] = json!(sources);
            auth_props["AuthorizerPayloadFormatVersion"] = cfg
                .get("AuthorizerPayloadFormatVersion")
                .cloned()
                .ok_or_else(|| {
                    invalid(id, &format!("Property 'AuthorizerPayloadFormatVersion' is required for Lambda authorizer {name}."))
                })?;
            if let Some(v) = cfg.get("EnableSimpleResponses") {
                auth_props["EnableSimpleResponses"] = v.clone();
            }
            if let Some(ttl) = identity.and_then(|i| i.get("ReauthorizeEvery")) {
                auth_props["AuthorizerResultTtlInSeconds"] = ttl.clone();
            }
            if let Some(role) = cfg.get("FunctionInvokeRole") {
                auth_props["AuthorizerCredentialsArn"] = role.clone();
            }
            out.push((
                format!("{id}{}AuthorizerPermission", sanitize(name)),
                authorizer_permission(id, function_arn),
            ));
            "CUSTOM"
        } else {
            return Err(invalid(
                id,
                &format!("Authorizer {name} must define either JwtConfiguration or FunctionArn."),
            ));
        };
        out.push((
            auth_id.clone(),
            json!({ "Type": "AWS::ApiGatewayV2::Authorizer", "Properties": auth_props }),
        ));
        authorizers.insert(
            name.clone(),
            json!([
                auth_id,
                auth_type,
                cfg.get("AuthorizationScopes")
                    .cloned()
                    .unwrap_or(Value::Null)
            ]),
        );
    }
    if let Some(default) = &default_authorizer {
        if default != "AWS_IAM" && !authorizers.contains_key(default) {
            return Err(invalid(
                id,
                &format!("Unable to set DefaultAuthorizer because '{default}' was not defined in 'Authorizers'."),
            ));
        }
    }

    // --- Stage ---
    let (stage_id, stage_name_value) = match &stage_name {
        None => (format!("{id}ApiGatewayDefaultStage"), json!("$default")),
        Some(v) => (
            format!("{id}{}Stage", v.as_str().map(sanitize).unwrap_or_default()),
            v.clone(),
        ),
    };
    stage_props.insert("ApiId".to_string(), json!({ "Ref": id }));
    stage_props.insert("StageName".to_string(), stage_name_value);
    stage_props.insert("AutoDeploy".to_string(), json!(true));
    out.push((
        stage_id.clone(),
        json!({ "Type": "AWS::ApiGatewayV2::Stage", "Properties": stage_props }),
    ));

    // --- Routes ---
    for route in routes {
        let authorizer = route
            .auth
            .as_ref()
            .and_then(|a| a.get("Authorizer"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| default_authorizer.clone());
        // With a DefinitionBody, SAM adds the event's route to the definition
        // itself, so it is part of what the API imports (and re-imports on an
        // update) rather than a separate route the import would replace. A
        // route needing an authorizer stays a separate resource: the
        // authorizer is created after the API it belongs to.
        if let (Some(path), None | Some("NONE")) = (&route.path, authorizer.as_deref()) {
            if let Some(body) = out[api_index]
                .1
                .pointer_mut("/Properties/Body")
                .and_then(Value::as_object_mut)
            {
                let paths = body
                    .entry("paths")
                    .or_insert_with(|| json!({}))
                    .as_object_mut()
                    .ok_or_else(|| invalid(id, "DefinitionBody paths must be a map."))?;
                let path_obj = paths
                    .entry(path.clone())
                    .or_insert_with(|| json!({}))
                    .as_object_mut()
                    .ok_or_else(|| invalid(id, &format!("Path '{path}' must be a map.")))?;
                let op = path_obj
                    .entry(op_key(&route.method))
                    .or_insert_with(|| json!({}))
                    .as_object_mut()
                    .ok_or_else(|| {
                        invalid(id, &format!("Method on path '{path}' must be a map."))
                    })?;
                if op.contains_key("x-amazon-apigateway-integration") {
                    return Err(invalid(
                        id,
                        &format!(
                            "Event {} of function {} adds method {} on path {path}, which is already defined.",
                            route.event_name, route.target.function_id, route.method
                        ),
                    ));
                }
                let mut integration = json!({
                    "type": "aws_proxy",
                    "httpMethod": "POST",
                    "uri": route.target.arn.clone(),
                    "payloadFormatVersion": route.payload_format_version.clone().unwrap_or(json!("2.0")),
                });
                if let Some(t) = &route.timeout_in_millis {
                    integration["timeoutInMillis"] = t.clone();
                }
                op.insert("x-amazon-apigateway-integration".to_string(), integration);
                out.push((
                    format!("{}Permission", route.id_base),
                    route_permission(id, route),
                ));
                continue;
            }
        }
        let integ_id = format!("{}Integration", route.id_base);
        let mut integ = json!({
            "ApiId": { "Ref": id },
            "IntegrationType": "AWS_PROXY",
            "IntegrationUri": route.target.arn.clone(),
            "PayloadFormatVersion": route.payload_format_version.clone().unwrap_or(json!("2.0")),
        });
        if let Some(t) = &route.timeout_in_millis {
            integ["TimeoutInMillis"] = t.clone();
        }
        out.push((
            integ_id.clone(),
            json!({ "Type": "AWS::ApiGatewayV2::Integration", "Properties": integ }),
        ));
        let route_key = match &route.path {
            None => "$default".to_string(),
            Some(path) => format!("{} {path}", route.method),
        };
        let mut route_props = json!({
            "ApiId": { "Ref": id },
            "RouteKey": route_key,
            "Target": { "Fn::Sub": format!("integrations/${{{integ_id}}}") },
        });
        match authorizer.as_deref() {
            None | Some("NONE") => {}
            Some("AWS_IAM") => {
                if !iam_enabled && default_authorizer.as_deref() != Some("AWS_IAM") {
                    return Err(invalid(
                        id,
                        "Unable to set Authorizer AWS_IAM on a route: the API must set Auth.EnableIamAuthorizer.",
                    ));
                }
                route_props["AuthorizationType"] = json!("AWS_IAM");
            }
            Some(name) => {
                let entry = authorizers.get(name).ok_or_else(|| {
                    invalid(
                        id,
                        &format!("Unable to set Authorizer [{name}] on a route because the related API does not define it in 'Authorizers'."),
                    )
                })?;
                route_props["AuthorizationType"] = entry[1].clone();
                route_props["AuthorizerId"] = json!({ "Ref": entry[0] });
                let scopes = route
                    .auth
                    .as_ref()
                    .and_then(|a| a.get("AuthorizationScopes"))
                    .cloned()
                    .or_else(|| (!entry[2].is_null()).then(|| entry[2].clone()));
                if let Some(scopes) = scopes {
                    route_props["AuthorizationScopes"] = scopes;
                }
            }
        }
        out.push((
            format!("{}Route", route.id_base),
            json!({ "Type": "AWS::ApiGatewayV2::Route", "Properties": route_props }),
        ));
        out.push((
            format!("{}Permission", route.id_base),
            route_permission(id, route),
        ));
    }
    propagate_condition(def, &mut out);
    Ok((out, vec![(format!("{id}.Stage"), stage_id)]))
}

/// SAM puts an API's `Condition` on every resource it generates for it
/// (deployment, stage, authorizers, routes, integrations, permissions), so a
/// condition that drops the API drops all of them.
fn propagate_condition(def: &ApiDef, out: &mut [(String, Value)]) {
    let Some(condition) = def.attributes.get("Condition") else {
        return;
    };
    for (logical_id, resource) in out.iter_mut() {
        if logical_id == &def.logical_id {
            continue;
        }
        if let Some(obj) = resource.as_object_mut() {
            obj.entry("Condition").or_insert_with(|| condition.clone());
        }
    }
}
