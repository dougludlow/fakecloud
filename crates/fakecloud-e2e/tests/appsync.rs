//! End-to-end test for AWS AppSync, driven through the real `aws-sdk-appsync`
//! client against a live fakecloud server. Exercises the full control-plane +
//! schema round trip: create a GraphQL API, read it back via Get/List, mint an
//! API key, create a `NONE` data source, ingest an SDL schema and settle its
//! creation status to `SUCCESS`, attach a resolver to a `type.field` and read
//! it back, create a pipeline function, tag/list-tags, and delete the API.

use aws_sdk_appsync::primitives::Blob;
use aws_sdk_appsync::types::{AuthenticationType, DataSourceType, SchemaStatus};
use fakecloud_testkit::TestServer;

async fn appsync_client(server: &TestServer) -> aws_sdk_appsync::Client {
    aws_sdk_appsync::Client::new(&server.aws_config().await)
}

#[tokio::test]
async fn appsync_control_plane_and_schema_round_trip() {
    let server = TestServer::start().await;
    let appsync = appsync_client(&server).await;

    // --- Create a GraphQL API ---
    let created = appsync
        .create_graphql_api()
        .name("e2e-demo")
        .authentication_type(AuthenticationType::ApiKey)
        .send()
        .await
        .expect("create_graphql_api");
    let api = created.graphql_api().expect("graphqlApi present");
    let api_id = api.api_id().expect("apiId").to_string();
    let arn = api.arn().expect("arn").to_string();
    assert_eq!(api.name(), Some("e2e-demo"));
    assert_eq!(api.authentication_type(), Some(&AuthenticationType::ApiKey));
    // The GRAPHQL + REALTIME endpoint URIs are minted.
    assert!(api.uris().and_then(|u| u.get("GRAPHQL")).is_some());
    assert!(api.uris().and_then(|u| u.get("REALTIME")).is_some());

    // --- Get + List round-trip ---
    let got = appsync
        .get_graphql_api()
        .api_id(&api_id)
        .send()
        .await
        .expect("get_graphql_api");
    assert_eq!(got.graphql_api().and_then(|a| a.name()), Some("e2e-demo"));

    let listed = appsync
        .list_graphql_apis()
        .send()
        .await
        .expect("list_graphql_apis");
    assert!(listed
        .graphql_apis()
        .iter()
        .any(|a| a.api_id() == Some(api_id.as_str())));

    // --- API key with expiry ---
    let key = appsync
        .create_api_key()
        .api_id(&api_id)
        .description("e2e key")
        .send()
        .await
        .expect("create_api_key");
    assert!(key.api_key().and_then(|k| k.id()).is_some());

    // --- Data source (NONE) ---
    let ds = appsync
        .create_data_source()
        .api_id(&api_id)
        .name("localSrc")
        .r#type(DataSourceType::None)
        .send()
        .await
        .expect("create_data_source");
    assert_eq!(
        ds.data_source().and_then(|d| d.r#type()),
        Some(&DataSourceType::None)
    );

    // --- Ingest an SDL schema, then settle creation status to SUCCESS ---
    let sdl = "type Query { hello: String }\nschema { query: Query }";
    appsync
        .start_schema_creation()
        .api_id(&api_id)
        .definition(Blob::new(sdl.as_bytes()))
        .send()
        .await
        .expect("start_schema_creation");
    let status = appsync
        .get_schema_creation_status()
        .api_id(&api_id)
        .send()
        .await
        .expect("get_schema_creation_status");
    assert_eq!(status.status(), Some(&SchemaStatus::Success));

    // --- Resolver on Query.hello, backed by the data source ---
    let resolver = appsync
        .create_resolver()
        .api_id(&api_id)
        .type_name("Query")
        .field_name("hello")
        .data_source_name("localSrc")
        .send()
        .await
        .expect("create_resolver");
    assert_eq!(
        resolver.resolver().and_then(|r| r.field_name()),
        Some("hello")
    );

    let got_resolver = appsync
        .get_resolver()
        .api_id(&api_id)
        .type_name("Query")
        .field_name("hello")
        .send()
        .await
        .expect("get_resolver");
    assert_eq!(
        got_resolver.resolver().and_then(|r| r.data_source_name()),
        Some("localSrc")
    );

    let resolvers = appsync
        .list_resolvers()
        .api_id(&api_id)
        .type_name("Query")
        .send()
        .await
        .expect("list_resolvers");
    assert_eq!(resolvers.resolvers().len(), 1);

    // --- Pipeline function ---
    let func = appsync
        .create_function()
        .api_id(&api_id)
        .name("fn1")
        .data_source_name("localSrc")
        .send()
        .await
        .expect("create_function");
    assert!(func
        .function_configuration()
        .and_then(|f| f.function_id())
        .is_some());

    // --- Tagging round-trip on the API ARN ---
    appsync
        .tag_resource()
        .resource_arn(&arn)
        .tags("team", "platform")
        .send()
        .await
        .expect("tag_resource");
    let tags = appsync
        .list_tags_for_resource()
        .resource_arn(&arn)
        .send()
        .await
        .expect("list_tags_for_resource");
    assert_eq!(
        tags.tags().and_then(|t| t.get("team")).map(String::as_str),
        Some("platform")
    );

    // --- Delete the API ---
    appsync
        .delete_graphql_api()
        .api_id(&api_id)
        .send()
        .await
        .expect("delete_graphql_api");
    let after = appsync.get_graphql_api().api_id(&api_id).send().await;
    assert!(
        after.is_err(),
        "GraphQL API should be gone after DeleteGraphqlApi"
    );
}

/// Event API create-time tags show in ListTagsForResource; UpdateGraphqlApi
/// merges onto the stored API (apiType / visibility / tags survive).
#[tokio::test]
async fn appsync_tags_single_store_and_update_merges() {
    use aws_sdk_appsync::types::{AuthProvider, EventConfig, GraphQlApiType, GraphQlApiVisibility};
    let server = TestServer::start().await;
    let appsync = appsync_client(&server).await;

    let api_key = AuthProvider::builder()
        .auth_type(AuthenticationType::ApiKey)
        .build()
        .unwrap();
    let ev = appsync
        .create_api()
        .name("events")
        .event_config(
            EventConfig::builder()
                .auth_providers(api_key.clone())
                .connection_auth_modes(
                    aws_sdk_appsync::types::AuthMode::builder()
                        .auth_type(AuthenticationType::ApiKey)
                        .build()
                        .unwrap(),
                )
                .default_publish_auth_modes(
                    aws_sdk_appsync::types::AuthMode::builder()
                        .auth_type(AuthenticationType::ApiKey)
                        .build()
                        .unwrap(),
                )
                .default_subscribe_auth_modes(
                    aws_sdk_appsync::types::AuthMode::builder()
                        .auth_type(AuthenticationType::ApiKey)
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .tags("env", "prod")
        .send()
        .await
        .expect("create_api");
    let ev_arn = ev.api().unwrap().api_arn().unwrap().to_string();
    let listed = appsync
        .list_tags_for_resource()
        .resource_arn(&ev_arn)
        .send()
        .await
        .expect("list_tags_for_resource");
    assert_eq!(
        listed.tags().unwrap().get("env").map(String::as_str),
        Some("prod")
    );

    let created = appsync
        .create_graphql_api()
        .name("merged")
        .authentication_type(AuthenticationType::ApiKey)
        .api_type(GraphQlApiType::Merged)
        .visibility(GraphQlApiVisibility::Private)
        .merged_api_execution_role_arn("arn:aws:iam::000000000000:role/merge")
        .tags("team", "x")
        .send()
        .await
        .expect("create_graphql_api");
    let api_id = created.graphql_api().unwrap().api_id().unwrap().to_string();
    let updated = appsync
        .update_graphql_api()
        .api_id(&api_id)
        .name("merged-2")
        .authentication_type(AuthenticationType::ApiKey)
        .send()
        .await
        .expect("update_graphql_api");
    let api = updated.graphql_api().unwrap();
    assert_eq!(api.name(), Some("merged-2"));
    assert_eq!(api.api_type(), Some(&GraphQlApiType::Merged));
    assert_eq!(api.visibility(), Some(&GraphQlApiVisibility::Private));
    assert_eq!(
        api.tags().and_then(|t| t.get("team")).map(String::as_str),
        Some("x")
    );
}
