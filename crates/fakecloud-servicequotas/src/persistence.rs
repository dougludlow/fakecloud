//! Snapshot save/load for Service Quotas state.

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::Mutex as AsyncMutex;

use fakecloud_core::multi_account::MultiAccountState;

use fakecloud_persistence::SnapshotStore;

use crate::catalog;
use crate::state::{
    ServiceQuotasData, ServiceQuotasSnapshot, SharedServiceQuotasState,
    SERVICEQUOTAS_SNAPSHOT_SCHEMA_VERSION,
};

#[derive(Debug, PartialEq, Eq)]
pub enum LoadOutcome {
    Empty,
    Loaded(usize),
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("failed to read servicequotas persistence snapshot: {0}")]
    Io(String),
    #[error("failed to parse servicequotas persistence snapshot: {0}")]
    Parse(String),
    #[error(
        "servicequotas persistence schema too new: on-disk={on_disk}, max supported={supported}"
    )]
    SchemaTooNew { on_disk: u32, supported: u32 },
}

pub fn load_into(
    store: &dyn SnapshotStore,
    state: &SharedServiceQuotasState,
) -> Result<LoadOutcome, LoadError> {
    let Some(bytes) = store.load().map_err(|e| LoadError::Io(e.to_string()))? else {
        return Ok(LoadOutcome::Empty);
    };
    let snapshot: ServiceQuotasSnapshot =
        serde_json::from_slice(&bytes).map_err(|e| LoadError::Parse(e.to_string()))?;
    if snapshot.schema_version > SERVICEQUOTAS_SNAPSHOT_SCHEMA_VERSION {
        return Err(LoadError::SchemaTooNew {
            on_disk: snapshot.schema_version,
            supported: SERVICEQUOTAS_SNAPSHOT_SCHEMA_VERSION,
        });
    }
    let mut accounts = snapshot.accounts;
    let default_region = state.read().region().to_string();
    migrate_quota_scopes(&mut accounts, &default_region);
    let count = accounts.account_count();
    *state.write() = accounts;
    Ok(LoadOutcome::Loaded(count))
}

/// Re-key state stored under a quota scope the catalog no longer has.
///
/// Applied values, tags and requests of a global quota carry no region; those
/// of a regional quota carry one. A snapshot written while the catalog had a
/// quota in the other scope (schema 1 had `s3`/`L-DC2B2D3D` regional, for
/// example) would otherwise be invisible. For a quota that is now global,
/// regional applied values collapse to one (the highest), tags of every
/// regional ARN merge onto the global ARN, and requests lose their region.
/// For a quota that is now regional, a global entry moves to
/// `default_region`. Entries of quotas the catalog does not know are left as
/// they are. Idempotent, so it runs on every load.
pub fn migrate_quota_scopes(
    accounts: &mut MultiAccountState<ServiceQuotasData>,
    default_region: &str,
) {
    for (_, data) in accounts.iter_mut() {
        migrate_account(data, default_region);
    }
}

/// The region a quota's state belongs in, if the catalog knows the quota.
fn target_region<'a>(
    service_code: &str,
    quota_code: &str,
    stored: &'a str,
    default_region: &'a str,
) -> Option<&'a str> {
    let def = catalog::quota(service_code, quota_code)?;
    Some(match (def.global, stored.is_empty()) {
        (true, _) => "",
        (false, true) => default_region,
        (false, false) => stored,
    })
}

/// `arn:<partition>:servicequotas:<region>:<account>:<service>/<quota>` with
/// its region replaced, plus the service and quota codes. `None` for anything
/// else.
fn rescope_arn(arn: &str, default_region: &str) -> Option<String> {
    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    let [prefix, partition, service, region, account, resource] = parts.as_slice() else {
        return None;
    };
    if *prefix != "arn" || *service != "servicequotas" {
        return None;
    }
    let (svc, code) = resource.split_once('/')?;
    let region = target_region(svc, code, region, default_region)?;
    Some(format!(
        "arn:{partition}:servicequotas:{region}:{account}:{resource}"
    ))
}

fn migrate_account(data: &mut ServiceQuotasData, default_region: &str) {
    if data.applied.is_empty() && data.tags.is_empty() && data.requests.is_empty() {
        return;
    }
    let mut applied = BTreeMap::new();
    for (key, value) in std::mem::take(&mut data.applied) {
        let new_key = match key.splitn(3, '|').collect::<Vec<_>>().as_slice() {
            [region, svc, code] => target_region(svc, code, region, default_region)
                .map(|r| format!("{r}|{svc}|{code}"))
                .unwrap_or_else(|| key.clone()),
            _ => key.clone(),
        };
        applied
            .entry(new_key)
            .and_modify(|v: &mut f64| *v = v.max(value))
            .or_insert(value);
    }
    data.applied = applied;

    let mut tags: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (arn, set) in std::mem::take(&mut data.tags) {
        let new_arn = rescope_arn(&arn, default_region).unwrap_or(arn);
        tags.entry(new_arn).or_default().extend(set);
    }
    data.tags = tags;

    for r in data.requests.values_mut() {
        if let Some(region) =
            target_region(&r.service_code, &r.quota_code, &r.region, default_region)
        {
            r.region = region.to_string();
        }
        if let Some(arn) = rescope_arn(&r.quota_arn, default_region) {
            r.quota_arn = arn;
        }
    }
}

pub async fn save_snapshot(
    state: &SharedServiceQuotasState,
    store: Option<Arc<dyn SnapshotStore>>,
    lock: &AsyncMutex<()>,
) {
    let Some(store) = store else {
        return;
    };
    let _guard = lock.lock().await;
    let snapshot = ServiceQuotasSnapshot {
        schema_version: SERVICEQUOTAS_SNAPSHOT_SCHEMA_VERSION,
        accounts: state.read().clone(),
    };
    let join = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let bytes = serde_json::to_vec(&snapshot)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        store.save(&bytes)
    })
    .await;
    match join {
        Ok(Ok(())) => {}
        Ok(Err(err)) => tracing::error!(%err, "failed to write servicequotas snapshot"),
        Err(err) => tracing::error!(%err, "servicequotas snapshot task panicked"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::RwLock;
    use std::sync::Mutex;

    struct MemStore(Mutex<Option<Vec<u8>>>);
    impl SnapshotStore for MemStore {
        fn load(&self) -> std::io::Result<Option<Vec<u8>>> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn save(&self, bytes: &[u8]) -> std::io::Result<()> {
            *self.0.lock().unwrap() = Some(bytes.to_vec());
            Ok(())
        }
    }

    fn state() -> SharedServiceQuotasState {
        Arc::new(RwLock::new(MultiAccountState::new(
            "000000000000",
            "us-east-1",
            "",
        )))
    }

    #[test]
    fn empty_store_is_empty() {
        assert_eq!(
            load_into(&MemStore(Mutex::new(None)), &state()).unwrap(),
            LoadOutcome::Empty
        );
    }

    #[test]
    fn round_trip_restores_accounts() {
        let mut accounts: MultiAccountState<ServiceQuotasData> =
            MultiAccountState::new("000000000000", "us-east-1", "");
        accounts.get_or_create("111122223333");
        accounts
            .get_or_create("111122223333")
            .enforcement
            .insert("vpc/L-0EA8095F".into(), true);
        let snap = ServiceQuotasSnapshot {
            schema_version: SERVICEQUOTAS_SNAPSHOT_SCHEMA_VERSION,
            accounts,
        };
        let store = MemStore(Mutex::new(Some(serde_json::to_vec(&snap).unwrap())));
        let restored = state();
        assert_eq!(
            load_into(&store, &restored).unwrap(),
            LoadOutcome::Loaded(2)
        );
        // Per-account enforcement overrides are account data and persist.
        assert_eq!(
            restored
                .read()
                .get("111122223333")
                .and_then(|d| d.enforcement.get("vpc/L-0EA8095F").copied()),
            Some(true)
        );
    }

    #[test]
    fn rejects_future_schema() {
        let accounts: MultiAccountState<ServiceQuotasData> =
            MultiAccountState::new("000000000000", "us-east-1", "");
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schema_version": SERVICEQUOTAS_SNAPSHOT_SCHEMA_VERSION + 1,
            "accounts": accounts,
        }))
        .unwrap();
        let store = MemStore(Mutex::new(Some(bytes)));
        assert!(matches!(
            load_into(&store, &state()),
            Err(LoadError::SchemaTooNew { .. })
        ));
    }

    /// A schema 1 snapshot (written while `s3`/`L-DC2B2D3D` was a regional
    /// quota) loads with its bucket quota state moved to the global scope.
    #[test]
    fn schema_1_regional_state_of_a_now_global_quota_is_migrated() {
        let v1 = serde_json::json!({
            "schema_version": 1,
            "accounts": {
                "default_account_id": "000000000000",
                "region": "us-east-1",
                "endpoint": "",
                "accounts": {
                    "111122223333": {
                        "applied": {
                            "us-east-1|s3|L-DC2B2D3D": 200.0,
                            "eu-west-1|s3|L-DC2B2D3D": 300.0,
                            "us-east-1|vpc|L-F678F1CE": 10.0,
                            "|iam|L-FE177D64": 2000.0
                        },
                        "tags": {
                            "arn:aws:servicequotas:us-east-1:111122223333:s3/L-DC2B2D3D": {"a": "1"},
                            "arn:aws:servicequotas:eu-west-1:111122223333:s3/L-DC2B2D3D": {"b": "2"},
                            "arn:aws:servicequotas:us-east-1:111122223333:vpc/L-F678F1CE": {"c": "3"}
                        },
                        "requests": {
                            "r1": {
                                "id": "r1",
                                "region": "eu-west-1",
                                "service_code": "s3",
                                "quota_code": "L-DC2B2D3D",
                                "desired_value": 300.0,
                                "status": "APPROVED",
                                "created": "2026-01-01T00:00:00Z",
                                "last_updated": "2026-01-01T00:00:00Z",
                                "requester": "{}",
                                "quota_arn": "arn:aws:servicequotas:eu-west-1:111122223333:s3/L-DC2B2D3D"
                            }
                        }
                    }
                }
            }
        });
        let store = MemStore(Mutex::new(Some(serde_json::to_vec(&v1).unwrap())));
        let restored = state();
        assert_eq!(
            load_into(&store, &restored).unwrap(),
            LoadOutcome::Loaded(1)
        );
        let guard = restored.read();
        let data = guard.get("111122223333").unwrap();
        let applied: Vec<(&str, f64)> =
            data.applied.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        assert_eq!(
            applied,
            [
                ("us-east-1|vpc|L-F678F1CE", 10.0),
                ("|iam|L-FE177D64", 2000.0),
                ("|s3|L-DC2B2D3D", 300.0),
            ]
        );
        let global = "arn:aws:servicequotas::111122223333:s3/L-DC2B2D3D";
        assert_eq!(data.tags.len(), 2);
        assert_eq!(data.tags[global].len(), 2);
        assert!(data
            .tags
            .contains_key("arn:aws:servicequotas:us-east-1:111122223333:vpc/L-F678F1CE"));
        let r = &data.requests["r1"];
        assert_eq!(r.region, "");
        assert_eq!(r.quota_arn, global);
        // The applied value reads back through the global key.
        let def = catalog::quota("s3", "L-DC2B2D3D").unwrap();
        assert_eq!(
            crate::provider::applied_value(Some(data), "ap-south-1", def),
            300.0
        );
    }

    /// A regional quota stored under the global scope moves to the server's
    /// default region; unknown quotas are left alone; running twice changes
    /// nothing.
    #[test]
    fn global_state_of_a_regional_quota_moves_to_the_default_region() {
        let mut accounts: MultiAccountState<ServiceQuotasData> =
            MultiAccountState::new("000000000000", "us-east-1", "");
        let data = accounts.get_or_create("000000000000");
        data.applied.insert("|vpc|L-F678F1CE".into(), 7.0);
        data.applied.insert("|nope|L-00000000".into(), 1.0);
        data.tags.insert(
            "arn:aws:servicequotas::000000000000:vpc/L-F678F1CE".into(),
            BTreeMap::from([("k".to_string(), "v".to_string())]),
        );
        migrate_quota_scopes(&mut accounts, "eu-west-2");
        migrate_quota_scopes(&mut accounts, "eu-west-2");
        let data = accounts.get("000000000000").unwrap();
        assert_eq!(data.applied["eu-west-2|vpc|L-F678F1CE"], 7.0);
        assert_eq!(data.applied["|nope|L-00000000"], 1.0);
        assert!(data
            .tags
            .contains_key("arn:aws:servicequotas:eu-west-2:000000000000:vpc/L-F678F1CE"));
    }
}
