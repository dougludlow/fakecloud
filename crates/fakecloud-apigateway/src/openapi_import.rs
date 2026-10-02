//! OpenAPI 3 / Swagger 2 import for REST APIs.
//!
//! Turns a definition (with API Gateway's `x-amazon-apigateway-*`
//! extensions) into the resources, methods, integrations, method and
//! integration responses, authorizers and request validators of a REST API,
//! the way `ImportRestApi` / `PutRestApi` and CloudFormation's
//! `AWS::ApiGateway::RestApi` `Body` / `BodyS3Location` do. The result lands
//! in the same state the per-resource APIs (`PutMethod`,
//! `PutIntegration`, ...) write, so the data plane serves an imported API
//! exactly like one built call by call.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::state::{make_id, ApiGatewayState, Authorizer, Integration, Method, Resource};

/// How an import combines with what the API already has, as `PutRestApi`'s
/// `mode` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    /// Replace the API's resources, methods and authorizers with the
    /// definition's.
    Overwrite,
    /// Add the definition's resources and methods to the existing ones; a
    /// method the definition also declares is replaced.
    Merge,
}

/// Operation keys under `paths.<path>`, with the API Gateway method each
/// maps to.
const OPERATIONS: &[(&str, &str)] = &[
    ("get", "GET"),
    ("put", "PUT"),
    ("post", "POST"),
    ("delete", "DELETE"),
    ("options", "OPTIONS"),
    ("head", "HEAD"),
    ("patch", "PATCH"),
    ("x-amazon-apigateway-any-method", "ANY"),
];

/// Parse a definition document given as text: JSON, else YAML.
pub fn parse_openapi_document(raw: &str) -> Result<Value, String> {
    if let Ok(v) = serde_json::from_str::<Value>(raw) {
        return Ok(v);
    }
    serde_yaml::from_str::<Value>(raw).map_err(|e| format!("Invalid OpenAPI input: {e}"))
}

/// Whether `spec` is an OpenAPI 3 / Swagger 2 definition.
pub fn is_openapi_document(spec: &Value) -> bool {
    spec.is_object()
        && (spec.get("swagger").is_some()
            || spec.get("openapi").is_some()
            || spec.get("paths").is_some_and(Value::is_object))
}

fn str_of(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_string)
}

fn string_map(v: Option<&Value>) -> BTreeMap<String, String> {
    v.and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// The security schemes of either spec flavour.
fn security_schemes(spec: &Value) -> Map<String, Value> {
    spec.get("securityDefinitions")
        .or_else(|| spec.pointer("/components/securitySchemes"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// Import `spec` into the REST API `api_id`, which must already exist in
/// `state`. Updates the API's name/description/version and other top-level
/// settings from the definition.
pub fn import_openapi(
    state: &mut ApiGatewayState,
    api_id: &str,
    spec: &Value,
    mode: ImportMode,
) -> Result<(), String> {
    if !is_openapi_document(spec) {
        return Err(
            "Invalid OpenAPI input: the definition has no swagger/openapi version or paths"
                .to_string(),
        );
    }
    let root_id = state
        .apis
        .get(api_id)
        .map(|a| a.root_resource_id.clone())
        .ok_or_else(|| format!("RestApi {api_id} not found"))?;

    // --- Top-level API settings ---
    if let Some(api) = state.apis.get_mut(api_id) {
        if let Some(title) = spec.pointer("/info/title").and_then(Value::as_str) {
            api.name = title.to_string();
        }
        if let Some(d) = spec.pointer("/info/description").and_then(Value::as_str) {
            api.description = Some(d.to_string());
        }
        if let Some(v) = spec.pointer("/info/version").and_then(Value::as_str) {
            api.version = Some(v.to_string());
        }
        if let Some(types) = spec
            .get("x-amazon-apigateway-binary-media-types")
            .and_then(Value::as_array)
        {
            api.binary_media_types = types
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
        }
        if let Some(src) = spec
            .get("x-amazon-apigateway-api-key-source")
            .and_then(Value::as_str)
        {
            api.api_key_source = src.to_string();
        }
        if let Some(policy) = spec.get("x-amazon-apigateway-policy") {
            api.policy = Some(match policy {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            });
        }
        if let Some(size) = spec
            .get("x-amazon-apigateway-minimum-compression-size")
            .and_then(Value::as_i64)
        {
            api.minimum_compression_size = Some(size);
        }
        if let Some(disable) = spec
            .pointer("/x-amazon-apigateway-endpoint-configuration/disableExecuteApiEndpoint")
            .and_then(Value::as_bool)
        {
            api.disable_execute_api_endpoint = disable;
        }
    }

    let prefix = format!("{api_id}/");
    if mode == ImportMode::Overwrite {
        if let Some(resources) = state.resources.get_mut(api_id) {
            resources.retain(|id, _| *id == root_id);
        }
        state.methods.retain(|k, _| !k.starts_with(&prefix));
        state.integrations.retain(|k, _| !k.starts_with(&prefix));
        state
            .method_responses
            .retain(|k, _| !k.starts_with(&prefix));
        state
            .integration_responses
            .retain(|k, _| !k.starts_with(&prefix));
        state.authorizers.remove(api_id);
        state.request_validators.remove(api_id);
    }

    // --- Authorizers + API-key scheme names ---
    let mut authorizer_ids: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut api_key_schemes: Vec<String> = Vec::new();
    for (scheme_name, scheme) in security_schemes(spec) {
        if let Some(cfg) = scheme.get("x-amazon-apigateway-authorizer") {
            let authorizer_type = cfg
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("token")
                .to_uppercase();
            let identity_source = str_of(cfg.get("identitySource")).or_else(|| {
                // A TOKEN authorizer reads the header the scheme names.
                (authorizer_type == "TOKEN"
                    && scheme.get("in").and_then(Value::as_str) == Some("header"))
                .then(|| {
                    scheme
                        .get("name")
                        .and_then(Value::as_str)
                        .map(|n| format!("method.request.header.{n}"))
                })
                .flatten()
            });
            let existing = state
                .authorizers
                .get(api_id)
                .and_then(|m| m.values().find(|a| a.name == scheme_name))
                .map(|a| a.id.clone());
            let id = existing.unwrap_or_else(make_id);
            let authorizer = Authorizer {
                id: id.clone(),
                name: scheme_name.clone(),
                authorizer_type: authorizer_type.clone(),
                provider_arns: cfg
                    .get("providerARNs")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                auth_type: str_of(scheme.get("x-amazon-apigateway-authtype")),
                authorizer_uri: str_of(cfg.get("authorizerUri")),
                authorizer_credentials: str_of(cfg.get("authorizerCredentials")),
                identity_source,
                identity_validation_expression: str_of(cfg.get("identityValidationExpression")),
                authorizer_result_ttl_in_seconds: cfg
                    .get("authorizerResultTtlInSeconds")
                    .and_then(Value::as_i64)
                    .map(|n| n as i32),
            };
            state
                .authorizers
                .entry(api_id.to_string())
                .or_default()
                .insert(id.clone(), authorizer);
            authorizer_ids.insert(scheme_name, (id, authorizer_type));
        } else if scheme.get("type").and_then(Value::as_str) == Some("apiKey") {
            api_key_schemes.push(scheme_name);
        }
    }

    // --- Request validators ---
    let mut validator_ids: BTreeMap<String, String> = BTreeMap::new();
    if let Some(validators) = spec
        .get("x-amazon-apigateway-request-validators")
        .and_then(Value::as_object)
    {
        for (name, cfg) in validators {
            let id = make_id();
            state
                .request_validators
                .entry(api_id.to_string())
                .or_default()
                .insert(
                    id.clone(),
                    json!({
                        "id": id,
                        "name": name,
                        "validateRequestBody": cfg.get("validateRequestBody").and_then(Value::as_bool).unwrap_or(false),
                        "validateRequestParameters": cfg.get("validateRequestParameters").and_then(Value::as_bool).unwrap_or(false),
                    }),
                );
            validator_ids.insert(name.clone(), id);
        }
    }
    let default_validator = spec
        .get("x-amazon-apigateway-request-validator")
        .and_then(Value::as_str)
        .and_then(|n| validator_ids.get(n).cloned());

    let global_security = spec.get("security").cloned();

    // --- Paths ---
    let paths = spec
        .get("paths")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (path, path_item) in &paths {
        let Some(path_item) = path_item.as_object() else {
            continue;
        };
        let resource_id = ensure_resource(state, api_id, &root_id, path);
        let path_params = path_item.get("parameters");
        for (op_key, http_method) in OPERATIONS {
            let Some(op) = path_item.get(*op_key).and_then(Value::as_object) else {
                continue;
            };
            let key = format!("{api_id}/{resource_id}/{http_method}");

            // Authorization from the operation's (else the global) security.
            let mut authorization_type = "NONE".to_string();
            let mut authorizer_id = None;
            let mut scopes: Vec<String> = Vec::new();
            let mut api_key_required = false;
            let security = op.get("security").or(global_security.as_ref());
            for requirement in security.and_then(Value::as_array).into_iter().flatten() {
                let Some(req) = requirement.as_object() else {
                    continue;
                };
                for (scheme_name, req_scopes) in req {
                    if let Some((id, ty)) = authorizer_ids.get(scheme_name).cloned().or_else(|| {
                        state.authorizers.get(api_id).and_then(|m| {
                            m.values()
                                .find(|a| &a.name == scheme_name)
                                .map(|a| (a.id.clone(), a.authorizer_type.clone()))
                        })
                    }) {
                        authorization_type = if ty == "COGNITO_USER_POOLS" {
                            "COGNITO_USER_POOLS".to_string()
                        } else {
                            "CUSTOM".to_string()
                        };
                        authorizer_id = Some(id);
                        scopes = req_scopes
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default();
                    } else if api_key_schemes.contains(scheme_name) {
                        api_key_required = true;
                    } else if scheme_name == "sigv4" {
                        authorization_type = "AWS_IAM".to_string();
                    }
                }
            }
            if let Some(auth) = op
                .get("x-amazon-apigateway-auth")
                .and_then(|a| a.get("type"))
                .and_then(Value::as_str)
            {
                if auth.eq_ignore_ascii_case("AWS_IAM") {
                    authorization_type = "AWS_IAM".to_string();
                }
            }

            let mut request_parameters = BTreeMap::new();
            for param in path_params
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .chain(
                    op.get("parameters")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten(),
                )
            {
                let (Some(name), Some(location)) = (
                    param.get("name").and_then(Value::as_str),
                    param.get("in").and_then(Value::as_str),
                ) else {
                    continue;
                };
                let location = match location {
                    "path" => "path",
                    "query" => "querystring",
                    "header" => "header",
                    _ => continue,
                };
                let required = param
                    .get("required")
                    .and_then(Value::as_bool)
                    .unwrap_or(location == "path");
                request_parameters.insert(format!("method.request.{location}.{name}"), required);
            }

            let request_validator_id = op
                .get("x-amazon-apigateway-request-validator")
                .and_then(Value::as_str)
                .and_then(|n| validator_ids.get(n).cloned())
                .or_else(|| default_validator.clone());

            state.methods.insert(
                key.clone(),
                Method {
                    rest_api_id: api_id.to_string(),
                    resource_id: resource_id.clone(),
                    http_method: http_method.to_string(),
                    authorization_type,
                    authorizer_id,
                    api_key_required,
                    operation_name: str_of(op.get("operationId")),
                    request_parameters,
                    request_models: BTreeMap::new(),
                    request_validator_id,
                    authorization_scopes: scopes,
                },
            );
            // A re-imported method drops its old responses and integration.
            let response_prefix = format!("{key}/");
            state.integrations.remove(&key);
            state
                .method_responses
                .retain(|k, _| !k.starts_with(&response_prefix));
            state
                .integration_responses
                .retain(|k, _| !k.starts_with(&response_prefix));

            // Method responses.
            for (status, response) in op
                .get("responses")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
            {
                if status == "default" {
                    continue;
                }
                let response_parameters: Map<String, Value> = response
                    .get("headers")
                    .and_then(Value::as_object)
                    .map(|h| {
                        h.keys()
                            .map(|name| (format!("method.response.header.{name}"), json!(false)))
                            .collect()
                    })
                    .unwrap_or_default();
                state.method_responses.insert(
                    format!("{key}/{status}"),
                    json!({
                        "statusCode": status,
                        "responseParameters": response_parameters,
                        "responseModels": {},
                    }),
                );
            }

            // Integration.
            if let Some(integ) = op
                .get("x-amazon-apigateway-integration")
                .and_then(Value::as_object)
            {
                state.integrations.insert(
                    key.clone(),
                    Integration {
                        rest_api_id: api_id.to_string(),
                        resource_id: resource_id.clone(),
                        http_method: http_method.to_string(),
                        integration_type: integ
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("MOCK")
                            .to_uppercase(),
                        integration_http_method: integ
                            .get("httpMethod")
                            .and_then(Value::as_str)
                            .map(str::to_uppercase),
                        uri: str_of(integ.get("uri")),
                        credentials: str_of(integ.get("credentials")),
                        request_parameters: string_map(integ.get("requestParameters")),
                        request_templates: string_map(integ.get("requestTemplates")),
                        passthrough_behavior: integ
                            .get("passthroughBehavior")
                            .and_then(Value::as_str)
                            .unwrap_or("WHEN_NO_MATCH")
                            .to_uppercase(),
                        timeout_in_millis: integ
                            .get("timeoutInMillis")
                            .and_then(Value::as_i64)
                            .map(|n| n as i32),
                        cache_namespace: str_of(integ.get("cacheNamespace")),
                        cache_key_parameters: integ
                            .get("cacheKeyParameters")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        content_handling: str_of(integ.get("contentHandling")),
                        connection_type: str_of(integ.get("connectionType")),
                        connection_id: str_of(integ.get("connectionId")),
                        tls_config: integ.get("tlsConfig").cloned(),
                    },
                );
                for (pattern, response) in integ
                    .get("responses")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                {
                    let Some(status) = response.get("statusCode").and_then(|s| {
                        s.as_str()
                            .map(str::to_string)
                            .or_else(|| s.as_i64().map(|n| n.to_string()))
                    }) else {
                        continue;
                    };
                    let mut record = json!({
                        "statusCode": status,
                        "responseParameters": response.get("responseParameters").cloned().unwrap_or_else(|| json!({})),
                        "responseTemplates": response.get("responseTemplates").cloned().unwrap_or_else(|| json!({})),
                    });
                    if pattern != "default" {
                        record["selectionPattern"] = json!(pattern);
                    }
                    if let Some(ch) = response.get("contentHandling") {
                        record["contentHandling"] = ch.clone();
                    }
                    state
                        .integration_responses
                        .insert(format!("{key}/{status}"), record);
                }
            }
        }
    }
    Ok(())
}

/// Resolve the resource for `path` under the API, creating each missing
/// segment, and return its id.
fn ensure_resource(state: &mut ApiGatewayState, api_id: &str, root_id: &str, path: &str) -> String {
    let resources = state.resources.entry(api_id.to_string()).or_default();
    let mut parent_id = root_id.to_string();
    let mut full = String::new();
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        full.push('/');
        full.push_str(segment);
        let existing = resources
            .values()
            .find(|r| r.path == full)
            .map(|r| r.id.clone());
        parent_id = match existing {
            Some(id) => id,
            None => {
                let id = make_id();
                resources.insert(
                    id.clone(),
                    Resource {
                        id: id.clone(),
                        parent_id: Some(parent_id),
                        path_part: Some(segment.to_string()),
                        path: full.clone(),
                    },
                );
                id
            }
        };
    }
    parent_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::RestApi;

    fn state_with_api() -> (ApiGatewayState, String, String) {
        let mut state = ApiGatewayState::new("123456789012", "us-east-1");
        let id = "api1".to_string();
        let root = "root1".to_string();
        state.apis.insert(
            id.clone(),
            RestApi {
                id: id.clone(),
                name: "orig".into(),
                description: None,
                version: None,
                created_date: chrono::Utc::now(),
                api_key_source: "HEADER".into(),
                endpoint_configuration: json!({"types": ["EDGE"]}),
                policy: None,
                binary_media_types: vec![],
                minimum_compression_size: None,
                disable_execute_api_endpoint: false,
                root_resource_id: root.clone(),
                tags: BTreeMap::new(),
                import_source: None,
            },
        );
        let mut res = BTreeMap::new();
        res.insert(
            root.clone(),
            Resource {
                id: root.clone(),
                parent_id: None,
                path_part: None,
                path: "/".into(),
            },
        );
        state.resources.insert(id.clone(), res);
        (state, id, root)
    }

    fn spec() -> Value {
        json!({
            "swagger": "2.0",
            "info": {"title": "Pets", "version": "1.0"},
            "securityDefinitions": {
                "MyAuth": {
                    "type": "apiKey", "name": "Authorization", "in": "header",
                    "x-amazon-apigateway-authorizer": {
                        "type": "token",
                        "authorizerUri": "arn:aws:apigateway:us-east-1:lambda:path/2015-03-31/functions/arn:aws:lambda:us-east-1:123456789012:function:auth/invocations"
                    }
                },
                "api_key": {"type": "apiKey", "name": "x-api-key", "in": "header"}
            },
            "paths": {
                "/pets/{id}": {
                    "get": {
                        "operationId": "getPet",
                        "parameters": [{"name": "id", "in": "path", "required": true}],
                        "security": [{"MyAuth": []}, {"api_key": []}],
                        "responses": {"200": {"description": "ok"}},
                        "x-amazon-apigateway-integration": {
                            "type": "aws_proxy",
                            "httpMethod": "POST",
                            "uri": "arn:aws:apigateway:us-east-1:lambda:path/2015-03-31/functions/arn:aws:lambda:us-east-1:123456789012:function:pets/invocations"
                        }
                    },
                    "options": {
                        "x-amazon-apigateway-integration": {
                            "type": "mock",
                            "requestTemplates": {"application/json": "{\"statusCode\": 200}"},
                            "responses": {"default": {"statusCode": "200", "responseParameters": {"method.response.header.Access-Control-Allow-Origin": "'*'"}}}
                        }
                    }
                },
                "/": {"x-amazon-apigateway-any-method": {"x-amazon-apigateway-integration": {"type": "mock"}}}
            }
        })
    }

    #[test]
    fn import_builds_resources_methods_integrations_and_authorizers() {
        let (mut state, id, root) = state_with_api();
        import_openapi(&mut state, &id, &spec(), ImportMode::Overwrite).unwrap();
        assert_eq!(state.apis[&id].name, "Pets");
        let resources = &state.resources[&id];
        let pets = resources.values().find(|r| r.path == "/pets").unwrap();
        let pet = resources.values().find(|r| r.path == "/pets/{id}").unwrap();
        assert_eq!(pet.parent_id.as_deref(), Some(pets.id.as_str()));

        let get = &state.methods[&format!("{id}/{}/GET", pet.id)];
        assert_eq!(get.authorization_type, "CUSTOM");
        assert!(get.api_key_required);
        assert_eq!(get.operation_name.as_deref(), Some("getPet"));
        assert!(get.request_parameters["method.request.path.id"]);
        let auth = &state.authorizers[&id][get.authorizer_id.as_ref().unwrap()];
        assert_eq!(auth.authorizer_type, "TOKEN");
        assert_eq!(
            auth.identity_source.as_deref(),
            Some("method.request.header.Authorization")
        );

        let integ = &state.integrations[&format!("{id}/{}/GET", pet.id)];
        assert_eq!(integ.integration_type, "AWS_PROXY");
        assert_eq!(integ.integration_http_method.as_deref(), Some("POST"));
        assert!(state
            .method_responses
            .contains_key(&format!("{id}/{}/GET/200", pet.id)));
        let options_resp = &state.integration_responses[&format!("{id}/{}/OPTIONS/200", pet.id)];
        assert_eq!(
            options_resp["responseParameters"]
                ["method.response.header.Access-Control-Allow-Origin"],
            "'*'"
        );
        assert!(options_resp.get("selectionPattern").is_none());
        assert!(state.methods.contains_key(&format!("{id}/{root}/ANY")));
    }

    #[test]
    fn overwrite_replaces_and_merge_keeps() {
        let (mut state, id, _) = state_with_api();
        import_openapi(&mut state, &id, &spec(), ImportMode::Overwrite).unwrap();
        let other = json!({"openapi": "3.0.1", "paths": {"/other": {"get": {}}}});
        import_openapi(&mut state, &id, &other, ImportMode::Merge).unwrap();
        assert!(state.resources[&id]
            .values()
            .any(|r| r.path == "/pets/{id}"));
        assert!(state.resources[&id].values().any(|r| r.path == "/other"));
        import_openapi(&mut state, &id, &other, ImportMode::Overwrite).unwrap();
        assert!(!state.resources[&id].values().any(|r| r.path == "/pets"));
        assert_eq!(state.resources[&id].len(), 2);
        assert!(!state.authorizers.contains_key(&id));
    }

    #[test]
    fn rejects_non_openapi_and_parses_yaml() {
        let (mut state, id, _) = state_with_api();
        assert!(import_openapi(&mut state, &id, &json!({"a": 1}), ImportMode::Overwrite).is_err());
        let doc = parse_openapi_document("openapi: 3.0.1\npaths:\n  /y:\n    get: {}\n").unwrap();
        import_openapi(&mut state, &id, &doc, ImportMode::Overwrite).unwrap();
        assert!(state.resources[&id].values().any(|r| r.path == "/y"));
    }
}
