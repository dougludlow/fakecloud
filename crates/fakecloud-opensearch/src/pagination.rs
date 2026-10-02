//! `MaxResults` / `NextToken` paging for every paginated OpenSearch and
//! Elasticsearch Service operation, applied at the dispatch boundary: the
//! request's token is validated before the handler runs and the handler's full,
//! stably ordered listing (state lives in `BTreeMap`s) is sliced afterwards.
//! [`PAGED_OPS`] is generated from the Smithy models.

use fakecloud_core::pagination::{page_json_response, parse_offset_token};
use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};
use http::StatusCode;
use serde_json::Value;

use crate::pagination_gen::PAGED_OPS;
use crate::service::Api;

/// Where a paging input travels on the wire.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Loc {
    /// An `@httpQuery` parameter.
    Query(&'static str),
    /// A top-level JSON body member.
    Body(&'static str),
}

/// One paginated operation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PagedOp {
    pub api: Api,
    pub action: &'static str,
    pub token: Loc,
    pub size: Option<Loc>,
    /// The output list a page slices.
    pub items: &'static str,
    /// The output member carrying the next page's token.
    pub next_token: &'static str,
    /// The output token is `@required`: the last page sends it empty rather
    /// than omitting it.
    pub next_token_required: bool,
    /// The error the operation declares for a token it did not mint; `None`
    /// when it declares none, in which case an unrecognised token starts
    /// from the top rather than returning an undeclared error.
    pub bad_token: Option<&'static str>,
}

/// A validated page request.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Page {
    op: &'static PagedOp,
    start: usize,
    size: Option<usize>,
}

fn read(req: &AwsRequest, body: &Value, loc: Loc) -> Option<String> {
    match loc {
        Loc::Query(name) => req.query_params.get(name).cloned(),
        Loc::Body(name) => match body.get(name)? {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        },
    }
}

/// Validate a paginated request's token before the handler runs, returning
/// the window to slice its result to (`None` when the operation is not
/// paginated or the request asks for everything).
pub(crate) fn validate(
    api: Api,
    action: &str,
    req: &AwsRequest,
) -> Result<Option<Page>, AwsServiceError> {
    let Some(op) = PAGED_OPS
        .iter()
        .find(|o| o.api == api && o.action == action)
    else {
        return Ok(None);
    };
    let body: Value = if req.body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&req.body).unwrap_or(Value::Null)
    };
    let token = read(req, &body, op.token);
    let start = match (parse_offset_token(token.as_deref()), op.bad_token) {
        (Ok(n), _) => n,
        (Err(_), Some(code)) => {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                code,
                format!("Invalid pagination token: {}", token.unwrap_or_default()),
            ))
        }
        (Err(_), None) => 0,
    };
    // `MaxResults` defaults to 0 in the model, meaning "the service default",
    // which here is the whole listing.
    let size = op
        .size
        .and_then(|loc| read(req, &body, loc))
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0);
    if start == 0 && size.is_none() {
        return Ok(None);
    }
    Ok(Some(Page { op, start, size }))
}

/// Slice a handler's response to `page`.
pub(crate) fn apply(resp: AwsResponse, page: Page) -> AwsResponse {
    let op = page.op;
    let paged = page_json_response(resp, op.items, op.next_token, page.start, page.size);
    if !op.next_token_required {
        return paged;
    }
    // Restore the required (empty) token on the last page.
    let fakecloud_core::service::ResponseBody::Bytes(bytes) = &paged.body else {
        return paged;
    };
    let Ok(mut v) = serde_json::from_slice::<Value>(bytes) else {
        return paged;
    };
    match v.as_object_mut() {
        Some(obj) if !obj.contains_key(op.next_token) => {
            obj.insert(op.next_token.to_string(), Value::String(String::new()));
            AwsResponse::json_value(paged.status, v)
        }
        _ => paged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generated table lists exactly the operations each model paginates
    /// (rerun `scripts/generate-opensearch-pagination-table.py` after a
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
            let mut table: Vec<&str> = PAGED_OPS
                .iter()
                .filter(|o| o.api == api)
                .map(|o| o.action)
                .collect();
            table.sort_unstable();
            assert_eq!(table, from_model, "{file}");
        }
    }
}
