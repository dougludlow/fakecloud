//! SAM `AWS::Serverless::Function` `Api`/`HttpApi` events expand into a working
//! implicit API: a function with an Api event previously deployed with no
//! routes (every call 404'd). The transform now synthesizes the implicit
//! RestApi (resources + methods + deployment + stage) and HttpApi (integration
//! + route + stage) so the routes exist.

mod helpers;

use aws_sdk_cloudformation::types::Capability;
use helpers::TestServer;

async fn physical_id(cfn: &aws_sdk_cloudformation::Client, stack: &str, logical: &str) -> String {
    cfn.describe_stack_resource()
        .stack_name(stack)
        .logical_resource_id(logical)
        .send()
        .await
        .unwrap_or_else(|e| panic!("describe_stack_resource {logical}: {e:?}"))
        .stack_resource_detail()
        .and_then(|d| d.physical_resource_id())
        .unwrap_or_else(|| panic!("{logical} has a physical id"))
        .to_string()
}

async fn wait_terminal(cfn: &aws_sdk_cloudformation::Client, stack: &str) -> (String, String) {
    helpers::wait_until(std::time::Duration::from_secs(60), || async {
        let out = cfn.describe_stacks().stack_name(stack).send().await.ok()?;
        let s = out.stacks().first()?;
        let status = s.stack_status()?.as_str().to_string();
        (!status.ends_with("IN_PROGRESS")).then(|| {
            (
                status,
                s.stack_status_reason().unwrap_or_default().to_string(),
            )
        })
    })
    .await
    .expect("stack reached a terminal status")
}

const TEMPLATE: &str = r#"
AWSTemplateFormatVersion: '2010-09-09'
Transform: AWS::Serverless-2016-10-31
Resources:
  Api:
    Type: AWS::Serverless::Function
    Properties:
      FunctionName: sam-api-fn
      Runtime: python3.12
      Handler: index.handler
      InlineCode: |
        def handler(event, context):
            return {"statusCode": 200, "body": "ok"}
      Events:
        Hello:
          Type: Api
          Properties:
            Path: /hello
            Method: get
"#;

#[tokio::test]
async fn sam_function_api_event_creates_implicit_rest_api() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;

    cfn.create_stack()
        .stack_name("sam-api")
        .template_body(TEMPLATE)
        .capabilities(Capability::CapabilityNamedIam)
        .send()
        .await
        .expect("create_stack");

    let described = cfn
        .describe_stacks()
        .stack_name("sam-api")
        .send()
        .await
        .expect("describe_stacks");
    assert_eq!(
        described
            .stacks()
            .first()
            .unwrap()
            .stack_status()
            .unwrap()
            .as_str(),
        "CREATE_COMPLETE"
    );

    // The implicit ServerlessRestApi must exist with a /hello resource and a GET
    // method wired to the function via an AWS_PROXY integration.
    // SAM names the implicit API after the stack (the generated OpenAPI
    // definition's title).
    let api_id = physical_id(&cfn, "sam-api", "ServerlessRestApi").await;
    let apigw = server.apigateway_client().await;
    let api = apigw
        .get_rest_api()
        .rest_api_id(&api_id)
        .send()
        .await
        .expect("get_rest_api");
    assert_eq!(api.name(), Some("sam-api"));
    let api_id = api_id.as_str();

    let resources = apigw
        .get_resources()
        .rest_api_id(api_id)
        .send()
        .await
        .expect("get_resources");
    let hello = resources
        .items()
        .iter()
        .find(|r| r.path() == Some("/hello"))
        .expect("/hello resource synthesized");
    assert!(
        hello
            .resource_methods()
            .map(|m| m.contains_key("GET"))
            .unwrap_or(false),
        "GET method on /hello: {:?}",
        hello.resource_methods()
    );

    // A deployed stage must exist so the route is reachable.
    let stages = apigw
        .get_stages()
        .rest_api_id(api_id)
        .send()
        .await
        .expect("get_stages");
    assert!(
        !stages.item().is_empty(),
        "a stage must be deployed for the implicit API"
    );
}

const EXPLICIT_TEMPLATE: &str = r#"
AWSTemplateFormatVersion: '2010-09-09'
Transform: AWS::Serverless-2016-10-31
Resources:
  Uploads:
    Type: AWS::S3::Bucket
    Properties:
      BucketName: sam-explicit-uploads
  MyApi:
    Type: AWS::Serverless::Api
    Properties:
      StageName: dev
      Cors:
        AllowOrigin: "'https://app.example.com'"
        AllowHeaders: "'Content-Type'"
  Fn:
    Type: AWS::Serverless::Function
    Properties:
      FunctionName: sam-explicit-fn
      Runtime: python3.12
      Handler: index.handler
      InlineCode: |
        def handler(event, context):
            return {"statusCode": 200, "body": "ok"}
      AutoPublishAlias: live
      FunctionUrlConfig:
        AuthType: NONE
      Events:
        GetItem:
          Type: Api
          Properties:
            RestApiId: !Ref MyApi
            Path: /items/{id}
            Method: get
        Upload:
          Type: S3
          Properties:
            Bucket: !Ref Uploads
            Events: s3:ObjectCreated:*
Outputs:
  AliasArn:
    Value: !Ref Fn.Alias
  Url:
    Value: !GetAtt FnUrl.FunctionUrl
"#;

/// An explicit `AWS::Serverless::Api` gets the routes of the function events
/// that name it (previously dropped, and the API failed with "Name is
/// required"), an S3 event wires the bucket's notification, and
/// `AutoPublishAlias` publishes a version + alias that the events target.
#[tokio::test]
async fn sam_explicit_api_s3_event_and_auto_publish_alias() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    cfn.create_stack()
        .stack_name("sam-explicit")
        .template_body(EXPLICIT_TEMPLATE)
        .capabilities(Capability::CapabilityNamedIam)
        .capabilities(Capability::CapabilityAutoExpand)
        .send()
        .await
        .expect("create_stack");
    let (status, reason) = wait_terminal(&cfn, "sam-explicit").await;
    assert_eq!(status, "CREATE_COMPLETE", "{reason}");

    let alias_arn = "arn:aws:lambda:us-east-1:123456789012:function:sam-explicit-fn:live";
    let outputs = cfn
        .describe_stacks()
        .stack_name("sam-explicit")
        .send()
        .await
        .unwrap()
        .stacks()[0]
        .outputs()
        .to_vec();
    let output = |key: &str| {
        outputs
            .iter()
            .find(|o| o.output_key() == Some(key))
            .and_then(|o| o.output_value())
            .unwrap_or_default()
            .to_string()
    };
    assert_eq!(output("AliasArn"), alias_arn);
    assert!(output("Url").contains(".lambda-url."), "{}", output("Url"));

    // --- the explicit API carries the route, named after the stack ---
    let api_id = physical_id(&cfn, "sam-explicit", "MyApi").await;
    let apigw = server.apigateway_client().await;
    let api = apigw
        .get_rest_api()
        .rest_api_id(&api_id)
        .send()
        .await
        .unwrap();
    assert_eq!(api.name(), Some("sam-explicit"));
    let resources = apigw
        .get_resources()
        .rest_api_id(&api_id)
        .send()
        .await
        .unwrap();
    let item = resources
        .items()
        .iter()
        .find(|r| r.path() == Some("/items/{id}"))
        .expect("/items/{id} resource");
    let methods = item.resource_methods().unwrap();
    assert!(
        methods.contains_key("GET") && methods.contains_key("OPTIONS"),
        "{methods:?}"
    );
    let integration = apigw
        .get_integration()
        .rest_api_id(&api_id)
        .resource_id(item.id().unwrap())
        .http_method("GET")
        .send()
        .await
        .unwrap();
    assert!(
        integration.uri().unwrap_or_default().contains(alias_arn),
        "integration targets the alias: {:?}",
        integration.uri()
    );
    apigw
        .get_stage()
        .rest_api_id(&api_id)
        .stage_name("dev")
        .send()
        .await
        .expect("dev stage deployed");

    // The CORS preflight is served by the deployed API's MOCK integration.
    let http = reqwest::Client::new();
    let resp = http
        .request(
            reqwest::Method::OPTIONS,
            format!("{}/dev/items/42", server.endpoint()),
        )
        .header(
            "host",
            format!("{api_id}.execute-api.us-east-1.amazonaws.com"),
        )
        .send()
        .await
        .expect("preflight");
    let status = resp.status();
    let allow_origin = resp
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "{body}");
    assert_eq!(allow_origin.as_deref(), Some("https://app.example.com"));

    // --- AutoPublishAlias: version 1 behind the `live` alias ---
    let lambda = server.lambda_client().await;
    let alias = lambda
        .get_alias()
        .function_name("sam-explicit-fn")
        .name("live")
        .send()
        .await
        .expect("get_alias");
    assert_eq!(alias.function_version(), Some("1"));
    let url = lambda
        .get_function_url_config()
        .function_name("sam-explicit-fn")
        .qualifier("live")
        .send()
        .await
        .expect("function url on the alias");
    assert_eq!(url.auth_type().as_str(), "NONE");

    // --- the S3 event notifies the alias ---
    let s3 = server.s3_client().await;
    let notification = s3
        .get_bucket_notification_configuration()
        .bucket("sam-explicit-uploads")
        .send()
        .await
        .unwrap();
    let lambda_configs = notification.lambda_function_configurations();
    assert_eq!(lambda_configs.len(), 1, "{lambda_configs:?}");
    assert_eq!(lambda_configs[0].lambda_function_arn(), alias_arn);
}

/// A function event of a type SAM does not define fails the stack instead of
/// being silently dropped.
#[tokio::test]
async fn sam_unknown_event_type_fails_the_stack() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let template = r#"
Transform: AWS::Serverless-2016-10-31
Resources:
  Fn:
    Type: AWS::Serverless::Function
    Properties:
      Runtime: python3.12
      Handler: index.handler
      InlineCode: "x"
      Events:
        Bad:
          Type: Pigeon
"#;
    let result = cfn
        .create_stack()
        .stack_name("sam-bad-event")
        .template_body(template)
        .capabilities(Capability::CapabilityAutoExpand)
        .send()
        .await;
    match result {
        Err(e) => {
            let msg = format!("{e:?}");
            assert!(msg.contains("Pigeon"), "{msg}");
        }
        Ok(_) => {
            let (status, reason) = wait_terminal(&cfn, "sam-bad-event").await;
            assert_ne!(status, "CREATE_COMPLETE");
            assert!(reason.contains("Pigeon"), "{reason}");
        }
    }
}

fn http_definition_template(description: &str) -> String {
    format!(
        r#"
Transform: AWS::Serverless-2016-10-31
Resources:
  Http:
    Type: AWS::Serverless::HttpApi
    Properties:
      Description: {description}
      Auth:
        Authorizers:
          Jwt:
            JwtConfiguration:
              issuer: https://issuer.example.com
              audience: [app]
            IdentitySource: $request.header.Authorization
      DefinitionBody:
        openapi: "3.0.1"
        info:
          title: sam-http-def
          version: "1"
        paths: {{}}
  Fn:
    Type: AWS::Serverless::Function
    Properties:
      FunctionName: sam-http-def-fn
      Runtime: python3.12
      Handler: index.handler
      InlineCode: "def handler(e, c): return {{}}"
      Events:
        Open:
          Type: HttpApi
          Properties:
            ApiId: !Ref Http
            Path: /open
            Method: GET
        Secured:
          Type: HttpApi
          Properties:
            ApiId: !Ref Http
            Path: /secured
            Method: GET
            Auth:
              Authorizer: Jwt
"#
    )
}

/// An HttpApi with a DefinitionBody keeps its event routes across a stack
/// update that only changes another property: the unauthenticated route is
/// part of the definition, and the JWT-secured route (a separate resource)
/// is not wiped by re-importing an unchanged definition.
#[tokio::test]
async fn sam_http_api_definition_body_keeps_event_routes_on_update() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    cfn.create_stack()
        .stack_name("sam-http-def")
        .template_body(http_definition_template("first"))
        .capabilities(Capability::CapabilityNamedIam)
        .capabilities(Capability::CapabilityAutoExpand)
        .send()
        .await
        .expect("create_stack");
    let (status, reason) = wait_terminal(&cfn, "sam-http-def").await;
    assert_eq!(status, "CREATE_COMPLETE", "{reason}");
    let api_id = physical_id(&cfn, "sam-http-def", "Http").await;
    let v2 = server.apigatewayv2_client().await;
    let route_keys = || async {
        let mut keys: Vec<String> = v2
            .get_routes()
            .api_id(&api_id)
            .send()
            .await
            .unwrap()
            .items()
            .iter()
            .filter_map(|r| r.route_key().map(str::to_string))
            .collect();
        keys.sort();
        keys
    };
    assert_eq!(route_keys().await, vec!["GET /open", "GET /secured"]);

    cfn.update_stack()
        .stack_name("sam-http-def")
        .template_body(http_definition_template("second"))
        .capabilities(Capability::CapabilityNamedIam)
        .capabilities(Capability::CapabilityAutoExpand)
        .send()
        .await
        .expect("update_stack");
    let (status, reason) = wait_terminal(&cfn, "sam-http-def").await;
    assert_eq!(status, "UPDATE_COMPLETE", "{reason}");
    let api = v2.get_api().api_id(&api_id).send().await.unwrap();
    assert_eq!(api.description(), Some("second"));
    assert_eq!(route_keys().await, vec!["GET /open", "GET /secured"]);
}
