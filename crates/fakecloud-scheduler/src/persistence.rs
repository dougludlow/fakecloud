//! Snapshot load helper for the scheduler state.
//!
//! Keeps the server's `main.rs` wiring block thin (single fn call)
//! while the interesting branches — schema-version gate, migration-
//! point, empty-startup — get unit-tested here.

use fakecloud_persistence::SnapshotStore;

use crate::state::{SharedSchedulerState, SCHEDULER_SNAPSHOT_SCHEMA_VERSION};

#[derive(Debug, PartialEq, Eq)]
pub enum LoadOutcome {
    /// No snapshot file on disk; start with fresh state.
    Empty,
    /// Snapshot loaded successfully; returns the restored account count.
    Loaded(usize),
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("failed to read scheduler persistence snapshot: {0}")]
    Io(String),
    #[error("failed to parse scheduler persistence snapshot: {0}")]
    Parse(String),
    #[error("scheduler persistence schema too new: on-disk={on_disk}, max supported={supported}")]
    SchemaTooNew { on_disk: u32, supported: u32 },
}

/// Load a snapshot into `state`. Returns `Empty` when the store has
/// nothing saved, `Loaded(n)` after a successful restore, or a
/// descriptive error the server turns into a fatal startup message.
pub fn load_into(
    store: &dyn SnapshotStore,
    state: &SharedSchedulerState,
) -> Result<LoadOutcome, LoadError> {
    let Some(bytes) = store.load().map_err(|e| LoadError::Io(e.to_string()))? else {
        return Ok(LoadOutcome::Empty);
    };
    let snapshot = crate::state::parse_scheduler_snapshot(&bytes)
        .map_err(|e| LoadError::Parse(e.to_string()))?;
    if snapshot.schema_version > SCHEDULER_SNAPSHOT_SCHEMA_VERSION {
        return Err(LoadError::SchemaTooNew {
            on_disk: snapshot.schema_version,
            supported: SCHEDULER_SNAPSHOT_SCHEMA_VERSION,
        });
    }
    let Some(accounts) = snapshot.accounts else {
        return Ok(LoadOutcome::Empty);
    };
    let count = accounts.account_count();
    *state.write() = accounts;
    Ok(LoadOutcome::Loaded(count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{SchedulerSnapshot, SchedulerState};
    use fakecloud_core::multi_account::{MultiAccountState, MultiRegionState};
    use parking_lot::RwLock;
    use std::sync::Arc;
    use std::sync::Mutex;

    fn make_state() -> SharedSchedulerState {
        Arc::new(RwLock::new(MultiRegionState::new(
            "000000000000",
            "us-east-1",
            "",
        )))
    }

    struct MemStore {
        data: Mutex<Option<Vec<u8>>>,
    }
    impl MemStore {
        fn new(data: Option<Vec<u8>>) -> Self {
            Self {
                data: Mutex::new(data),
            }
        }
    }
    impl SnapshotStore for MemStore {
        fn load(&self) -> std::io::Result<Option<Vec<u8>>> {
            Ok(self.data.lock().unwrap().clone())
        }
        fn save(&self, bytes: &[u8]) -> std::io::Result<()> {
            *self.data.lock().unwrap() = Some(bytes.to_vec());
            Ok(())
        }
    }

    #[test]
    fn load_into_empty_returns_empty() {
        let state = make_state();
        let store = MemStore::new(None);
        let outcome = load_into(&store, &state).unwrap();
        assert_eq!(outcome, LoadOutcome::Empty);
    }

    #[test]
    fn load_into_valid_snapshot_restores_accounts() {
        let state = make_state();
        let mut mas: MultiRegionState<SchedulerState> =
            MultiRegionState::new("999999999999", "us-east-1", "");
        mas.regional_mut("999999999999", "eu-west-1");
        let snap = SchedulerSnapshot::of(SCHEDULER_SNAPSHOT_SCHEMA_VERSION, mas);
        let bytes = serde_json::to_vec(&snap).unwrap();
        let store = MemStore::new(Some(bytes));
        let outcome = load_into(&store, &state).unwrap();
        assert_eq!(outcome, LoadOutcome::Loaded(1));
        let accounts = state.read();
        assert!(accounts.regional("999999999999", "eu-west-1").is_some());
    }

    #[test]
    fn load_into_migrates_v1_snapshot_by_arn_region() {
        let state = make_state();
        let mut legacy: MultiAccountState<SchedulerState> =
            MultiAccountState::new("000000000000", "us-east-1", "");
        let west_group = crate::state::ScheduleGroup {
            arn: crate::state::group_arn("eu-west-1", "000000000000", "jobs"),
            name: "jobs".into(),
            state: "ACTIVE".into(),
            creation_date: chrono::Utc::now(),
            last_modification_date: chrono::Utc::now(),
            tags: Default::default(),
        };
        legacy
            .default_mut()
            .groups
            .insert("jobs".into(), west_group);
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "accounts": legacy,
        }))
        .unwrap();
        let store = MemStore::new(Some(bytes));
        assert_eq!(load_into(&store, &state).unwrap(), LoadOutcome::Loaded(1));
        let accounts = state.read();
        let west = accounts.regional("000000000000", "eu-west-1").unwrap();
        assert!(west.groups.contains_key("jobs"));
        // The migrated region gets its own default group, in its own region.
        assert!(west.groups[crate::state::DEFAULT_GROUP]
            .arn
            .contains(":eu-west-1:"));
        let east = accounts.regional("000000000000", "us-east-1").unwrap();
        assert!(!east.groups.contains_key("jobs"));
    }

    #[test]
    fn load_into_rejects_future_schema() {
        let state = make_state();
        let mas: MultiRegionState<SchedulerState> =
            MultiRegionState::new("000000000000", "us-east-1", "");
        let snap = SchedulerSnapshot::of(SCHEDULER_SNAPSHOT_SCHEMA_VERSION + 1, mas);
        let bytes = serde_json::to_vec(&snap).unwrap();
        let store = MemStore::new(Some(bytes));
        let err = load_into(&store, &state).err().unwrap();
        assert!(matches!(err, LoadError::SchemaTooNew { .. }));
    }

    #[test]
    fn load_into_reports_parse_errors() {
        let state = make_state();
        let store = MemStore::new(Some(b"not json".to_vec()));
        let err = load_into(&store, &state).err().unwrap();
        assert!(matches!(err, LoadError::Parse(_)));
    }
}
