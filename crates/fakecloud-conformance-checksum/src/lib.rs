//! Per-operation Smithy shape-tree checksum.
//!
//! This is the single implementation shared by the compile-time
//! `#[test_action(..., checksum = "...")]` proc macro
//! (`fakecloud-conformance-macros`) and the `fakecloud-conformance checksums`
//! CLI. Both used to carry their own copy and drifted apart (the CLI kept an
//! enum shape's `@length`/`@pattern` constraints next to its values, the macro
//! replaced them), so the CLI printed checksums no `#[test_action]` would
//! accept. Every annotated checksum in the conformance tests is computed by
//! this code, so its output is a compatibility contract: changing the
//! canonical form invalidates every `#[test_action]` in the repo.
//!
//! It works on the raw Smithy JSON AST (`serde_json::Value`) rather than a
//! parsed model so nothing about a richer model representation can leak into
//! the hash.

use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};

/// The parts of an operation shape that feed the checksum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationInfo {
    pub name: String,
    pub input_shape: Option<String>,
    pub output_shape: Option<String>,
    pub error_shapes: Vec<String>,
}

/// Resolve a `#[test_action]` service argument to its `aws-models/<key>.json`
/// model key using the parsed `aws-models/service-map.json`: a direct key
/// match first (`"cloudwatch"`), then a match on an entry's `service_name`
/// (`"monitoring"` -> `"cloudwatch"`).
pub fn resolve_model_key(service_map: &serde_json::Value, service: &str) -> Option<String> {
    let obj = service_map.as_object()?;
    if obj.contains_key(service) {
        return Some(service.to_string());
    }
    obj.iter()
        .find(|(key, entry)| {
            entry
                .get("service_name")
                .and_then(|v| v.as_str())
                .unwrap_or(key)
                == service
        })
        .map(|(key, _)| key.clone())
}

/// Every operation shape ID bound to the model's service, directly or through
/// (nested) resources, in model order.
pub fn operation_targets(root: &serde_json::Value) -> Vec<String> {
    let mut targets = Vec::new();
    let shapes = match root.get("shapes").and_then(|v| v.as_object()) {
        Some(s) => s,
        None => return targets,
    };
    if let Some(service) = shapes
        .values()
        .find(|def| def.get("type").and_then(|v| v.as_str()) == Some("service"))
    {
        push_targets(service.get("operations"), &mut targets);
        if let Some(resources) = service.get("resources").and_then(|v| v.as_array()) {
            for res in resources {
                if let Some(t) = res.get("target").and_then(|v| v.as_str()) {
                    collect_resource_operations(shapes, t, &mut targets);
                }
            }
        }
    }
    targets
}

fn push_targets(list: Option<&serde_json::Value>, targets: &mut Vec<String>) {
    if let Some(items) = list.and_then(|v| v.as_array()) {
        for item in items {
            if let Some(t) = item.get("target").and_then(|v| v.as_str()) {
                targets.push(t.to_string());
            }
        }
    }
}

fn collect_resource_operations(
    shapes: &serde_json::Map<String, serde_json::Value>,
    resource_target: &str,
    targets: &mut Vec<String>,
) {
    let resource_def = match shapes.get(resource_target) {
        Some(d) => d,
        None => return,
    };
    for key in &["create", "read", "update", "delete", "list", "put"] {
        if let Some(op) = resource_def
            .get(*key)
            .and_then(|v| v.get("target"))
            .and_then(|v| v.as_str())
        {
            targets.push(op.to_string());
        }
    }
    push_targets(resource_def.get("operations"), targets);
    push_targets(resource_def.get("collectionOperations"), targets);
    if let Some(resources) = resource_def.get("resources").and_then(|v| v.as_array()) {
        for res in resources {
            if let Some(t) = res.get("target").and_then(|v| v.as_str()) {
                collect_resource_operations(shapes, t, targets);
            }
        }
    }
}

/// Look up an operation bound to the model's service by its short name.
pub fn find_operation(root: &serde_json::Value, action: &str) -> Option<OperationInfo> {
    let shapes = root.get("shapes")?.as_object()?;
    let target = operation_targets(root)
        .into_iter()
        .find(|t| short_name(t) == action)?;
    let op_def = shapes.get(target.as_str())?;
    let shape_ref = |key: &str| {
        op_def
            .get(key)
            .and_then(|v| v.get("target"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };
    let error_shapes = op_def
        .get("errors")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    e.get("target")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .collect()
        })
        .unwrap_or_default();
    Some(OperationInfo {
        name: short_name(&target).to_string(),
        input_shape: shape_ref("input"),
        output_shape: shape_ref("output"),
        error_shapes,
    })
}

fn short_name(shape_id: &str) -> &str {
    shape_id.rsplit('#').next().unwrap_or(shape_id)
}

/// Checksum of `action` in the model, or `None` when the service does not
/// bind that operation.
pub fn operation_checksum(root: &serde_json::Value, action: &str) -> Option<String> {
    let op = find_operation(root, action)?;
    Some(compute_checksum(root, &op))
}

/// First 8 hex characters of the SHA-256 of [`canonical_string`].
pub fn compute_checksum(root: &serde_json::Value, op: &OperationInfo) -> String {
    let canonical = canonical_string(root, op);
    let digest = Sha256::digest(canonical.as_bytes());
    digest[..4].iter().map(|b| format!("{:02x}", b)).collect()
}

#[derive(Debug)]
struct ShapeCanonical {
    shape_type: String,
    members: BTreeMap<String, MemberCanonical>,
    constraints: String,
}

#[derive(Debug)]
struct MemberCanonical {
    target: String,
    required: bool,
}

/// The canonical text that gets hashed: the operation's input/output/error
/// references followed by every shape reachable from them, in shape-ID order.
pub fn canonical_string(root: &serde_json::Value, op: &OperationInfo) -> String {
    let empty = serde_json::Map::new();
    let shapes = root
        .get("shapes")
        .and_then(|v| v.as_object())
        .unwrap_or(&empty);
    let mut collected = BTreeMap::new();
    let mut visited = HashSet::new();

    let roots = op
        .input_shape
        .iter()
        .chain(op.output_shape.iter())
        .chain(op.error_shapes.iter());
    for id in roots {
        collect_shape_tree(shapes, id, &mut collected, &mut visited);
    }

    let mut parts = vec![format!("op:{}", op.name)];
    if let Some(ref input) = op.input_shape {
        parts.push(format!("in:{}", input));
    }
    if let Some(ref output) = op.output_shape {
        parts.push(format!("out:{}", output));
    }
    for error in &op.error_shapes {
        parts.push(format!("err:{}", error));
    }
    for (id, shape) in &collected {
        let mut shape_str = format!("shape:{}:type:{}", id, shape.shape_type);
        if !shape.constraints.is_empty() {
            shape_str.push_str(&format!(":constraints:{}", shape.constraints));
        }
        for (name, member) in &shape.members {
            shape_str.push_str(&format!(
                ":member:{}:{}:req:{}",
                name, member.target, member.required
            ));
        }
        parts.push(shape_str);
    }
    parts.join("\n")
}

fn ref_target<'a>(shape_def: &'a serde_json::Value, key: &str) -> &'a str {
    shape_def
        .get(key)
        .and_then(|v| v.get("target"))
        .and_then(|v| v.as_str())
        .unwrap_or("smithy.api#String")
}

fn collect_shape_tree(
    shapes: &serde_json::Map<String, serde_json::Value>,
    shape_id: &str,
    collected: &mut BTreeMap<String, ShapeCanonical>,
    visited: &mut HashSet<String>,
) {
    if !visited.insert(shape_id.to_string()) {
        return;
    }

    // Prelude shapes are recorded by name only.
    if shape_id.starts_with("smithy.api#") {
        collected.insert(
            shape_id.to_string(),
            ShapeCanonical {
                shape_type: short_name(shape_id).to_string(),
                members: BTreeMap::new(),
                constraints: String::new(),
            },
        );
        return;
    }

    let shape_def = match shapes.get(shape_id) {
        Some(s) => s,
        None => return,
    };
    let type_str = shape_def
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let raw_traits = shape_def.get("traits").and_then(|v| v.as_object());

    let shape_type = match type_str {
        "list" => format!("list<{}>", ref_target(shape_def, "member")),
        "map" => format!(
            "map<{},{}>",
            ref_target(shape_def, "key"),
            ref_target(shape_def, "value")
        ),
        other => other.to_string(),
    };
    let mut canonical = ShapeCanonical {
        shape_type,
        members: BTreeMap::new(),
        constraints: format_constraints(raw_traits),
    };

    match type_str {
        "structure" | "union" => {
            if let Some(members) = shape_def.get("members").and_then(|v| v.as_object()) {
                for (name, member_def) in members {
                    let target = member_def
                        .get("target")
                        .and_then(|v| v.as_str())
                        .unwrap_or("smithy.api#String")
                        .to_string();
                    let required = member_def
                        .get("traits")
                        .and_then(|v| v.as_object())
                        .is_some_and(|t| t.contains_key("smithy.api#required"));
                    collect_shape_tree(shapes, &target, collected, visited);
                    canonical
                        .members
                        .insert(name.clone(), MemberCanonical { target, required });
                }
            }
        }
        "list" => {
            collect_shape_tree(shapes, ref_target(shape_def, "member"), collected, visited);
        }
        "map" => {
            collect_shape_tree(shapes, ref_target(shape_def, "key"), collected, visited);
            collect_shape_tree(shapes, ref_target(shape_def, "value"), collected, visited);
        }
        // An enumerated string's value list REPLACES its length/pattern
        // constraints in the canonical form rather than being appended to
        // them. That is lossy, but it is what every recorded `#[test_action]`
        // checksum was computed with, so it is preserved deliberately.
        "string" => {
            if let Some(enum_vals) = raw_traits
                .and_then(|t| t.get("smithy.api#enum"))
                .and_then(|v| v.as_array())
            {
                let vals: Vec<&str> = enum_vals
                    .iter()
                    .filter_map(|e| e.get("value").and_then(|v| v.as_str()))
                    .collect();
                canonical.constraints = format!("enum:{}", vals.join(","));
            }
        }
        "enum" => {
            if let Some(members) = shape_def.get("members").and_then(|v| v.as_object()) {
                // Values are ordered by member name. Sort explicitly so the
                // result does not depend on whether serde_json's
                // `preserve_order` feature is unified into this build.
                let mut named: Vec<(&String, &str)> = members
                    .iter()
                    .map(|(name, member_def)| {
                        let value = member_def
                            .get("traits")
                            .and_then(|t| t.get("smithy.api#enumValue"))
                            .and_then(|v| v.as_str())
                            .unwrap_or(name);
                        (name, value)
                    })
                    .collect();
                named.sort_by(|a, b| a.0.cmp(b.0));
                let vals: Vec<&str> = named.into_iter().map(|(_, v)| v).collect();
                canonical.constraints = format!("enum:{}", vals.join(","));
            }
        }
        _ => {}
    }

    collected.insert(shape_id.to_string(), canonical);
}

fn format_constraints(raw_traits: Option<&serde_json::Map<String, serde_json::Value>>) -> String {
    let raw = match raw_traits {
        Some(t) => t,
        None => return String::new(),
    };
    let mut parts = Vec::new();
    if let Some(length) = raw.get("smithy.api#length") {
        if let Some(min) = length.get("min").and_then(|v| v.as_u64()) {
            parts.push(format!("len_min:{}", min));
        }
        if let Some(max) = length.get("max").and_then(|v| v.as_u64()) {
            parts.push(format!("len_max:{}", max));
        }
    }
    if let Some(range) = raw.get("smithy.api#range") {
        if let Some(min) = range.get("min").and_then(|v| v.as_f64()) {
            parts.push(format!("range_min:{}", min));
        }
        if let Some(max) = range.get("max").and_then(|v| v.as_f64()) {
            parts.push(format!("range_max:{}", max));
        }
    }
    if let Some(pattern) = raw.get("smithy.api#pattern").and_then(|v| v.as_str()) {
        parts.push(format!("pattern:{}", pattern));
    }
    parts.join(";")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model(extra: serde_json::Value) -> serde_json::Value {
        let mut shapes = json!({
            "ex#Svc": {"type": "service", "operations": [{"target": "ex#Op"}]},
            "ex#Op": {
                "type": "operation",
                "input": {"target": "ex#OpInput"},
                "output": {"target": "ex#OpOutput"},
            },
            "ex#OpOutput": {"type": "structure", "members": {}},
        });
        for (k, v) in extra.as_object().unwrap() {
            shapes[k] = v.clone();
        }
        json!({"smithy": "2.0", "shapes": shapes})
    }

    fn op() -> OperationInfo {
        OperationInfo {
            name: "Op".into(),
            input_shape: Some("ex#OpInput".into()),
            output_shape: Some("ex#OpOutput".into()),
            error_shapes: vec![],
        }
    }

    #[test]
    fn enum_values_replace_length_and_pattern_constraints() {
        let root = model(json!({
            "ex#OpInput": {"type": "structure", "members": {
                "Kind": {"target": "ex#Kind", "traits": {"smithy.api#required": {}}},
            }},
            "ex#Kind": {
                "type": "enum",
                "traits": {"smithy.api#length": {"min": 1, "max": 64}, "smithy.api#pattern": "^.*$"},
                "members": {
                    "B": {"target": "smithy.api#Unit", "traits": {"smithy.api#enumValue": "b"}},
                    "A": {"target": "smithy.api#Unit", "traits": {"smithy.api#enumValue": "a"}},
                },
            },
        }));
        let canonical = canonical_string(&root, &op());
        assert!(
            canonical.contains("shape:ex#Kind:type:enum:constraints:enum:a,b\n"),
            "{canonical}"
        );
        assert!(canonical.contains(":member:Kind:ex#Kind:req:true"));
    }

    #[test]
    fn operation_lookup_follows_nested_resources() {
        let root = json!({"smithy": "2.0", "shapes": {
            "ex#Svc": {"type": "service", "resources": [{"target": "ex#Outer"}]},
            "ex#Outer": {"type": "resource", "read": {"target": "ex#GetOuter"},
                         "resources": [{"target": "ex#Inner"}]},
            "ex#Inner": {"type": "resource", "collectionOperations": [{"target": "ex#ListInner"}]},
            "ex#GetOuter": {"type": "operation"},
            "ex#ListInner": {"type": "operation", "errors": [{"target": "ex#Boom"}]},
        }});
        assert_eq!(
            operation_targets(&root),
            vec!["ex#GetOuter".to_string(), "ex#ListInner".to_string()]
        );
        let op = find_operation(&root, "ListInner").unwrap();
        assert_eq!(op.error_shapes, vec!["ex#Boom".to_string()]);
        assert!(operation_checksum(&root, "Missing").is_none());
    }

    #[test]
    fn service_name_resolves_to_model_key() {
        let map = json!({
            "cloudwatch": {"service_name": "monitoring", "repo_dir": "x"},
            "sqs": {"service_name": "sqs", "repo_dir": "y"},
        });
        assert_eq!(resolve_model_key(&map, "sqs").as_deref(), Some("sqs"));
        assert_eq!(
            resolve_model_key(&map, "monitoring").as_deref(),
            Some("cloudwatch")
        );
        assert_eq!(resolve_model_key(&map, "nope"), None);
    }
}
