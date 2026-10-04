pub mod cors;
pub mod extras;
pub mod http_proxy;
pub mod lambda_proxy;
pub mod management;
pub mod mock;
mod pagination_gen;
pub mod router;
pub(crate) mod service;
pub(crate) mod state;
pub mod websocket;
pub mod websocket_dispatch;

pub use service::{domain_for_host, ApiGatewayV2Service};
pub use state::{
    apigateway_arn, execute_api_arn, AccessLogSettings, ApiGatewayV2Snapshot, ApiGatewayV2State,
    Authorizer, ConnectionInfo, CorsConfiguration, DefinitionImport, Deployment, HttpApi,
    Integration, JwtConfiguration, Route, SharedApiGatewayV2State, SharedWebSocketRegistry, Stage,
    WebSocketRegistry, APIGATEWAYV2_SNAPSHOT_SCHEMA_VERSION,
};

/// The generated pagination table covers every operation the model
/// paginates (rerun `scripts/generate-json-pagination-tables.py` after a model
/// refresh). Listed exceptions page a map, not a list.
#[cfg(test)]
mod pagination_table_tests {
    fn check(ops: &[fakecloud_core::pagination::JsonPagedOp], model: &str, not_lists: &[&str]) {
        let path = format!("{}/../../aws-models/{model}", env!("CARGO_MANIFEST_DIR"));
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read model")).unwrap();
        let expected: Vec<String> = fakecloud_core::pagination::model_paginated_actions(&json)
            .into_iter()
            .filter(|a| !not_lists.contains(&a.as_str()))
            .collect();
        let mut table: Vec<String> = ops.iter().map(|o| o.action.to_string()).collect();
        table.sort_unstable();
        assert_eq!(table, expected, "{model}");
    }

    #[test]
    fn table_matches_the_model() {
        check(crate::pagination_gen::PAGED_OPS, "apigatewayv2.json", &[]);
    }
}
