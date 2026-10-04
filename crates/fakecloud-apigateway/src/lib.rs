//! API Gateway v1 (REST APIs) implementation.
//!
//! Distinct from `fakecloud-apigatewayv2` (HTTP APIs). The v1 surface
//! uses REST-style URLs (`POST /restapis`, `GET /restapis/{id}/...`)
//! and a different resource hierarchy: REST APIs own a tree of
//! resources, each with methods, integrations, method/integration
//! responses; deployments snapshot the API and stages point at them.
//!
//! Lambda integrations re-use the `DeliveryBus::invoke_lambda` path
//! already used by API Gateway v2 and EventBridge — same envelope,
//! different version field (`event.version = "1.0"`).

pub mod data_plane;
pub mod dispatch;
pub mod facade;
pub mod lambda_proxy;
pub mod model_validation;
pub mod openapi_import;
mod pagination_gen;
pub(crate) mod service;
pub(crate) mod state;
pub(crate) mod validation;
pub mod vtl;

pub use facade::ApiGatewayFacade;
pub use state::{
    apigateway_arn, execute_api_arn, make_id, ApiGatewaySnapshot, ApiGatewayState, ApiKey,
    AuthEffect, Authorizer, CachedAuthorizerResult, Deployment, Integration, Method, Model,
    Resource, RestApi, SharedApiGatewayState, Stage, UsagePlan, APIGATEWAY_SNAPSHOT_SCHEMA_VERSION,
};

pub use service::ApiGatewayService;

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
        check(
            crate::pagination_gen::PAGED_OPS,
            "apigateway.json",
            &["GetUsage"],
        );
    }
}
