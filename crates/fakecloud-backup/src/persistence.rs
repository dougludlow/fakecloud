//! Snapshot save/load for AWS Backup state.

use std::sync::Arc;

use tokio::sync::Mutex as AsyncMutex;

use fakecloud_persistence::SnapshotStore;

use crate::state::{BackupSnapshot, SharedBackupState, BACKUP_SNAPSHOT_SCHEMA_VERSION};

#[derive(Debug, PartialEq, Eq)]
pub enum LoadOutcome {
    Empty,
    Loaded(usize),
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("failed to read backup persistence snapshot: {0}")]
    Io(String),
    #[error("failed to parse backup persistence snapshot: {0}")]
    Parse(String),
    #[error("backup persistence schema too new: on-disk={on_disk}, max supported={supported}")]
    SchemaTooNew { on_disk: u32, supported: u32 },
}

pub fn load_into(
    store: &dyn SnapshotStore,
    state: &SharedBackupState,
) -> Result<LoadOutcome, LoadError> {
    let Some(bytes) = store.load().map_err(|e| LoadError::Io(e.to_string()))? else {
        return Ok(LoadOutcome::Empty);
    };
    let snapshot: BackupSnapshot =
        serde_json::from_slice(&bytes).map_err(|e| LoadError::Parse(e.to_string()))?;
    if snapshot.schema_version > BACKUP_SNAPSHOT_SCHEMA_VERSION {
        return Err(LoadError::SchemaTooNew {
            on_disk: snapshot.schema_version,
            supported: BACKUP_SNAPSHOT_SCHEMA_VERSION,
        });
    }
    let mut snapshot = snapshot;
    for (_, st) in snapshot.accounts.iter_mut() {
        st.migrate_legacy_access_point_tags();
    }
    let accounts = snapshot.accounts.account_count();
    *state.write() = snapshot.accounts;
    Ok(LoadOutcome::Loaded(accounts))
}

pub async fn save_snapshot(
    state: &SharedBackupState,
    store: Option<Arc<dyn SnapshotStore>>,
    lock: &AsyncMutex<()>,
) {
    let Some(store) = store else {
        return;
    };
    let _guard = lock.lock().await;
    let snapshot = BackupSnapshot {
        schema_version: BACKUP_SNAPSHOT_SCHEMA_VERSION,
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
        Ok(Err(err)) => tracing::error!(%err, "failed to write backup snapshot"),
        Err(err) => tracing::error!(%err, "backup snapshot task panicked"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::BackupState;
    use fakecloud_core::multi_account::MultiAccountState;
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

    fn state() -> SharedBackupState {
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
        let mut accounts: MultiAccountState<BackupState> =
            MultiAccountState::new("000000000000", "us-east-1", "");
        accounts.get_or_create("111122223333");
        let snap = BackupSnapshot {
            schema_version: BACKUP_SNAPSHOT_SCHEMA_VERSION,
            accounts,
        };
        let store = MemStore(Mutex::new(Some(serde_json::to_vec(&snap).unwrap())));
        assert_eq!(load_into(&store, &state()).unwrap(), LoadOutcome::Loaded(2));
    }

    #[test]
    fn legacy_access_point_tags_move_into_tag_store() {
        // Snapshot written by a build that kept access-point tags on the
        // record itself (`access_points.<arn>.tags`).
        let mut accounts: MultiAccountState<BackupState> =
            MultiAccountState::new("000000000000", "us-east-1", "");
        accounts.get_or_create("111122223333");
        let mut json = serde_json::to_value(BackupSnapshot {
            schema_version: BACKUP_SNAPSHOT_SCHEMA_VERSION,
            accounts,
        })
        .unwrap();
        let arn = "arn:aws:backup:us-east-1:111122223333:backup-access-point:ap1";
        let record = serde_json::json!({
            "arn": arn,
            "name": "ap1",
            "recovery_point_arn": "rp",
            "backup_vault_name": "v",
            "backup_vault_arn": "va",
            "resource_arn": "r",
            "resource_type": "EBS",
            "creation_time": "2026-01-01T00:00:00Z",
            "status": "AVAILABLE",
            "tags": {"team": "data", "env": "old"}
        });
        let acct = json
            .pointer_mut("/accounts/accounts/111122223333")
            .expect("account entry in snapshot JSON");
        acct["access_points"] = serde_json::json!({ arn: record });
        acct["tags"] = serde_json::json!({ arn: {"env": "new"} });
        let store = MemStore(Mutex::new(Some(serde_json::to_vec(&json).unwrap())));
        let shared = state();
        load_into(&store, &shared).unwrap();
        let guard = shared.read();
        let st = guard.get("111122223333").unwrap();
        let tags = &st.tags[arn];
        assert_eq!(tags["team"], "data");
        // The ARN-keyed store wins on a clash.
        assert_eq!(tags["env"], "new");
        assert!(st.access_points[arn].legacy_tags.is_empty());
        // Re-saving does not write the legacy field back.
        let out = serde_json::to_value(&st.access_points[arn]).unwrap();
        assert!(out.get("tags").is_none());
    }

    #[test]
    fn rejects_future_schema() {
        let accounts: MultiAccountState<BackupState> =
            MultiAccountState::new("000000000000", "us-east-1", "");
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schema_version": BACKUP_SNAPSHOT_SCHEMA_VERSION + 1,
            "accounts": accounts,
        }))
        .unwrap();
        let store = MemStore(Mutex::new(Some(bytes)));
        assert!(matches!(
            load_into(&store, &state()),
            Err(LoadError::SchemaTooNew { .. })
        ));
    }
}
