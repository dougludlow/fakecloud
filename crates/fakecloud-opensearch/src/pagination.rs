//! `MaxResults` / `NextToken` paging for every paginated OpenSearch and
//! Elasticsearch Service operation, applied at the dispatch boundary through
//! `fakecloud_core::pagination`: the token is validated before the handler runs
//! and the handler's full, stably ordered listing (state lives in `BTreeMap`s)
//! is sliced afterwards. The tables are generated from the Smithy models.

use fakecloud_core::pagination::JsonPagedOp;

use crate::pagination_gen::{ES_PAGED_OPS, OPENSEARCH_PAGED_OPS};
use crate::service::Api;

/// The paginated operations of `api`.
pub(crate) fn paged_ops(api: Api) -> &'static [JsonPagedOp] {
    match api {
        Api::Es => ES_PAGED_OPS,
        Api::OpenSearch => OPENSEARCH_PAGED_OPS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// The generated table lists exactly the operations each model paginates
    /// (rerun `scripts/generate-json-pagination-tables.py` after a
    /// model refresh).
    #[test]
    fn table_matches_the_models() {
        for (api, file) in [(Api::Es, "es.json"), (Api::OpenSearch, "opensearch.json")] {
            let path = format!("{}/../../aws-models/{file}", env!("CARGO_MANIFEST_DIR"));
            let model: Value =
                serde_json::from_str(&std::fs::read_to_string(&path).expect("read model")).unwrap();
            let shapes = model["shapes"].as_object().unwrap();
            let mut from_model: Vec<&str> = shapes
                .iter()
                .filter(|(_, s)| s["type"] == "operation")
                .filter(|(_, s)| {
                    s["input"]["target"]
                        .as_str()
                        .and_then(|t| shapes.get(t))
                        .is_some_and(|i| {
                            ["NextToken", "nextToken"]
                                .iter()
                                .any(|n| i["members"].get(*n).is_some())
                        })
                })
                .map(|(id, _)| id.rsplit('#').next().unwrap())
                .collect();
            from_model.sort_unstable();
            let mut table: Vec<&str> = paged_ops(api).iter().map(|o| o.action).collect();
            table.sort_unstable();
            assert_eq!(table, from_model, "{file}");
        }
    }
}
