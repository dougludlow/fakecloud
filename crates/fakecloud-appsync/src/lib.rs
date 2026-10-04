//! AWS AppSync (`appsync`) restJson1 control plane + schema state for fakecloud.
//!
//! The full 74-operation AWS AppSync Smithy model. AppSync signs SigV4 with the
//! `appsync` scope and speaks restJson1; every operation is a RESTful
//! `<METHOD> /v1|/v2/...` route with path labels (e.g. `POST /v1/apis`,
//! `GET /v1/apis/{apiId}`, `POST /v1/apis/{apiId}/types/{typeName}/resolvers`),
//! so requests are routed by their HTTP method + `@http` URI template.
//!
//! This is real, persisted, account-partitioned control-plane + schema state,
//! not a set of stubs:
//!
//! * **GraphQL APIs.** `CreateGraphqlApi` mints an `apiId`, ARN, GRAPHQL +
//!   REALTIME endpoint `uris`, and echoes the auth config (`API_KEY`,
//!   `AWS_IAM`, `AMAZON_COGNITO_USER_POOLS`, `OPENID_CONNECT`, `AWS_LAMBDA`),
//!   round-tripping via Get/List/Update/Delete.
//! * **Sub-resources.** API keys (with expiry), data sources (all
//!   `DataSourceType`s with their config echoed), resolvers (UNIT/PIPELINE,
//!   VTL or `APPSYNC_JS` code, attached to `type.field`), functions, schema
//!   types, API caches, domain names + API associations, the newer Event-API
//!   surface (`CreateApi`/channel namespaces), and merged/source-API
//!   associations. Every sub-resource op validates its parent and returns the
//!   declared `NotFoundException` when the parent (or the resource) is absent.
//! * **Schema lifecycle.** `StartSchemaCreation` ingests an SDL schema blob and
//!   settles `GetSchemaCreationStatus` to `SUCCESS` on read (a state machine
//!   like other async ops). `GetIntrospectionSchema` returns a representation
//!   (SDL or JSON per the `format` query param) derived from the stored schema.
//! * **Evaluation.** `EvaluateCode` / `EvaluateMappingTemplate` validate the
//!   code/template + context are well-formed and return a deterministic,
//!   honestly-documented evaluated result (see [`evaluate`]); they do NOT run a
//!   full VTL/`APPSYNC_JS` interpreter.
//!
//! Model-driven validation rejects contract violations with the error codes
//! each operation declares (`BadRequestException` universally, plus
//! `NotFoundException`, `ConcurrentModificationException`, etc.).
//!
//! **Not yet implemented (documented gap, not stubbed):** the GraphQL query
//! *execution* data plane -- actually resolving GraphQL queries/subscriptions
//! against the configured data sources -- is a later batch. This crate is the
//! faithful control plane + schema state that such execution would build on.

pub mod evaluate;
mod pagination_gen;
pub mod persistence;
pub mod schema;
pub mod service;
pub mod state;
mod validate;

pub use service::{AppSyncService, APPSYNC_ACTIONS};
pub use state::{
    AppSyncData, AppSyncSnapshot, SharedAppSyncState, APPSYNC_SNAPSHOT_SCHEMA_VERSION,
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
        check(crate::pagination_gen::PAGED_OPS, "appsync.json", &[]);
    }
}
