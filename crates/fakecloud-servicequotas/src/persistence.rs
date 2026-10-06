//! Snapshot save/load for Service Quotas state.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use tokio::sync::Mutex as AsyncMutex;

use fakecloud_core::multi_account::MultiAccountState;

use fakecloud_persistence::SnapshotStore;

use crate::catalog;
use crate::state::{
    QuotaRequest, ServiceQuotasData, ServiceQuotasSnapshot, SharedServiceQuotasState,
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
    migrate_quota_scopes(&mut accounts, Utc::now());
    let count = accounts.account_count();
    *state.write() = accounts;
    Ok(LoadOutcome::Loaded(count))
}

/// Re-key state stored under a quota scope the catalog no longer has.
///
/// Applied values, tags and requests of a global quota carry no region; those
/// of a regional quota carry one. A snapshot written while the catalog had a
/// quota in the other scope (schema 1 had `s3`/`L-DC2B2D3D` regional, for
/// example) would otherwise be invisible. The snapshot's own default region
/// decides what wins:
///
/// - a quota that is now global keeps the applied value of the default
///   region when there is one (v0.49.0 kept the S3 bucket quota there), else
///   the highest regional value; tags of every regional ARN merge onto the
///   global ARN, the default region's winning a clash; requests lose their
///   region, and when that leaves several open requests for the quota, the
///   newest stays open and the others are `CASE_CLOSED`;
/// - a quota that is now regional moves its global entries to the default
///   region.
///
/// Entries of quotas the catalog does not know are left as they are.
/// Idempotent, so it runs on every load.
pub fn migrate_quota_scopes(
    accounts: &mut MultiAccountState<ServiceQuotasData>,
    now: DateTime<Utc>,
) {
    let default_region = accounts.region().to_string();
    for (_, data) in accounts.iter_mut() {
        migrate_account(data, &default_region, now);
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
/// its region replaced, and the region it had. `None` for anything else.
fn rescope_arn(arn: &str, default_region: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    let [prefix, partition, service, region, account, resource] = parts.as_slice() else {
        return None;
    };
    if *prefix != "arn" || *service != "servicequotas" {
        return None;
    }
    let (svc, code) = resource.split_once('/')?;
    let target = target_region(svc, code, region, default_region)?;
    Some((
        format!("arn:{partition}:servicequotas:{target}:{account}:{resource}"),
        region.to_string(),
    ))
}

fn migrate_account(data: &mut ServiceQuotasData, default_region: &str, now: DateTime<Utc>) {
    if data.applied.is_empty() && data.tags.is_empty() && data.requests.is_empty() {
        return;
    }

    // Applied values, grouped by the key they belong under.
    let mut grouped: BTreeMap<String, Vec<(String, f64)>> = BTreeMap::new();
    for (key, value) in std::mem::take(&mut data.applied) {
        let (new_key, stored_region) = match key.splitn(3, '|').collect::<Vec<_>>().as_slice() {
            [region, svc, code] => (
                target_region(svc, code, region, default_region)
                    .map(|r| format!("{r}|{svc}|{code}"))
                    .unwrap_or_else(|| key.clone()),
                region.to_string(),
            ),
            _ => (key.clone(), String::new()),
        };
        grouped
            .entry(new_key)
            .or_default()
            .push((stored_region, value));
    }
    data.applied = grouped
        .into_iter()
        .map(|(key, values)| {
            let from_default = key
                .starts_with('|')
                .then(|| values.iter().find(|(r, _)| r == default_region))
                .flatten()
                .map(|(_, v)| *v);
            let value = from_default.unwrap_or_else(|| {
                values
                    .iter()
                    .map(|(_, v)| *v)
                    .fold(f64::NEG_INFINITY, f64::max)
            });
            (key, value)
        })
        .collect();

    // Tags: merge, applying the default region's last so they win a clash.
    let mut moved: Vec<(String, bool, BTreeMap<String, String>)> = std::mem::take(&mut data.tags)
        .into_iter()
        .map(|(arn, set)| match rescope_arn(&arn, default_region) {
            Some((new_arn, region)) => (new_arn, region == default_region, set),
            None => (arn, false, set),
        })
        .collect();
    moved.sort_by_key(|(_, from_default, _)| *from_default);
    for (arn, _, set) in moved {
        data.tags.entry(arn).or_default().extend(set);
    }

    for r in data.requests.values_mut() {
        if let Some(region) =
            target_region(&r.service_code, &r.quota_code, &r.region, default_region)
        {
            r.region = region.to_string();
        }
        if let Some((arn, _)) = rescope_arn(&r.quota_arn, default_region) {
            r.quota_arn = arn;
        }
    }
    // At most one open request per quota and region: the newest.
    let mut open: BTreeMap<(String, String, String), Vec<&mut QuotaRequest>> = BTreeMap::new();
    for r in data.requests.values_mut() {
        if matches!(r.status.as_str(), "PENDING" | "CASE_OPENED") {
            open.entry((
                r.service_code.clone(),
                r.quota_code.clone(),
                r.region.clone(),
            ))
            .or_default()
            .push(r);
        }
    }
    for (_, mut group) in open {
        group.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.id.cmp(&b.id)));
        for older in group.into_iter().skip(1) {
            older.status = "CASE_CLOSED".to_string();
            older.last_updated = now;
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

    fn request(id: &str, region: &str, status: &str, created: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "region": region,
            "service_code": "s3",
            "quota_code": "L-DC2B2D3D",
            "desired_value": 300.0,
            "status": status,
            "created": created,
            "last_updated": created,
            "requester": "{}",
            "quota_arn": format!("arn:aws:servicequotas:{region}:111122223333:s3/L-DC2B2D3D"),
        })
    }

    /// A schema 1 snapshot (written while `s3`/`L-DC2B2D3D` was a regional
    /// quota, which v0.49.0 kept in us-east-1) loads with its bucket quota
    /// state moved to the global scope.
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
                            "arn:aws:servicequotas:us-east-1:111122223333:s3/L-DC2B2D3D": {"a": "1", "env": "home"},
                            "arn:aws:servicequotas:eu-west-1:111122223333:s3/L-DC2B2D3D": {"b": "2", "env": "away"},
                            "arn:aws:servicequotas:us-east-1:111122223333:vpc/L-F678F1CE": {"c": "3"}
                        },
                        "requests": {
                            "old": request("old", "eu-west-1", "PENDING", "2026-01-01T00:00:00Z"),
                            "new": request("new", "us-east-1", "CASE_OPENED", "2026-02-01T00:00:00Z"),
                            "done": request("done", "eu-west-1", "APPROVED", "2025-12-01T00:00:00Z")
                        }
                    }
                }
            }
        });
        let store = MemStore(Mutex::new(Some(serde_json::to_vec(&v1).unwrap())));
        let restored = state();
        let before = Utc::now();
        assert_eq!(
            load_into(&store, &restored).unwrap(),
            LoadOutcome::Loaded(1)
        );
        let guard = restored.read();
        let data = guard.get("111122223333").unwrap();
        // The snapshot's default region's value wins over a higher one.
        let applied: Vec<(&str, f64)> =
            data.applied.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        assert_eq!(
            applied,
            [
                ("us-east-1|vpc|L-F678F1CE", 10.0),
                ("|iam|L-FE177D64", 2000.0),
                ("|s3|L-DC2B2D3D", 200.0),
            ]
        );
        let global = "arn:aws:servicequotas::111122223333:s3/L-DC2B2D3D";
        assert_eq!(data.tags.len(), 2);
        let tags = &data.tags[global];
        assert_eq!(tags.len(), 3);
        assert_eq!(tags["env"], "home");
        assert!(data
            .tags
            .contains_key("arn:aws:servicequotas:us-east-1:111122223333:vpc/L-F678F1CE"));
        for r in data.requests.values() {
            assert_eq!(r.region, "");
            assert_eq!(r.quota_arn, global);
        }
        // Two open requests for the now-global quota: the newest stays open.
        assert_eq!(data.requests["new"].status, "CASE_OPENED");
        assert_eq!(data.requests["old"].status, "CASE_CLOSED");
        assert!(data.requests["old"].last_updated >= before);
        assert_eq!(data.requests["done"].status, "APPROVED");
        let def = catalog::quota("s3", "L-DC2B2D3D").unwrap();
        assert_eq!(
            crate::provider::applied_value(Some(data), "ap-south-1", def),
            200.0
        );
    }

    /// Without a value in the snapshot's default region, the highest
    /// regional value is kept.
    #[test]
    fn now_global_quota_without_a_default_region_value_keeps_the_highest() {
        let mut accounts: MultiAccountState<ServiceQuotasData> =
            MultiAccountState::new("000000000000", "us-east-1", "");
        let data = accounts.get_or_create("000000000000");
        data.applied.insert("eu-west-1|s3|L-DC2B2D3D".into(), 300.0);
        data.applied
            .insert("ap-south-1|s3|L-DC2B2D3D".into(), 250.0);
        migrate_quota_scopes(&mut accounts, Utc::now());
        let data = accounts.get("000000000000").unwrap();
        assert_eq!(data.applied.len(), 1);
        assert_eq!(data.applied["|s3|L-DC2B2D3D"], 300.0);
    }

    /// A regional quota stored under the global scope moves to the
    /// snapshot's default region, not the server's current one; unknown
    /// quotas are left alone; running twice changes nothing.
    #[test]
    fn global_state_of_a_regional_quota_moves_to_the_snapshot_default_region() {
        let mut accounts: MultiAccountState<ServiceQuotasData> =
            MultiAccountState::new("000000000000", "eu-west-2", "");
        let data = accounts.get_or_create("000000000000");
        data.applied.insert("|vpc|L-F678F1CE".into(), 7.0);
        data.applied.insert("|nope|L-00000000".into(), 1.0);
        data.tags.insert(
            "arn:aws:servicequotas::000000000000:vpc/L-F678F1CE".into(),
            BTreeMap::from([("k".to_string(), "v".to_string())]),
        );
        let bytes = serde_json::to_vec(&ServiceQuotasSnapshot {
            schema_version: 1,
            accounts,
        })
        .unwrap();
        let store = MemStore(Mutex::new(Some(bytes)));
        // The server now runs in us-east-1.
        let restored = state();
        load_into(&store, &restored).unwrap();
        let mut guard = restored.write();
        for _ in 0..2 {
            let data = guard.get("000000000000").unwrap();
            assert_eq!(data.applied["eu-west-2|vpc|L-F678F1CE"], 7.0);
            assert!(!data.applied.contains_key("us-east-1|vpc|L-F678F1CE"));
            assert_eq!(data.applied["|nope|L-00000000"], 1.0);
            assert!(data
                .tags
                .contains_key("arn:aws:servicequotas:eu-west-2:000000000000:vpc/L-F678F1CE"));
            migrate_quota_scopes(&mut guard, Utc::now());
        }
    }
}
