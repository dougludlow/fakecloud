//! Account-partitioned, serializable state for AWS AppSync (`appsync`).
//!
//! Every resource is stored as its already-output-valid wire JSON object so
//! reads echo exactly what writes persisted. All map keys are plain `String`s
//! (apiId, resource name, ARN, association id), so the snapshot never depends on
//! the tuple-key serde adapter that has silently broken snapshot serialization
//! on other services. Sub-resources keyed by a compound `String` (e.g. a
//! resolver's `typeName::fieldName`) reuse the `::` delimiter, which the
//! `ResourceName` grammar (`[_A-Za-z][_0-9A-Za-z]*`) can never contain.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use fakecloud_core::multi_account::{AccountState, MultiAccountState};
use serde_json::Value;

pub const APPSYNC_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// A stored GraphQL-schema document + its async creation status.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SchemaState {
    /// The raw SDL schema document ingested by `StartSchemaCreation`.
    #[serde(default)]
    pub definition: String,
    /// Current `SchemaStatus` (`Processing` until the first status read
    /// settles it to `Success`), matching the async lifecycle of other ops.
    #[serde(default)]
    pub status: String,
    /// Human-readable status details.
    #[serde(default)]
    pub details: String,
}

/// Per-account AWS AppSync state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppSyncData {
    /// GraphQL APIs keyed by `apiId`, stored as their `GraphqlApi` wire object.
    #[serde(default)]
    pub graphql_apis: BTreeMap<String, Value>,
    /// Schema state keyed by `apiId`.
    #[serde(default)]
    pub schemas: BTreeMap<String, SchemaState>,
    /// API keys keyed by `apiId` -> `id` -> `ApiKey` wire object.
    #[serde(default)]
    pub api_keys: BTreeMap<String, BTreeMap<String, Value>>,
    /// Data sources keyed by `apiId` -> `name` -> `DataSource` wire object.
    #[serde(default)]
    pub data_sources: BTreeMap<String, BTreeMap<String, Value>>,
    /// Resolvers keyed by `apiId` -> `typeName::fieldName` -> `Resolver`.
    #[serde(default)]
    pub resolvers: BTreeMap<String, BTreeMap<String, Value>>,
    /// Functions keyed by `apiId` -> `functionId` -> `FunctionConfiguration`.
    #[serde(default)]
    pub functions: BTreeMap<String, BTreeMap<String, Value>>,
    /// Schema types keyed by `apiId` -> `typeName` -> `Type` wire object.
    #[serde(default)]
    pub types: BTreeMap<String, BTreeMap<String, Value>>,
    /// API caches keyed by `apiId`, stored as their `ApiCache` wire object.
    #[serde(default)]
    pub api_caches: BTreeMap<String, Value>,
    /// GraphQL-API environment variables keyed by `apiId`.
    #[serde(default)]
    pub env_vars: BTreeMap<String, BTreeMap<String, String>>,
    /// Custom domain names keyed by `domainName` -> `DomainNameConfig`.
    #[serde(default)]
    pub domain_names: BTreeMap<String, Value>,
    /// Domain-name -> `ApiAssociation` links (`AssociateApi`).
    #[serde(default)]
    pub api_associations: BTreeMap<String, Value>,
    /// Event APIs keyed by `apiId`, stored as their `Api` wire object.
    #[serde(default)]
    pub apis: BTreeMap<String, Value>,
    /// Channel namespaces keyed by event-`apiId` -> `name` -> `ChannelNamespace`.
    #[serde(default)]
    pub channel_namespaces: BTreeMap<String, BTreeMap<String, Value>>,
    /// Source-API associations keyed by `associationId` -> `SourceApiAssociation`.
    #[serde(default)]
    pub source_api_associations: BTreeMap<String, Value>,
    /// Types merged into a merged API by `StartSchemaMerge`, keyed by
    /// `associationId` -> `typeName` -> `Type` wire object. Populated when a
    /// schema merge copies the source API's types so `ListTypesByAssociation`
    /// can return them instead of a constant empty list.
    #[serde(default)]
    pub association_types: BTreeMap<String, BTreeMap<String, Value>>,
    /// Data-source introspection jobs keyed by `introspectionId`.
    #[serde(default)]
    pub introspections: BTreeMap<String, Value>,
    /// Tags keyed by resource ARN.
    #[serde(default)]
    pub tags: BTreeMap<String, BTreeMap<String, String>>,
}

/// Current time as an RFC3339 string (AppSync's timestamp members serialise as
/// epoch-seconds on the restJson1 wire, but stored objects only need to be
/// self-consistent; epoch floats are used where the model declares a timestamp).
pub fn now_epoch() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

impl AppSyncData {
    /// Replace `arn`'s whole tag set with a request's `tags` map (an absent or
    /// empty map clears it). Tags have exactly one home, this ARN-keyed map,
    /// which TagResource / UntagResource / ListTagsForResource and every tagged
    /// create share.
    pub fn set_tags(&mut self, arn: &str, tags: Option<&Value>) {
        let set: BTreeMap<String, String> = tags
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
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

    /// A response view of a stored resource object with its `tags` rendered
    /// from the tag store under the ARN held in its `arn_member`.
    pub fn render(&self, record: &Value, arn_member: &str) -> Value {
        let mut out = record.clone();
        if let Some(obj) = out.as_object_mut() {
            obj.remove("tags");
            let arn = obj
                .get(arn_member)
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(set) = arn
                .and_then(|a| self.tags.get(&a))
                .filter(|s| !s.is_empty())
            {
                obj.insert("tags".to_string(), serde_json::json!(set));
            }
        }
        out
    }

    /// One-time migration for snapshots written before tags had a single home:
    /// move inline `tags` off GraphQL API / Event API / channel namespace /
    /// domain name objects into the tag store (stored tags win on conflict).
    pub fn migrate_inline_tags(&mut self) {
        let mut moved: Vec<(String, Value)> = Vec::new();
        let mut take = |rec: &mut Value, arn_member: &str| {
            let Some(obj) = rec.as_object_mut() else {
                return;
            };
            let Some(arn) = obj
                .get(arn_member)
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                return;
            };
            if let Some(tags) = obj.remove("tags") {
                moved.push((arn, tags));
            }
        };
        for api in self.graphql_apis.values_mut() {
            take(api, "arn");
        }
        for api in self.apis.values_mut() {
            take(api, "apiArn");
        }
        for ns in self
            .channel_namespaces
            .values_mut()
            .flat_map(|m| m.values_mut())
        {
            take(ns, "channelNamespaceArn");
        }
        for d in self.domain_names.values_mut() {
            take(d, "domainNameArn");
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

impl AccountState for AppSyncData {
    fn new_for_account(_account_id: &str, _region: &str, _endpoint: &str) -> Self {
        Self::default()
    }
}

pub type SharedAppSyncState = Arc<RwLock<MultiAccountState<AppSyncData>>>;

#[derive(Debug, Serialize, Deserialize)]
pub struct AppSyncSnapshot {
    pub schema_version: u32,
    pub accounts: MultiAccountState<AppSyncData>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_account_is_empty() {
        let data = AppSyncData::new_for_account("000000000000", "us-east-1", "");
        assert!(data.graphql_apis.is_empty());
        assert!(data.tags.is_empty());
    }
}
