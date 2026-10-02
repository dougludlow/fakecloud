//! Segment handlers: versioned create/get/update/delete/list plus the
//! import/export-job reads.

use serde_json::{json, Map, Value};

use fakecloud_core::service::{AwsResponse, AwsServiceError};

use super::{created, merge_body, not_found, not_found_app, ok, paginate, Ctx, PinpointService};
use crate::shared;
use crate::state::Versioned;

impl PinpointService {
    pub(super) fn create_segment(
        &self,
        ctx: &Ctx,
        app_id: &str,
        body: &Value,
    ) -> Result<AwsResponse, AwsServiceError> {
        let id = shared::hex_id();
        let mut guard = self.state.write();
        let data = guard
            .get_mut(&ctx.account)
            .filter(|d| d.apps.contains_key(app_id))
            .ok_or_else(|| super::not_found_app(app_id))?;
        let record = build_segment(ctx, app_id, &id, 1, body);
        // Create-time tags live in the ARN-keyed tag store the reads render.
        if let Some(arn) = record.get("Arn").and_then(Value::as_str) {
            data.set_tags(arn, body.get("tags"));
        }
        if let Some(app) = data.apps.get_mut(app_id) {
            app.segments.insert(
                id,
                Versioned {
                    current: record.clone(),
                    versions: vec![record.clone()],
                },
            );
        }
        created(data.render(&record))
    }

    pub(super) fn get_segment(
        &self,
        ctx: &Ctx,
        app_id: &str,
        sid: &str,
    ) -> Result<AwsResponse, AwsServiceError> {
        self.with_app_data(&ctx.account, app_id, |data, app| {
            app.segments
                .get(sid)
                .map(|v| data.render(&v.current))
                .ok_or_else(|| not_found_segment(sid))
        })
        .and_then(ok)
    }

    pub(super) fn update_segment(
        &self,
        ctx: &Ctx,
        app_id: &str,
        sid: &str,
        body: &Value,
    ) -> Result<AwsResponse, AwsServiceError> {
        let mut guard = self.state.write();
        let data = guard
            .get_mut(&ctx.account)
            .filter(|d| d.apps.contains_key(app_id))
            .ok_or_else(|| not_found_app(app_id))?;
        let seg = data
            .apps
            .get_mut(app_id)
            .and_then(|app| app.segments.get_mut(sid))
            .ok_or_else(|| not_found_segment(sid))?;
        let version = seg.versions.len() as i64 + 1;
        // `tags` on an update is deprecated and ignored by AWS: the segment
        // keeps the tags in the tag store.
        let record = build_segment(ctx, app_id, sid, version, body);
        seg.versions.push(record.clone());
        seg.current = record.clone();
        ok(data.render(&record))
    }

    pub(super) fn delete_segment(
        &self,
        ctx: &Ctx,
        app_id: &str,
        sid: &str,
    ) -> Result<AwsResponse, AwsServiceError> {
        let mut guard = self.state.write();
        let data = guard
            .get_mut(&ctx.account)
            .filter(|d| d.apps.contains_key(app_id))
            .ok_or_else(|| not_found_app(app_id))?;
        let removed = data
            .apps
            .get_mut(app_id)
            .and_then(|app| app.segments.remove(sid));
        match removed {
            Some(v) => {
                let out = data.render(&v.current);
                if let Some(arn) = v.current.get("Arn").and_then(Value::as_str) {
                    data.remove_tags(arn);
                }
                ok(out)
            }
            None => Err(not_found_segment(sid)),
        }
    }

    pub(super) fn get_segments(
        &self,
        ctx: &Ctx,
        app_id: &str,
        q: &[(String, String)],
    ) -> Result<AwsResponse, AwsServiceError> {
        let items = self.with_app_data(&ctx.account, app_id, |data, app| {
            Ok(app
                .segments
                .values()
                .map(|v| data.render(&v.current))
                .collect::<Vec<_>>())
        })?;
        segments_page(items, q)
    }

    pub(super) fn get_segment_versions(
        &self,
        ctx: &Ctx,
        app_id: &str,
        sid: &str,
        q: &[(String, String)],
    ) -> Result<AwsResponse, AwsServiceError> {
        let items = self.with_app_data(&ctx.account, app_id, |data, app| {
            app.segments
                .get(sid)
                .map(|v| {
                    v.versions
                        .iter()
                        .rev()
                        .map(|r| data.render(r))
                        .collect::<Vec<_>>()
                })
                .ok_or_else(|| not_found_segment(sid))
        })?;
        segments_page(items, q)
    }

    pub(super) fn get_segment_version(
        &self,
        ctx: &Ctx,
        app_id: &str,
        sid: &str,
        ver: &str,
    ) -> Result<AwsResponse, AwsServiceError> {
        self.with_app_data(&ctx.account, app_id, |data, app| {
            let seg = app
                .segments
                .get(sid)
                .ok_or_else(|| not_found_segment(sid))?;
            let n: usize = ver
                .parse()
                .map_err(|_| not_found(&format!("Segment version '{ver}' does not exist.")))?;
            seg.versions
                .get(n.wrapping_sub(1))
                .map(|r| data.render(r))
                .ok_or_else(|| not_found(&format!("Segment version '{ver}' does not exist.")))
        })
        .and_then(ok)
    }

    pub(super) fn get_segment_import_jobs(
        &self,
        ctx: &Ctx,
        app_id: &str,
        sid: &str,
        _q: &[(String, String)],
    ) -> Result<AwsResponse, AwsServiceError> {
        self.segment_jobs(ctx, app_id, sid)
    }

    pub(super) fn get_segment_export_jobs(
        &self,
        ctx: &Ctx,
        app_id: &str,
        sid: &str,
        _q: &[(String, String)],
    ) -> Result<AwsResponse, AwsServiceError> {
        self.segment_jobs(ctx, app_id, sid)
    }

    fn segment_jobs(
        &self,
        ctx: &Ctx,
        app_id: &str,
        sid: &str,
    ) -> Result<AwsResponse, AwsServiceError> {
        self.with_app(&ctx.account, app_id, |app| {
            if app.segments.contains_key(sid) {
                Ok(json!({ "Item": [] }))
            } else {
                Err(not_found_segment(sid))
            }
        })
        .and_then(ok)
    }
}

fn not_found_segment(sid: &str) -> AwsServiceError {
    not_found(&format!("Segment with id '{sid}' does not exist."))
}

fn segments_page(
    items: Vec<Value>,
    q: &[(String, String)],
) -> Result<AwsResponse, AwsServiceError> {
    let (page, next) = paginate(items, q)?;
    let mut out = Map::new();
    out.insert("Item".into(), json!(page));
    if let Some(n) = next {
        out.insert("NextToken".into(), json!(n));
    }
    ok(Value::Object(out))
}

fn build_segment(ctx: &Ctx, app_id: &str, id: &str, version: i64, body: &Value) -> Value {
    let now = shared::now_iso();
    let segment_type = if body
        .get("ImportDefinition")
        .map(|v| !v.is_null())
        .unwrap_or(false)
    {
        "IMPORT"
    } else {
        "DIMENSIONAL"
    };
    let mut out = Map::new();
    // Persist the full write-request definition (Dimensions / SegmentGroups /
    // Name / tags / ImportDefinition, ...) so GetSegment round-trips it, then
    // overlay server-authoritative members.
    merge_body(&mut out, body);
    // Tags live in the ARN-keyed tag store, never inline on the record.
    out.remove("tags");
    out.insert("Id".into(), json!(id));
    out.insert("ApplicationId".into(), json!(app_id));
    out.insert(
        "Arn".into(),
        json!(shared::nested_arn(
            &ctx.region,
            &ctx.account,
            app_id,
            "segments",
            id
        )),
    );
    out.insert("CreationDate".into(), json!(now));
    out.insert("LastModifiedDate".into(), json!(now));
    out.insert("SegmentType".into(), json!(segment_type));
    out.insert("Version".into(), json!(version));
    Value::Object(out)
}
