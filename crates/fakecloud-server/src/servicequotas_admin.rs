//! `/_fakecloud/service-quotas/*`: inspect and change Service Quotas state.
//!
//! - `GET /quotas` lists every quota with its applied value, usage and
//!   enforcement state; `PUT`/`DELETE /quotas/{service}/{quota}` set or reset
//!   one quota's applied value and enforcement override.
//! - `GET`/`PUT /enforcement` read and change the global switch and per-quota
//!   overrides.
//! - `GET`/`PUT /request-approval` read and change how increase requests are
//!   decided; `GET /requests` lists them and `POST /requests/{id}/approve` or
//!   `/deny` decides a pending one.
//!
//! The logic lives in `fakecloud_servicequotas`; this module is the HTTP shim,
//! and it persists the Service Quotas snapshot after every successful change.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use fakecloud_servicequotas::service::{
    Decision, IntrospectionError, PutEnforcementRequest, PutQuotaRequest,
};
use fakecloud_servicequotas::ServiceQuotasService;

type Svc = Arc<ServiceQuotasService>;

fn respond(result: Result<Value, IntrospectionError>) -> Response {
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => (e.status, Json(json!({ "error": e.message }))).into_response(),
    }
}

/// Persist the snapshot when a change went through, then answer.
async fn respond_saved(svc: &Svc, result: Result<Value, IntrospectionError>) -> Response {
    if result.is_ok() {
        svc.save().await;
    }
    respond(result)
}

/// A JSON body that failed to parse, answered in the same `{"error": ...}`
/// shape as every other rejection.
fn parse_body<T: serde::de::DeserializeOwned + Default>(
    body: &[u8],
) -> Result<T, IntrospectionError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(body).map_err(|e| IntrospectionError {
        status: StatusCode::BAD_REQUEST,
        message: format!("invalid request body: {e}"),
    })
}

async fn list_quotas(State(svc): State<Svc>, Query(q): Query<HashMap<String, String>>) -> Response {
    respond(svc.introspect_quotas(
        q.get("accountId").map(String::as_str),
        q.get("region").map(String::as_str),
        q.get("serviceCode").map(String::as_str),
    ))
}

async fn put_quota(
    State(svc): State<Svc>,
    Path((service_code, quota_code)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Response {
    let result = parse_body::<PutQuotaRequest>(&body)
        .and_then(|b| svc.introspect_put_quota(&service_code, &quota_code, &b));
    respond_saved(&svc, result).await
}

async fn delete_quota(
    State(svc): State<Svc>,
    Path((service_code, quota_code)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let result = svc.introspect_delete_quota(
        &service_code,
        &quota_code,
        q.get("accountId").map(String::as_str),
        q.get("region").map(String::as_str),
    );
    respond_saved(&svc, result).await
}

async fn get_enforcement(State(svc): State<Svc>) -> Response {
    respond(Ok(svc.introspect_enforcement()))
}

async fn put_enforcement(State(svc): State<Svc>, body: axum::body::Bytes) -> Response {
    let result =
        parse_body::<PutEnforcementRequest>(&body).and_then(|b| svc.introspect_put_enforcement(&b));
    respond_saved(&svc, result).await
}

async fn get_request_approval(State(svc): State<Svc>) -> Response {
    respond(Ok(svc.introspect_request_approval()))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestApprovalBody {
    #[serde(default)]
    mode: Option<String>,
}

async fn put_request_approval(State(svc): State<Svc>, body: axum::body::Bytes) -> Response {
    let result = parse_body::<RequestApprovalBody>(&body).and_then(|b| {
        let mode = b.mode.ok_or_else(|| IntrospectionError {
            status: StatusCode::BAD_REQUEST,
            message: "mode is required (auto or manual)".to_string(),
        })?;
        svc.introspect_set_request_approval(&mode)
    });
    respond_saved(&svc, result).await
}

async fn list_requests(
    State(svc): State<Svc>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    respond(svc.introspect_requests(
        q.get("accountId").map(String::as_str),
        q.get("status").map(String::as_str),
    ))
}

async fn approve_request(State(svc): State<Svc>, Path(request_id): Path<String>) -> Response {
    let result = svc.introspect_decide_request(&request_id, Decision::Approve);
    respond_saved(&svc, result).await
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DenyBody {
    #[serde(default)]
    status: Option<String>,
}

async fn deny_request(
    State(svc): State<Svc>,
    Path(request_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let result = parse_body::<DenyBody>(&body)
        .and_then(|b| Decision::deny(b.status.as_deref()))
        .and_then(|d| svc.introspect_decide_request(&request_id, d));
    respond_saved(&svc, result).await
}

pub fn router(svc: Svc) -> Router {
    Router::new()
        .route("/_fakecloud/service-quotas/quotas", get(list_quotas))
        .route(
            "/_fakecloud/service-quotas/quotas/{service_code}/{quota_code}",
            put(put_quota).delete(delete_quota),
        )
        .route(
            "/_fakecloud/service-quotas/enforcement",
            get(get_enforcement).put(put_enforcement),
        )
        .route(
            "/_fakecloud/service-quotas/request-approval",
            get(get_request_approval).put(put_request_approval),
        )
        .route("/_fakecloud/service-quotas/requests", get(list_requests))
        .route(
            "/_fakecloud/service-quotas/requests/{request_id}/approve",
            post(approve_request),
        )
        .route(
            "/_fakecloud/service-quotas/requests/{request_id}/deny",
            post(deny_request),
        )
        .with_state(svc)
}
