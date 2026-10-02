//! `Search` and `QueryLineage`: read-only queries evaluated over the real
//! stored resources instead of a synthesised empty result.
//!
//! * `Search` maps the requested `ResourceType` to its stored resource family,
//!   evaluates the `SearchExpression` (filters, nested filters,
//!   sub-expressions, `And` / `Or`) against each record (with its tags
//!   attached, so `Tags.<key>` filters work), sorts, paginates and projects
//!   every hit onto the `SearchRecord` member the resource type carries.
//! * `QueryLineage` walks the lineage graph formed by the stored
//!   `AddAssociation` edges from the requested start entities, in the
//!   requested direction and depth, and returns the reached vertices (with
//!   their lineage type and entity type) and traversed edges.

use std::cmp::Ordering;
use std::collections::{BTreeSet, VecDeque};

use serde_json::{Map, Value};

use fakecloud_core::service::{AwsResponse, AwsServiceError};

use crate::generated::{OpMeta, SEARCH_RECORD, SEARCH_SHAPES};
use crate::state::SageMakerData;

use super::{engine, not_found, ok_json, Ctx, SageMakerService};

const FEATURE_META: &str = "__FeatureMetadata";

/// `ResourceType` -> (stored family, `SearchRecord` member). Resource types
/// whose `SearchRecord` carries no member (`Image`, `ImageVersion`) map to
/// `None` and match nothing.
fn search_target(resource_type: &str) -> Option<(&'static str, &'static str)> {
    Some(match resource_type {
        "TrainingJob" => ("TrainingJob", "TrainingJob"),
        "Experiment" => ("Experiment", "Experiment"),
        "ExperimentTrial" => ("Trial", "Trial"),
        "ExperimentTrialComponent" => ("TrialComponent", "TrialComponent"),
        "Endpoint" => ("Endpoint", "Endpoint"),
        "Model" => ("Model", "Model"),
        "ModelPackage" => ("ModelPackage", "ModelPackage"),
        "ModelPackageGroup" => ("ModelPackageGroup", "ModelPackageGroup"),
        "Pipeline" => ("Pipeline", "Pipeline"),
        "PipelineExecution" => ("PipelineExecution", "PipelineExecution"),
        "PipelineVersion" => ("PipelineVersion", "PipelineVersion"),
        "FeatureGroup" => ("FeatureGroup", "FeatureGroup"),
        "FeatureMetadata" => ("FeatureGroup", "FeatureMetadata"),
        "Project" => ("Project", "Project"),
        "HyperParameterTuningJob" => ("HyperParameterTuningJob", "HyperParameterTuningJob"),
        "ModelCard" => ("ModelCard", "ModelCard"),
        "Job" => ("Job", "Job"),
        "HubContent" => ("HubContent", "HubContent"),
        _ => return None,
    })
}

/// The candidate documents a search over `resource_type` evaluates: each
/// stored record of the family with its tags attached. `FeatureMetadata`
/// fans each feature group out into one document per feature definition.
fn candidates(data: &SageMakerData, resource_type: &str, family: &str) -> Vec<Value> {
    let entries = data.list_resource_entries(family);
    if resource_type != "FeatureMetadata" {
        return entries
            .iter()
            .map(|(_, r)| data.record_with_tags(family, r))
            .collect();
    }
    let mut docs = Vec::new();
    for (_, group) in &entries {
        let Some(g) = group.as_object() else { continue };
        let Some(defs) = g.get("FeatureDefinitions").and_then(Value::as_array) else {
            continue;
        };
        for def in defs {
            let Some(name) = def.get("FeatureName").and_then(Value::as_str) else {
                continue;
            };
            let mut doc = Map::new();
            for k in [
                "FeatureGroupArn",
                "FeatureGroupName",
                "CreationTime",
                "LastModifiedTime",
            ] {
                if let Some(v) = g.get(k) {
                    doc.insert(k.to_string(), v.clone());
                }
            }
            doc.insert("FeatureName".to_string(), Value::String(name.to_string()));
            if let Some(t) = def.get("FeatureType") {
                doc.insert("FeatureType".to_string(), t.clone());
            }
            if let Some(fm) = g
                .get(FEATURE_META)
                .and_then(|m| m.get(name))
                .and_then(Value::as_object)
            {
                for k in ["Description", "Parameters"] {
                    if let Some(v) = fm.get(k) {
                        doc.insert(k.to_string(), v.clone());
                    }
                }
            }
            docs.push(Value::Object(doc));
        }
    }
    docs
}

/// Resolve a dotted property path (`TrainingJobName`, `HyperParameters.lr`,
/// `Tags.env`) against a document. Under `Tags`, the whole remainder of the
/// path is the tag key (keys may themselves contain dots), looked up in the
/// `[{Key, Value}]` tag list; any other segment walks object members.
fn resolve_path<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = doc;
    let segs: Vec<&str> = path.split('.').collect();
    for (i, seg) in segs.iter().enumerate() {
        if *seg == "Tags" && i + 1 < segs.len() {
            if let Some(list) = cur.get("Tags").and_then(Value::as_array) {
                let key = segs[i + 1..].join(".");
                return list
                    .iter()
                    .find(|t| t.get("Key").and_then(Value::as_str) == Some(key.as_str()))
                    .and_then(|t| t.get("Value"));
            }
        }
        cur = cur.get(*seg)?;
    }
    Some(cur)
}

/// A stored value as a comparable number: JSON numbers directly, numeric
/// strings, and RFC3339 timestamps (as epoch seconds).
fn as_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok().or_else(|| {
            chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|d| d.timestamp_millis() as f64 / 1000.0)
        }),
        _ => None,
    }
}

fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Compare a stored value against a filter value: numerically when both read
/// as numbers / timestamps, else as strings.
fn compare(stored: &Value, filter: &str) -> Ordering {
    let fv = Value::String(filter.to_string());
    match (as_number(stored), as_number(&fv)) {
        (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
        _ => as_text(stored).as_str().cmp(filter),
    }
}

fn eval_filter(doc: &Value, filter: &Value) -> bool {
    let name = filter.get("Name").and_then(Value::as_str).unwrap_or("");
    let op = filter
        .get("Operator")
        .and_then(Value::as_str)
        .unwrap_or("Equals");
    let value = filter.get("Value").and_then(Value::as_str).unwrap_or("");
    let stored = resolve_path(doc, name).filter(|v| !v.is_null());
    match op {
        "Exists" => stored.is_some(),
        "NotExists" => stored.is_none(),
        _ => {
            let Some(stored) = stored else {
                return false;
            };
            match op {
                "Equals" => compare(stored, value) == Ordering::Equal,
                "NotEquals" => compare(stored, value) != Ordering::Equal,
                "GreaterThan" => compare(stored, value) == Ordering::Greater,
                "GreaterThanOrEqualTo" => compare(stored, value) != Ordering::Less,
                "LessThan" => compare(stored, value) == Ordering::Less,
                "LessThanOrEqualTo" => compare(stored, value) != Ordering::Greater,
                "Contains" => match stored {
                    Value::Array(items) => items.iter().any(|i| as_text(i) == value),
                    other => as_text(other).contains(value),
                },
                "In" => value
                    .split(',')
                    .map(str::trim)
                    .any(|v| compare(stored, v) == Ordering::Equal),
                _ => false,
            }
        }
    }
}

/// A nested filter matches when at least one element of the list at
/// `NestedPropertyName` satisfies every filter (filter names are given in
/// full, `<NestedPropertyName>.<member>`).
fn eval_nested(doc: &Value, nested: &Value) -> bool {
    let prefix = nested
        .get("NestedPropertyName")
        .and_then(Value::as_str)
        .unwrap_or("");
    let filters = nested
        .get("Filters")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let Some(items) = resolve_path(doc, prefix).and_then(Value::as_array) else {
        return false;
    };
    let strip = format!("{prefix}.");
    items.iter().any(|item| {
        filters.iter().all(|f| {
            let mut f = f.clone();
            if let Some(obj) = f.as_object_mut() {
                if let Some(n) = obj.get("Name").and_then(Value::as_str) {
                    let short = n.strip_prefix(&strip).unwrap_or(n).to_string();
                    obj.insert("Name".to_string(), Value::String(short));
                }
            }
            eval_filter(item, &f)
        })
    })
}

/// Evaluate a `SearchExpression`: every filter, nested filter and
/// sub-expression is a clause, combined with the expression's `Operator`
/// (`And` by default). An expression with no clauses matches everything.
fn eval_expression(doc: &Value, expr: &Value) -> bool {
    let mut clauses: Vec<bool> = Vec::new();
    for f in expr
        .get("Filters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        clauses.push(eval_filter(doc, f));
    }
    for n in expr
        .get("NestedFilters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        clauses.push(eval_nested(doc, n));
    }
    for s in expr
        .get("SubExpressions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        clauses.push(eval_expression(doc, s));
    }
    if clauses.is_empty() {
        return true;
    }
    match expr.get("Operator").and_then(Value::as_str) {
        Some("Or") => clauses.into_iter().any(|c| c),
        _ => clauses.into_iter().all(|c| c),
    }
}

/// Project a document onto a `Search` result structure: keep its modelled
/// top-level members, then complete any required member.
fn project_shape(ctx: &Ctx, meta: &OpMeta, shape: &str, doc: &Value) -> Value {
    let Some(s) = SEARCH_SHAPES.iter().find(|s| s.name == shape) else {
        return Value::Object(Map::new());
    };
    let mut out = Map::new();
    engine::project(doc.as_object(), s.members, &mut out);
    engine::fill_required(ctx, meta, "", &mut out, s.req);
    Value::Object(out)
}

fn search_record(ctx: &Ctx, meta: &OpMeta, member: &str, doc: &Value) -> Value {
    let mut rec = Map::new();
    let value = if member == "Model" {
        // `SearchRecord.Model` is a `ModelDashboardModel` wrapping the model.
        let mut dash = Map::new();
        dash.insert("Model".to_string(), project_shape(ctx, meta, "Model", doc));
        Value::Object(dash)
    } else {
        let shape = SEARCH_RECORD
            .iter()
            .find(|(m, _)| *m == member)
            .map(|(_, s)| *s)
            .unwrap_or(member);
        project_shape(ctx, meta, shape, doc)
    };
    rec.insert(member.to_string(), value);
    Value::Object(rec)
}

pub(super) fn search(
    svc: &SageMakerService,
    ctx: &Ctx,
    meta: &OpMeta,
    body: &Map<String, Value>,
) -> (AwsResponse, bool) {
    let resource_type = body.get("Resource").and_then(Value::as_str).unwrap_or("");
    let mut docs: Vec<Value> = match search_target(resource_type) {
        Some((family, _)) => {
            let g = svc.state.read();
            g.get(&ctx.account)
                .map(|d| candidates(d, resource_type, family))
                .unwrap_or_default()
        }
        None => Vec::new(),
    };

    if let Some(expr) = body.get("SearchExpression") {
        docs.retain(|d| eval_expression(d, expr));
    }
    // Visibility conditions narrow the result set like equality filters.
    for cond in body
        .get("VisibilityConditions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let key = cond.get("Key").and_then(Value::as_str).unwrap_or("");
        let value = cond.get("Value").and_then(Value::as_str).unwrap_or("");
        let filter = serde_json::json!({"Name": key, "Operator": "Equals", "Value": value});
        docs.retain(|d| eval_filter(d, &filter));
    }

    // Sort: `SortBy` property (default `CreationTime`), `SortOrder`
    // (default `Descending`). Documents missing the property sort last.
    let sort_by = body
        .get("SortBy")
        .and_then(Value::as_str)
        .unwrap_or("CreationTime")
        .to_string();
    let ascending = body.get("SortOrder").and_then(Value::as_str) == Some("Ascending");
    docs.sort_by(|a, b| {
        let (va, vb) = (resolve_path(a, &sort_by), resolve_path(b, &sort_by));
        let ord = match (va, vb) {
            (Some(x), Some(y)) => match (as_number(x), as_number(y)) {
                (Some(p), Some(q)) => p.partial_cmp(&q).unwrap_or(Ordering::Equal),
                _ => as_text(x).cmp(&as_text(y)),
            },
            (Some(_), None) => return Ordering::Less,
            (None, Some(_)) => return Ordering::Greater,
            (None, None) => Ordering::Equal,
        };
        if ascending {
            ord
        } else {
            ord.reverse()
        }
    });

    let total = docs.len();
    let page_size = body
        .get("MaxResults")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .unwrap_or(10) as usize;
    let start = body
        .get("NextToken")
        .and_then(Value::as_str)
        .and_then(|t| t.parse::<usize>().ok())
        .unwrap_or(0)
        .min(total);
    let end = (start + page_size).min(total);
    let member = search_target(resource_type).map(|(_, m)| m);
    let results: Vec<Value> = match member {
        Some(member) => docs[start..end]
            .iter()
            .map(|d| search_record(ctx, meta, member, d))
            .collect(),
        None => Vec::new(),
    };

    let mut out = Map::new();
    out.insert("Results".to_string(), Value::Array(results));
    if end < total {
        out.insert("NextToken".to_string(), Value::String(end.to_string()));
    }
    out.insert(
        "TotalHits".to_string(),
        serde_json::json!({"Value": total, "Relation": "EqualTo"}),
    );
    (ok_json(Value::Object(out)), false)
}

// ── QueryLineage ─────────────────────────────────────────────────────────

/// Lineage entity families and the `LineageType` each reports.
const LINEAGE_FAMILIES: &[(&str, &str, &str)] = &[
    ("Artifact", "Artifact", "ArtifactType"),
    ("Context", "Context", "ContextType"),
    ("Action", "Action", "ActionType"),
    ("TrialComponent", "TrialComponent", ""),
];

/// A lineage entity resolved by ARN: its record (if stored), lineage type
/// and entity type.
struct Entity {
    record: Option<Value>,
    lineage_type: Option<&'static str>,
    entity_type: Option<String>,
}

fn lineage_entity(data: &SageMakerData, arn: &str) -> Entity {
    for (family, lineage, type_member) in LINEAGE_FAMILIES {
        if let Some(key) = data.resolve_key(family, arn) {
            let record = data.get_resource(family, &key).cloned();
            let entity_type = if type_member.is_empty() {
                None
            } else {
                record
                    .as_ref()
                    .and_then(|r| r.get(*type_member))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            };
            return Entity {
                record,
                lineage_type: Some(lineage),
                entity_type,
            };
        }
    }
    // An ARN only referenced by an association: infer the lineage type from
    // its resource path.
    let resource = arn.splitn(6, ':').nth(5).unwrap_or("");
    let lineage_type = if resource.starts_with("artifact/") {
        Some("Artifact")
    } else if resource.starts_with("context/") {
        Some("Context")
    } else if resource.starts_with("action/") {
        Some("Action")
    } else if resource.starts_with("experiment-trial-component/") {
        Some("TrialComponent")
    } else {
        None
    };
    Entity {
        record: None,
        lineage_type,
        entity_type: None,
    }
}

/// Whether a reached vertex passes `QueryLineage`'s `Filters`.
fn vertex_passes(entity: &Entity, filters: Option<&Value>) -> bool {
    let Some(f) = filters else { return true };
    if let Some(types) = f.get("Types").and_then(Value::as_array) {
        if !types.is_empty()
            && !types
                .iter()
                .any(|t| t.as_str().is_some() && t.as_str() == entity.entity_type.as_deref())
        {
            return false;
        }
    }
    if let Some(types) = f.get("LineageTypes").and_then(Value::as_array) {
        if !types.is_empty()
            && !types
                .iter()
                .any(|t| t.as_str().is_some() && t.as_str() == entity.lineage_type)
        {
            return false;
        }
    }
    let rec = entity.record.as_ref();
    let time = |member: &str| rec.and_then(|r| r.get(member)).and_then(as_number);
    for (key, member, after) in [
        ("CreatedBefore", "CreationTime", false),
        ("CreatedAfter", "CreationTime", true),
        ("ModifiedBefore", "LastModifiedTime", false),
        ("ModifiedAfter", "LastModifiedTime", true),
    ] {
        if let Some(bound) = f.get(key).and_then(as_number) {
            match time(member) {
                Some(t) if (after && t > bound) || (!after && t < bound) => {}
                _ => return false,
            }
        }
    }
    if let Some(props) = f.get("Properties").and_then(Value::as_object) {
        let stored = rec
            .and_then(|r| r.get("Properties"))
            .and_then(Value::as_object);
        for (k, v) in props {
            if stored.and_then(|s| s.get(k)) != Some(v) {
                return false;
            }
        }
    }
    true
}

pub(super) fn query_lineage(
    svc: &SageMakerService,
    ctx: &Ctx,
    body: &Map<String, Value>,
) -> Result<(AwsResponse, bool), AwsServiceError> {
    let start_arns: Vec<String> = body
        .get("StartArns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    let direction = body
        .get("Direction")
        .and_then(Value::as_str)
        .unwrap_or("Ascendants");
    let include_edges = body
        .get("IncludeEdges")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let max_depth = body.get("MaxDepth").and_then(Value::as_u64).unwrap_or(10);
    let filters = body.get("Filters");

    let g = svc.state.read();
    let empty = SageMakerData::default();
    let data = g.get(&ctx.account).unwrap_or(&empty);
    let edges: Vec<(String, String, Option<Value>)> = data
        .list_resource_entries("Association")
        .into_iter()
        .filter_map(|(_, r)| {
            let s = r.get("SourceArn").and_then(Value::as_str)?.to_string();
            let d = r.get("DestinationArn").and_then(Value::as_str)?.to_string();
            Some((s, d, r.get("AssociationType").cloned()))
        })
        .collect();

    // Every start entity must exist: as a stored lineage entity or as an
    // endpoint of a stored association.
    for arn in &start_arns {
        let known = lineage_entity(data, arn).record.is_some()
            || edges.iter().any(|(s, d, _)| s == arn || d == arn);
        if !known {
            return Err(not_found(format!("Lineage entity '{arn}' does not exist.")));
        }
    }

    let follow_down = matches!(direction, "Descendants" | "Both");
    let follow_up = matches!(direction, "Ascendants" | "Both");
    let mut visited: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut used_edges: BTreeSet<usize> = BTreeSet::new();
    let mut queue: VecDeque<(String, u64)> = VecDeque::new();
    for arn in &start_arns {
        if seen.insert(arn.clone()) {
            visited.push(arn.clone());
            queue.push_back((arn.clone(), 0));
        }
    }
    while let Some((arn, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        for (i, (s, d, _)) in edges.iter().enumerate() {
            let next = if follow_down && *s == arn {
                Some(d)
            } else if follow_up && *d == arn {
                Some(s)
            } else {
                None
            };
            if let Some(next) = next {
                used_edges.insert(i);
                if seen.insert(next.clone()) {
                    visited.push(next.clone());
                    queue.push_back((next.clone(), depth + 1));
                }
            }
        }
    }

    let vertices: Vec<Value> = visited
        .iter()
        .filter_map(|arn| {
            let e = lineage_entity(data, arn);
            if !vertex_passes(&e, filters) {
                return None;
            }
            let mut v = Map::new();
            v.insert("Arn".to_string(), Value::String(arn.clone()));
            if let Some(t) = e.entity_type {
                v.insert("Type".to_string(), Value::String(t));
            }
            if let Some(l) = e.lineage_type {
                v.insert("LineageType".to_string(), Value::String(l.to_string()));
            }
            Some(Value::Object(v))
        })
        .collect();

    let total = vertices.len();
    let page_size = body
        .get("MaxResults")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .unwrap_or(10) as usize;
    let start = body
        .get("NextToken")
        .and_then(Value::as_str)
        .and_then(|t| t.parse::<usize>().ok())
        .unwrap_or(0)
        .min(total);
    let end = (start + page_size).min(total);

    let mut out = Map::new();
    out.insert(
        "Vertices".to_string(),
        Value::Array(vertices[start..end].to_vec()),
    );
    if include_edges {
        let list: Vec<Value> = used_edges
            .iter()
            .map(|i| {
                let (s, d, t) = &edges[*i];
                let mut e = Map::new();
                e.insert("SourceArn".to_string(), Value::String(s.clone()));
                e.insert("DestinationArn".to_string(), Value::String(d.clone()));
                if let Some(t) = t {
                    e.insert("AssociationType".to_string(), t.clone());
                }
                Value::Object(e)
            })
            .collect();
        out.insert("Edges".to_string(), Value::Array(list));
    }
    if end < total {
        out.insert("NextToken".to_string(), Value::String(end.to_string()));
    }
    Ok((ok_json(Value::Object(out)), false))
}
