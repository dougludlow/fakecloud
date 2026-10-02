//! AWS X-Ray (`xray`) restJson1 control plane + in-memory trace data plane for
//! fakecloud.
//!
//! The full 38-operation AWS X-Ray Smithy model. X-Ray signs SigV4 with the
//! `xray` scope and speaks restJson1; every operation is a `POST /<UriPath>`
//! (e.g. `POST /TraceSegments`, `POST /Traces`, `POST /ServiceGraph`), so
//! requests are routed by their `@http` URI path.
//!
//! This is real, persisted, account-partitioned state, not a set of stubs:
//!
//! * **Data plane.** `PutTraceSegments` ingests X-Ray trace segment documents
//!   (JSON), parsing each document's `trace_id` / `id` / `name` / `start_time` /
//!   `end_time` / `http` / `error`/`fault`/`throttle` flags and its nested
//!   `subsegments` (including `namespace: "remote"` downstream calls). Segments
//!   are stored keyed by trace id. `BatchGetTraces` reassembles stored traces,
//!   `GetTraceSummaries` filters them by time range (plus a documented
//!   filter-expression subset), and `GetServiceGraph` / `GetTraceGraph` /
//!   `GetTimeSeriesServiceStatistics` derive a service graph (nodes + edges with
//!   Ok/Error/Fault statistics and response time) computed deterministically
//!   from the ingested segments. See [`graph`] and [`segment`].
//! * **Control plane.** Sampling rules (with the built-in, undeletable `Default`
//!   rule seeded per account), groups, the account encryption config, resource
//!   policies, the indexing rule, the trace-segment destination, and ARN-keyed
//!   resource tagging are straightforward CRUD, persisted via snapshot/restore.
//!
//! Model-driven validation rejects contract violations with the error codes each
//! operation declares (`InvalidRequestException` universally,
//! `ResourceNotFoundException` on the ops that declare it, plus
//! `RuleLimitExceededException` / `TooManyTagsException`).

pub mod graph;
mod pagination_gen;
pub mod persistence;
pub mod segment;
pub mod service;
pub mod state;
mod validate;

pub use service::{XrayService, XRAY_ACTIONS};
pub use state::{SharedXrayState, XrayData, XraySnapshot, XRAY_SNAPSHOT_SCHEMA_VERSION};

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
        check(crate::pagination_gen::PAGED_OPS, "xray.json", &[]);
    }
}
