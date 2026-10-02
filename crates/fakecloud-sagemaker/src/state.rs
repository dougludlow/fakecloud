//! Account-partitioned, serializable state for the Amazon SageMaker control plane.
//!
//! The registry is deliberately schema-light: every named resource family
//! (models, endpoints, endpoint configs, training jobs, notebook instances,
//! pipelines, ...) is stored in one uniform two-level map,
//! `resources[family][id]`, where the stored value is the resource's JSON
//! record (its persisted attributes plus any minted ARN / id / timestamps).
//! Keeping every key a plain `String` means the snapshot never depends on the
//! tuple-key serde adapter and new resource families need no new struct fields.
//!
//! Alongside the resource map are:
//! * `tags` — resource tags keyed by ARN.
//! * `singletons` — account-scoped singleton values keyed by a stable string
//!   (e.g. the Service Catalog portfolio status).

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use fakecloud_core::multi_account::{AccountState, MultiAccountState};

pub const SAGEMAKER_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// Per-account SageMaker control-plane state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SageMakerData {
    /// `family -> id -> record`. The record is the resource's persisted JSON
    /// attributes plus any minted ARN / id / timestamps.
    #[serde(default)]
    pub resources: BTreeMap<String, BTreeMap<String, Value>>,
    /// Resource tags keyed by ARN.
    #[serde(default)]
    pub tags: BTreeMap<String, BTreeMap<String, String>>,
    /// Account-scoped singleton values keyed by a stable string.
    #[serde(default)]
    pub singletons: BTreeMap<String, Value>,
    /// Monotonic counter used to mint unique ids within an account.
    #[serde(default)]
    pub seq: u64,
}

impl SageMakerData {
    /// Fetch a resource record by family + id.
    pub fn get_resource(&self, family: &str, id: &str) -> Option<&Value> {
        self.resources.get(family).and_then(|m| m.get(id))
    }

    /// Fetch a mutable resource record by family + id.
    pub fn get_resource_mut(&mut self, family: &str, id: &str) -> Option<&mut Value> {
        self.resources.get_mut(family).and_then(|m| m.get_mut(id))
    }

    /// Insert / replace a resource record.
    ///
    /// Tags have exactly one home: the ARN-keyed [`Self::tags`] map that
    /// `AddTags` / `ListTags` / `DeleteTags` operate on. A record that arrives
    /// carrying a `Tags` list (a tagged `Create*`, or a CloudFormation resource
    /// with a `Tags` property) has that list moved into the tag store under the
    /// record's `{Family}Arn`, *replacing* any prior tag set for that ARN, and
    /// the list is not kept inline. Reads re-attach the tags through
    /// [`Self::record_with_tags`], so Describe, List and ListTags all see the
    /// same set.
    pub fn put_resource(&mut self, family: &str, id: &str, mut record: Value) {
        if let Some(obj) = record.as_object_mut() {
            self.absorb_inline_tags(family, obj, true);
        }
        self.resources
            .entry(family.to_string())
            .or_default()
            .insert(id.to_string(), record);
    }

    /// Move a record's inline `Tags` list (`[{Key, Value}]`) into the ARN-keyed
    /// tag store. With `replace`, the list becomes the ARN's entire tag set;
    /// otherwise its entries are merged over the existing set. A record without
    /// a `{Family}Arn` keeps its list inline (there is no ARN to key it by).
    fn absorb_inline_tags(&mut self, family: &str, obj: &mut Map<String, Value>, replace: bool) {
        let Some(arn) = record_arn(family, obj) else {
            return;
        };
        let Some(Value::Array(list)) = obj.remove("Tags") else {
            return;
        };
        self.apply_tag_list(&arn, &list, replace);
    }

    /// Apply a `[{Key, Value}]` tag list to `arn`'s tag set, either replacing
    /// it or merging over it. An emptied set is removed entirely.
    pub fn apply_tag_list(&mut self, arn: &str, list: &[Value], replace: bool) {
        let mut set = if replace {
            BTreeMap::new()
        } else {
            self.tags.remove(arn).unwrap_or_default()
        };
        for t in list {
            if let Some(key) = t.get("Key").and_then(Value::as_str) {
                let val = t.get("Value").and_then(Value::as_str).unwrap_or_default();
                set.insert(key.to_string(), val.to_string());
            }
        }
        if set.is_empty() {
            self.tags.remove(arn);
        } else {
            self.tags.insert(arn.to_string(), set);
        }
    }

    /// The tag set stored for `arn` as an AWS `[{Key, Value}]` list.
    pub fn tag_list(&self, arn: &str) -> Vec<Value> {
        self.tags
            .get(arn)
            .map(|set| {
                set.iter()
                    .map(|(k, v)| serde_json::json!({"Key": k, "Value": v}))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A read view of a stored record with its tags (from the ARN-keyed tag
    /// store) re-attached as the `Tags` member, so Describe / List / Search
    /// outputs that model `Tags` reflect `AddTags` / `DeleteTags`. Records with
    /// no tags are returned unchanged.
    pub fn record_with_tags(&self, family: &str, record: &Value) -> Value {
        let mut out = record.clone();
        if let Some(obj) = out.as_object_mut() {
            if let Some(arn) = record_arn(family, obj) {
                let tags = self.tag_list(&arn);
                if !tags.is_empty() {
                    obj.insert("Tags".to_string(), Value::Array(tags));
                }
            }
        }
        out
    }

    /// Remove a resource record, returning it if present. The resource's tags
    /// go with it, so a same-name re-create starts untagged.
    pub fn remove_resource(&mut self, family: &str, id: &str) -> Option<Value> {
        let removed = self.resources.get_mut(family).and_then(|m| m.remove(id));
        if let Some(m) = self.resources.get(family) {
            if m.is_empty() {
                self.resources.remove(family);
            }
        }
        if let Some(arn) = removed
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|o| record_arn(family, o))
        {
            self.tags.remove(&arn);
        }
        removed
    }

    /// One-time migration for snapshots written before tags had a single home:
    /// move every record's inline `Tags` list into the ARN-keyed tag store
    /// (merged under any tags already stored for that ARN, which are newer).
    pub fn migrate_inline_tags(&mut self) {
        let mut moved: Vec<(String, Vec<Value>)> = Vec::new();
        for (family, records) in self.resources.iter_mut() {
            for rec in records.values_mut() {
                let Some(obj) = rec.as_object_mut() else {
                    continue;
                };
                let Some(arn) = record_arn(family, obj) else {
                    continue;
                };
                if let Some(Value::Array(list)) = obj.remove("Tags") {
                    moved.push((arn, list));
                }
            }
        }
        for (arn, list) in moved {
            let existing = self.tags.remove(&arn).unwrap_or_default();
            self.apply_tag_list(&arn, &list, true);
            if !existing.is_empty() {
                self.tags.entry(arn).or_default().extend(existing);
            }
        }
    }

    /// Resolve a caller-supplied identifier value to a stored key within a
    /// family. Matches the direct storage key first, then falls back to a record
    /// whose *canonical* identifier member — `{Family}Name`, `{Family}Id` or
    /// `{Family}Arn` — equals the value (so a resource keyed by its Name can
    /// still be described by the minted Id or ARN the create returned).
    ///
    /// The fallback is restricted to the family's own canonical members rather
    /// than any suffix-matching `*Name` / `*Id` / `*Arn` member, so an unrelated
    /// member that happens to carry an equal value (e.g. a shared `RoleArn`, or a
    /// cross-referenced `SourceArn`) on a sibling record cannot mis-resolve to
    /// the wrong record.
    pub fn resolve_key(&self, family: &str, value: &str) -> Option<String> {
        let m = self.resources.get(family)?;
        if m.contains_key(value) {
            return Some(value.to_string());
        }
        let canonical = [
            format!("{family}Name"),
            format!("{family}Id"),
            format!("{family}Arn"),
        ];
        for (k, rec) in m {
            if let Some(obj) = rec.as_object() {
                if canonical
                    .iter()
                    .any(|cand| obj.get(cand).and_then(Value::as_str) == Some(value))
                {
                    return Some(k.clone());
                }
            }
        }
        None
    }

    /// All records of a family as `(id, record)` pairs, ordered by id.
    pub fn list_resource_entries(&self, family: &str) -> Vec<(String, Value)> {
        self.resources
            .get(family)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    /// Next unique sequence value for id minting.
    pub fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
}

/// The resource ARN a stored record is tagged under: its canonical
/// `{Family}Arn` member.
fn record_arn(family: &str, obj: &Map<String, Value>) -> Option<String> {
    obj.get(&format!("{family}Arn"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl AccountState for SageMakerData {
    fn new_for_account(_account_id: &str, _region: &str, _endpoint: &str) -> Self {
        Self::default()
    }
}

pub type SharedSageMakerState = Arc<RwLock<MultiAccountState<SageMakerData>>>;

#[derive(Debug, Serialize, Deserialize)]
pub struct SageMakerSnapshot {
    pub schema_version: u32,
    pub accounts: MultiAccountState<SageMakerData>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn new_account_is_empty() {
        let data = SageMakerData::new_for_account("000000000000", "us-east-1", "");
        assert!(data.resources.is_empty());
        assert!(data.tags.is_empty());
    }

    #[test]
    fn resource_round_trips() {
        let mut d = SageMakerData::default();
        d.put_resource(
            "Model",
            "m1",
            json!({"ModelName": "m1", "ModelArn": "arn:aws:sagemaker:us-east-1:0:model/m1"}),
        );
        assert_eq!(d.get_resource("Model", "m1").unwrap()["ModelName"], "m1");
        assert_eq!(d.resolve_key("Model", "m1").as_deref(), Some("m1"));
        // resolve by the minted ARN
        assert_eq!(
            d.resolve_key("Model", "arn:aws:sagemaker:us-east-1:0:model/m1")
                .as_deref(),
            Some("m1")
        );
        assert!(d.remove_resource("Model", "m1").is_some());
        assert!(d.get_resource("Model", "m1").is_none());
    }

    #[test]
    fn resolve_key_ignores_noncanonical_sibling_member() {
        let mut d = SageMakerData::default();
        let shared = "arn:aws:iam::0:role/shared";
        // A record keyed by its name, carrying an unrelated RoleArn member.
        d.put_resource("Model", "m1", json!({"ModelName": "m1", "RoleArn": shared}));
        // Resolving by the shared RoleArn must NOT mis-match m1: RoleArn is not a
        // canonical Model identifier member.
        assert_eq!(d.resolve_key("Model", shared), None);
        // The canonical ModelName still resolves.
        assert_eq!(d.resolve_key("Model", "m1").as_deref(), Some("m1"));
    }
}
