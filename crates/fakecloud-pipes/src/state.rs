//! In-memory state for AWS EventBridge Pipes. Each pipe is stored as a JSON
//! object (the raw create/update input plus generated metadata: Arn, state,
//! timestamps) and echoed verbatim on read, mirroring the Glue/Athena/Batch
//! pattern. Real source->enrichment->target execution lands in a later batch;
//! this batch is the control plane + faithful state machine + persistence.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type SharedPipesState = Arc<RwLock<PipesAccounts>>;

/// A JSON-backed pipe store: pipe name -> (raw input + generated fields).
pub type PipeStore = BTreeMap<String, Value>;

/// Pipes state partitioned by account and then region: pipes are regional,
/// so the same pipe name can exist in two regions of one account.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PipesAccounts {
    /// account id -> region -> state.
    pub accounts: BTreeMap<String, BTreeMap<String, PipesState>>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PipesState {
    /// Pipes keyed by name.
    #[serde(default)]
    pub pipes: PipeStore,
    /// Tags keyed by resource ARN -> { key: value }.
    #[serde(default)]
    pub tags: BTreeMap<String, BTreeMap<String, String>>,
    /// Source checkpoints for streaming sources, so a restart resumes
    /// instead of re-replaying the retained backlog. Keyed by
    /// `"<pipeArn>#<shardId>"` for a Kinesis source (value = the sequence
    /// number of the last delivered record in that shard, so the cursor
    /// survives retention trims) and `"<pipeArn>"` for a DynamoDB-stream
    /// source (value = the last delivered sequence number). SQS sources
    /// don't checkpoint — they ack by deleting the source message.
    #[serde(default)]
    pub source_checkpoints: BTreeMap<String, String>,
}

impl PipesAccounts {
    pub fn new() -> Self {
        Self::default()
    }

    /// The state of `account_id` in `region`, created empty on first use.
    pub fn get_or_create(&mut self, account_id: &str, region: &str) -> &mut PipesState {
        self.accounts
            .entry(account_id.to_string())
            .or_default()
            .entry(region.to_string())
            .or_default()
    }

    /// The state of `account_id` in `region`, `None` when never touched.
    pub fn get(&self, account_id: &str, region: &str) -> Option<&PipesState> {
        self.accounts.get(account_id)?.get(region)
    }

    /// Mutable [`Self::get`]; never creates.
    pub fn get_mut(&mut self, account_id: &str, region: &str) -> Option<&mut PipesState> {
        self.accounts.get_mut(account_id)?.get_mut(region)
    }

    /// Every (account, region, state).
    pub fn iter_regional(&self) -> impl Iterator<Item = (&str, &str, &PipesState)> {
        self.accounts.iter().flat_map(|(a, regions)| {
            regions
                .iter()
                .map(move |(r, s)| (a.as_str(), r.as_str(), s))
        })
    }
}

/// The shape v1 snapshots stored: one state per account.
#[derive(Deserialize)]
struct LegacyPipesAccounts {
    #[serde(default)]
    accounts: BTreeMap<String, PipesState>,
}

#[derive(Deserialize)]
struct LegacyPipesSnapshot {
    #[serde(default)]
    accounts: Option<LegacyPipesAccounts>,
}

#[derive(Deserialize)]
struct SnapshotVersion {
    schema_version: u32,
}

/// Parse a persisted Pipes snapshot. A v1 snapshot (one state per account)
/// is split by region: each pipe goes to the region of its `Arn` (the
/// server's `default_region` when it names none), and tags and source
/// checkpoints follow the pipe ARN they are keyed by. A snapshot newer than
/// this build comes back with its `schema_version` and no state.
pub fn parse_pipes_snapshot(
    bytes: &[u8],
    default_region: &str,
) -> Result<PipesSnapshot, serde_json::Error> {
    let SnapshotVersion { schema_version } = serde_json::from_slice(bytes)?;
    if schema_version >= PIPES_SNAPSHOT_SCHEMA_VERSION {
        if schema_version > PIPES_SNAPSHOT_SCHEMA_VERSION {
            return Ok(PipesSnapshot {
                schema_version,
                accounts: None,
            });
        }
        return serde_json::from_slice(bytes);
    }
    let legacy: LegacyPipesSnapshot = serde_json::from_slice(bytes)?;
    let mut out = PipesAccounts::new();
    let region_of = |arn: &str| {
        fakecloud_aws::arn::region_of(arn)
            .unwrap_or(default_region)
            .to_string()
    };
    for (account, st) in legacy.accounts.map(|a| a.accounts).unwrap_or_default() {
        for (name, pipe) in st.pipes {
            let region = region_of(pipe["Arn"].as_str().unwrap_or(""));
            out.get_or_create(&account, &region)
                .pipes
                .insert(name, pipe);
        }
        for (arn, tags) in st.tags {
            let region = region_of(&arn);
            out.get_or_create(&account, &region).tags.insert(arn, tags);
        }
        for (key, cursor) in st.source_checkpoints {
            let pipe_arn = key.split('#').next().unwrap_or("");
            let region = region_of(pipe_arn);
            out.get_or_create(&account, &region)
                .source_checkpoints
                .insert(key, cursor);
        }
    }
    Ok(PipesSnapshot {
        schema_version: PIPES_SNAPSHOT_SCHEMA_VERSION,
        accounts: Some(out),
    })
}

/// On-disk snapshot envelope; versioned so format changes fail loudly.
#[derive(Clone, Serialize, Deserialize)]
pub struct PipesSnapshot {
    pub schema_version: u32,
    #[serde(default)]
    pub accounts: Option<PipesAccounts>,
}

/// v2: state partitioned by (account, region); v1 kept one state per account.
pub const PIPES_SNAPSHOT_SCHEMA_VERSION: u32 = 2;

/// Pipe lifecycle states (subset of the AWS `PipeState` enum that this
/// emulator transitions through).
pub const STATE_CREATING: &str = "CREATING";
pub const STATE_RUNNING: &str = "RUNNING";
pub const STATE_STOPPED: &str = "STOPPED";
pub const STATE_UPDATING: &str = "UPDATING";
pub const STATE_STARTING: &str = "STARTING";
pub const STATE_STOPPING: &str = "STOPPING";
pub const STATE_DELETING: &str = "DELETING";

/// A pipe in one of these states has an in-flight transition; on restart the
/// recovery pass must re-drive it to its settled state, otherwise a pipe
/// snapshotted mid-transition would stay stuck forever.
pub fn is_transient_state(state: &str) -> bool {
    matches!(
        state,
        STATE_CREATING | STATE_UPDATING | STATE_STARTING | STATE_STOPPING | STATE_DELETING
    )
}
