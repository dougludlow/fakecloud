//! Account-partitioned, serializable state for the AWS IoT Core control plane.
//!
//! The registry is deliberately schema-light: every named resource family
//! (things, policies, certificates, jobs, topic rules, security profiles, ...)
//! is stored in one uniform two-level map, `resources[resource_type][id]`,
//! where the stored value is the resource's JSON record (its persisted
//! attributes plus any minted ARN / id / timestamps). Keeping every key a
//! plain `String` means the snapshot never depends on the tuple-key serde
//! adapter and new resource families need no new struct fields.
//!
//! Alongside the resource map are:
//! * `tags` — resource tags keyed by ARN.
//! * `singletons` — account-scoped singleton configurations (indexing config,
//!   event configurations, V2 logging options, audit configuration, default
//!   authorizer, encryption configuration, package configuration, the CA
//!   registration code, ...), keyed by a stable string.
//! * `relations` — many-to-many relationship sets keyed by a stable string
//!   (e.g. `thing-principals:<thing>` -> principal ARNs,
//!   `principal-policies:<principal>` -> policy names,
//!   `group-things:<group>` -> thing names). Stored as ordered vectors so
//!   list operations are deterministic.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use fakecloud_core::multi_account::{AccountState, MultiAccountState};

pub const IOT_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// Per-account IoT Core control-plane state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IotData {
    /// `resource_type -> id -> record`. The record is the resource's persisted
    /// JSON attributes plus any minted ARN / id / timestamps.
    #[serde(default)]
    pub resources: BTreeMap<String, BTreeMap<String, Value>>,
    /// Resource tags keyed by ARN.
    #[serde(default)]
    pub tags: BTreeMap<String, BTreeMap<String, String>>,
    /// Account-scoped singleton configurations keyed by a stable string.
    #[serde(default)]
    pub singletons: BTreeMap<String, Value>,
    /// Many-to-many relationship sets keyed by a stable string.
    #[serde(default)]
    pub relations: BTreeMap<String, Vec<String>>,
    /// Monotonic counter used to mint unique ids within an account.
    #[serde(default)]
    pub seq: u64,
}

impl IotData {
    /// Fetch a resource record by type + id.
    pub fn get_resource(&self, rtype: &str, id: &str) -> Option<&Value> {
        self.resources.get(rtype).and_then(|m| m.get(id))
    }

    /// Insert / replace a resource record.
    pub fn put_resource(&mut self, rtype: &str, id: &str, record: Value) {
        self.resources
            .entry(rtype.to_string())
            .or_default()
            .insert(id.to_string(), record);
    }

    /// Remove a resource record, returning it if present.
    pub fn remove_resource(&mut self, rtype: &str, id: &str) -> Option<Value> {
        let removed = self.resources.get_mut(rtype).and_then(|m| m.remove(id));
        if let Some(m) = self.resources.get(rtype) {
            if m.is_empty() {
                self.resources.remove(rtype);
            }
        }
        removed
    }

    /// All records of a type, ordered by id.
    pub fn list_resources(&self, rtype: &str) -> Vec<Value> {
        self.resources
            .get(rtype)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    /// All records of a type as `(id, record)` pairs, ordered by id. Used by
    /// list operations whose output element is a plain identifier string (the
    /// stored key) rather than a projected object.
    pub fn list_resource_entries(&self, rtype: &str) -> Vec<(String, Value)> {
        self.resources
            .get(rtype)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    /// Replace `arn`'s whole tag set (an empty set removes the entry). Tags have
    /// exactly one home, this ARN-keyed map, which TagResource / UntagResource /
    /// ListTagsForResource and every tagged `Create*` share.
    pub fn set_tags(&mut self, arn: &str, tags: BTreeMap<String, String>) {
        if tags.is_empty() {
            self.tags.remove(arn);
        } else {
            self.tags.insert(arn.to_string(), tags);
        }
    }

    /// Drop every tag stored for `arn` (the resource was deleted).
    pub fn remove_tags(&mut self, arn: &str) {
        self.tags.remove(arn);
    }

    /// One-time migration for snapshots written before tags had a single home:
    /// move each record's inline `tags` member into the ARN-keyed tag store,
    /// keyed by the record's own IoT ARN member (the `*Arn` naming this
    /// resource's primary name). Tags already in the store win on conflict.
    pub fn migrate_inline_tags(&mut self) {
        // Package versions used to be minted as `package/<ver>`, shared by every
        // package with that version name. Rewrite each stored version to its
        // real `package/<pkg>/version/<ver>` ARN first, so its inline tags move
        // under its own ARN rather than a shared one.
        if let Some(versions) = self.resources.get_mut("packages/versions") {
            for (key, rec) in versions.iter_mut() {
                let Some((pkg, ver)) = key.split_once('/') else {
                    continue;
                };
                let Some(obj) = rec.as_object_mut() else {
                    continue;
                };
                let Some(old) = obj.get("packageVersionArn").and_then(Value::as_str) else {
                    continue;
                };
                // Keep the old ARN's partition / region / account prefix.
                let parts: Vec<&str> = old.splitn(6, ':').collect();
                if parts.len() != 6 {
                    continue;
                }
                let arn = format!("{}:package/{pkg}/version/{ver}", parts[..5].join(":"));
                obj.insert("packageVersionArn".to_string(), Value::String(arn));
            }
        }
        let mut moved: Vec<(String, Value)> = Vec::new();
        for records in self.resources.values_mut() {
            for (key, rec) in records.iter_mut() {
                let Some(obj) = rec.as_object_mut() else {
                    continue;
                };
                if !obj.contains_key("tags") {
                    continue;
                }
                let primary = key.rsplit('/').next().unwrap_or(key);
                let suffix = format!("/{primary}");
                let arn = obj
                    .iter()
                    .filter(|(k, _)| k.ends_with("Arn"))
                    .filter_map(|(_, v)| v.as_str())
                    .find(|v| v.starts_with("arn:") && v.contains(":iot:") && v.ends_with(&suffix))
                    .map(str::to_string);
                if let Some(arn) = arn {
                    if let Some(tags) = obj.remove("tags") {
                        moved.push((arn, tags));
                    }
                }
            }
        }
        for (arn, tags) in moved {
            let mut set = crate::service::parse_tags(&tags);
            if let Some(existing) = self.tags.remove(&arn) {
                set.extend(existing);
            }
            self.set_tags(&arn, set);
        }
    }

    /// Next unique sequence value for id minting.
    pub fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
}

impl AccountState for IotData {
    fn new_for_account(_account_id: &str, _region: &str, _endpoint: &str) -> Self {
        Self::default()
    }
}

pub type SharedIotState = Arc<RwLock<MultiAccountState<IotData>>>;

#[derive(Debug, Serialize, Deserialize)]
pub struct IotSnapshot {
    pub schema_version: u32,
    pub accounts: MultiAccountState<IotData>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn new_account_is_empty() {
        let data = IotData::new_for_account("000000000000", "us-east-1", "");
        assert!(data.resources.is_empty());
        assert!(data.tags.is_empty());
        assert!(data.singletons.is_empty());
    }

    #[test]
    fn resource_round_trips() {
        let mut d = IotData::default();
        d.put_resource("things", "sensor-1", json!({"thingName": "sensor-1"}));
        assert_eq!(
            d.get_resource("things", "sensor-1").unwrap()["thingName"],
            "sensor-1"
        );
        assert_eq!(d.list_resources("things").len(), 1);
        assert!(d.remove_resource("things", "sensor-1").is_some());
        assert!(d.get_resource("things", "sensor-1").is_none());
        assert!(!d.resources.contains_key("things"));
    }
}
