//! Account-partitioned, serializable state for Amazon Pinpoint (`pinpoint`).
//!
//! Every resource is stored under its owning application (or, for the global
//! families, at the top level) as a plain JSON `Value` record keyed by a
//! `String` id — so the snapshot never depends on the tuple-key serde adapter.
//! The handlers project the exact model output shape out of these records on
//! read.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use fakecloud_core::multi_account::{AccountState, MultiAccountState};

pub const PINPOINT_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// A versioned resource (campaign / segment): the current record plus every
/// historical version. The 1-based version number indexes `versions`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Versioned {
    /// The current (latest) version's record.
    pub current: Value,
    /// All version records, oldest first (index + 1 == `Version`).
    pub versions: Vec<Value>,
}

/// A versioned message template (email / push / sms / voice / inapp).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Template {
    /// `EMAIL` / `SMS` / `VOICE` / `PUSH` / `INAPP`.
    pub template_type: String,
    /// All version records, oldest first (index + 1 == version number).
    pub versions: Vec<Value>,
    /// The active version number, as a string (Pinpoint versions are strings).
    pub active_version: String,
}

/// Per-application Pinpoint state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct App {
    /// The `ApplicationResponse` core record (`Id`, `Arn`, `Name`, `tags`,
    /// `CreationDate`).
    pub record: Value,
    /// The `ApplicationSettingsResource` projection.
    #[serde(default)]
    pub settings: Value,
    /// Campaigns keyed by campaign id.
    #[serde(default)]
    pub campaigns: BTreeMap<String, Versioned>,
    /// Segments keyed by segment id.
    #[serde(default)]
    pub segments: BTreeMap<String, Versioned>,
    /// Journeys keyed by journey id.
    #[serde(default)]
    pub journeys: BTreeMap<String, Value>,
    /// Endpoints keyed by endpoint id.
    #[serde(default)]
    pub endpoints: BTreeMap<String, Value>,
    /// Channels keyed by canonical channel key (`adm`, `apns`, `sms`, ...).
    #[serde(default)]
    pub channels: BTreeMap<String, Value>,
    /// Import jobs keyed by job id.
    #[serde(default)]
    pub import_jobs: BTreeMap<String, Value>,
    /// Export jobs keyed by job id.
    #[serde(default)]
    pub export_jobs: BTreeMap<String, Value>,
    /// The single event stream, if configured.
    #[serde(default)]
    pub event_stream: Option<Value>,
}

/// Per-account Pinpoint state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PinpointData {
    /// Applications keyed by application id.
    #[serde(default)]
    pub apps: BTreeMap<String, App>,
    /// Message templates keyed by template name (global, not app-scoped).
    #[serde(default)]
    pub templates: BTreeMap<String, Template>,
    /// Recommender configurations keyed by recommender id (global).
    #[serde(default)]
    pub recommenders: BTreeMap<String, Value>,
    /// Tag sets keyed by resource ARN.
    #[serde(default)]
    pub tags: BTreeMap<String, BTreeMap<String, String>>,
}

impl PinpointData {
    /// Replace `arn`'s whole tag set with a request's `tags` map (an absent or
    /// empty map clears it). Tags have exactly one home, this ARN-keyed map,
    /// which TagResource / UntagResource / ListTagsForResource and every
    /// tagged create share.
    pub fn set_tags(&mut self, arn: &str, tags: Option<&Value>) {
        let set: BTreeMap<String, String> = tags
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                    .collect()
            })
            .unwrap_or_default();
        if set.is_empty() {
            self.tags.remove(arn);
        } else {
            self.tags.insert(arn.to_string(), set);
        }
    }

    /// Drop every tag stored for `arn` (the resource was deleted).
    pub fn remove_tags(&mut self, arn: &str) {
        self.tags.remove(arn);
    }

    /// A response view of a stored record with its `tags` rendered from the
    /// tag store under the record's own `Arn` member.
    pub fn render(&self, record: &Value) -> Value {
        match record.get("Arn").and_then(Value::as_str) {
            Some(arn) => self.render_as(record, arn),
            None => record.clone(),
        }
    }

    /// A response view of a stored record with its `tags` rendered from the
    /// tag store under `arn` (for resources whose response carries no `Arn`,
    /// such as journeys). Any inline copy is replaced.
    pub fn render_as(&self, record: &Value, arn: &str) -> Value {
        let mut out = record.clone();
        if let Some(obj) = out.as_object_mut() {
            obj.remove("tags");
            if let Some(set) = self.tags.get(arn).filter(|s| !s.is_empty()) {
                obj.insert("tags".to_string(), serde_json::json!(set));
            }
        }
        out
    }

    /// One-time migration for snapshots written before tags had a single home:
    /// move inline `tags` off app / campaign / segment / template records into
    /// the tag store (tags already in the store win on conflict).
    pub fn migrate_inline_tags(&mut self) {
        let mut moved: Vec<(String, Value)> = Vec::new();
        let mut take = |rec: &mut Value| {
            let Some(obj) = rec.as_object_mut() else {
                return;
            };
            let Some(arn) = obj.get("Arn").and_then(Value::as_str).map(str::to_string) else {
                return;
            };
            if let Some(tags) = obj.remove("tags") {
                moved.push((arn, tags));
            }
        };
        for app in self.apps.values_mut() {
            take(&mut app.record);
            for v in app.campaigns.values_mut().chain(app.segments.values_mut()) {
                take(&mut v.current);
                for ver in v.versions.iter_mut() {
                    take(ver);
                }
            }
        }
        for t in self.templates.values_mut() {
            for ver in t.versions.iter_mut() {
                take(ver);
            }
        }
        for (arn, tags) in moved {
            let existing = self.tags.remove(&arn).unwrap_or_default();
            self.set_tags(&arn, Some(&tags));
            if !existing.is_empty() {
                self.tags.entry(arn).or_default().extend(existing);
            }
        }
    }
}

impl AccountState for PinpointData {
    fn new_for_account(_account_id: &str, _region: &str, _endpoint: &str) -> Self {
        Self::default()
    }
}

pub type SharedPinpointState = Arc<RwLock<MultiAccountState<PinpointData>>>;

#[derive(Debug, Serialize, Deserialize)]
pub struct PinpointSnapshot {
    pub schema_version: u32,
    pub accounts: MultiAccountState<PinpointData>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_account_is_empty() {
        let data = PinpointData::new_for_account("000000000000", "us-east-1", "");
        assert!(data.apps.is_empty());
        assert!(data.templates.is_empty());
        assert!(data.recommenders.is_empty());
        assert!(data.tags.is_empty());
    }
}
