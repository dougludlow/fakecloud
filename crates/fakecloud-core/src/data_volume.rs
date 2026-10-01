//! Naming and lifecycle of the named container volumes that keep a
//! container-backed resource's data (an RDS database, an ElastiCache RDB, an
//! EC2 instance's data dir) across a container being recreated.
//!
//! A volume belongs to exactly one *scope*, and its name carries the scope's
//! tag:
//!
//! * **Data directory** (`--storage-mode persistent --data-path <dir>`): the
//!   tag is a short hash of the canonical `--data-path` together with a
//!   random id minted the first time fakecloud uses the directory and stored
//!   in it ([`SCOPE_FILE`]). The same data dir reattaches the same volumes
//!   across restarts. A different data dir (a copy included, since its path
//!   differs), the same path after the directory was wiped (a new id), or a
//!   second fakecloud on the same daemon (two containers mounting different
//!   host dirs at the same in-container path differ by id) never sees them,
//!   so a fresh data dir can't inherit another one's database. The volumes
//!   are labelled with the scope tag and the data path, so `docker volume ls
//!   --filter label=fakecloud-data-path=<dir>` finds the ones a data dir
//!   owns.
//! * **Process** (memory mode): nothing outlives the process's state, so the
//!   tag is unique to the process and the volume carries the
//!   `fakecloud-instance=fakecloud-<pid>` ownership label the startup reaper
//!   uses to remove it once that process is gone.
//!
//! Volumes created before scoping existed used an unscoped *legacy* name. A
//! resource restored from a data dir written by such a build keeps using its
//! legacy volume (docker has no volume rename, and copying a database between
//! volumes needs an extra container and can fail half way). Each service
//! records the choice on the resource as a [`DataVolumeBinding`]: resources
//! created by this build are bound to their scoped volume from the start, and
//! a resource persisted without a binding is bound once by [`resolve_binding`]
//! against the daemon's volume list.

use std::collections::HashSet;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use sha2::{Digest, Sha256};

/// File in the data dir holding the random half of its volume scope.
pub const SCOPE_FILE: &str = "data-volume-scope";
/// Label carrying the scope tag a volume belongs to.
pub const SCOPE_LABEL: &str = "fakecloud-data-scope";
/// Label carrying the canonical `--data-path` of a data-dir scoped volume.
pub const DATA_PATH_LABEL: &str = "fakecloud-data-path";
/// Ownership label shared with containers and networks (see the reaper).
pub const INSTANCE_LABEL: &str = "fakecloud-instance";

/// Bound on each volume CLI call, so a wedged daemon can't hang a create.
const CLI_TIMEOUT: Duration = Duration::from_secs(30);

/// Which data volume a persisted resource mounts.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataVolumeBinding {
    /// The volume named for the current scope.
    Scoped,
    /// An unscoped volume a pre-scoping build created for the resource.
    Legacy(String),
}

/// Which lifetime a fakecloud process's container volumes are tied to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeScope {
    /// Persistent mode: tied to the data directory.
    DataDir { tag: String, path: String },
    /// Memory mode: tied to this process.
    Process { tag: String, pid: u32 },
}

fn random_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn short_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The data dir's stored id, minting it on first use. Written to a temp file
/// and renamed into place (works on any filesystem), so a crash never leaves
/// a torn id.
fn data_dir_id(dir: &Path) -> std::io::Result<String> {
    let file = dir.join(SCOPE_FILE);
    let read = |file: &Path| -> std::io::Result<String> {
        let id = std::fs::read_to_string(file)?.trim().to_string();
        if valid_id(&id) {
            Ok(id)
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} does not hold a volume scope id", file.display()),
            ))
        }
    };
    match read(&file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        other => return other,
    }
    // A per-writer temp name, so two writers never trip over one temp file.
    // (Two servers sharing one data dir isn't supported; the rename keeps the
    // file whole either way.)
    let tmp = dir.join(format!("{SCOPE_FILE}.{}.tmp", random_id()));
    std::fs::write(&tmp, format!("{}\n", random_id()))?;
    std::fs::rename(&tmp, &file)?;
    read(&file)
}

impl VolumeScope {
    /// Scope for a persistent data directory: a hash of its canonical path
    /// and the id stored in it (minted on first use). Fails if the directory
    /// can't be read or written, since naming volumes for a scope the next
    /// start can't recover would orphan them.
    pub fn for_data_dir(path: &Path) -> std::io::Result<Self> {
        let canonical = std::fs::canonicalize(path)?;
        let id = data_dir_id(&canonical)?;
        let path = canonical.to_string_lossy().into_owned();
        Ok(Self::DataDir {
            tag: format!("d{}", short_hash(format!("{path}\n{id}").as_bytes())),
            path,
        })
    }

    /// Scope for a memory-mode process. The tag is random, so a later process
    /// that happens to reuse this PID never reattaches a volume the reaper
    /// hasn't removed yet.
    pub fn for_process() -> Self {
        Self::Process {
            tag: format!("p{}", &random_id()[..12]),
            pid: std::process::id(),
        }
    }

    pub fn tag(&self) -> &str {
        match self {
            Self::DataDir { tag, .. } | Self::Process { tag, .. } => tag,
        }
    }

    /// `key=value` labels a volume in this scope is created with.
    pub fn labels(&self) -> Vec<String> {
        match self {
            Self::DataDir { tag, path } => vec![
                format!("{SCOPE_LABEL}={tag}"),
                format!("{DATA_PATH_LABEL}={path}"),
            ],
            Self::Process { tag, pid } => vec![
                format!("{SCOPE_LABEL}={tag}"),
                format!("{INSTANCE_LABEL}=fakecloud-{pid}"),
            ],
        }
    }
}

static SCOPE: OnceLock<VolumeScope> = OnceLock::new();

/// Tie this process's volumes to `data_path`. Called once by the server in
/// persistent mode, before any container runtime is built. A later call
/// returns the scope already in place (it can't change under volumes already
/// named for it).
pub fn init_data_dir_scope(data_path: &Path) -> std::io::Result<&'static VolumeScope> {
    let scope = match SCOPE.get() {
        Some(scope) => scope,
        None => {
            let scope = VolumeScope::for_data_dir(data_path)?;
            SCOPE.get_or_init(|| scope)
        }
    };
    require_data_dir_scope(scope)
}

/// A process scope already in place means something named volumes before
/// the data dir was known; going on would give durable data a
/// process-lifetime name (removed on shutdown, unreachable next start).
fn require_data_dir_scope(scope: &VolumeScope) -> std::io::Result<&VolumeScope> {
    match scope {
        VolumeScope::DataDir { .. } => Ok(scope),
        VolumeScope::Process { .. } => Err(std::io::Error::other(
            "container data volumes were named before the data directory scope was set",
        )),
    }
}

/// The scope this process names its volumes in: the data dir once
/// [`init_data_dir_scope`] ran, otherwise the process.
pub fn current_scope() -> &'static VolumeScope {
    SCOPE.get_or_init(VolumeScope::for_process)
}

/// Replace characters outside Docker's `[a-zA-Z0-9_.-]` volume-name set.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// `fakecloud-<service>-data-<scope tag>-<parts...>`.
pub fn scoped_volume_name(service: &str, scope_tag: &str, parts: &[&str]) -> String {
    let mut name = format!("fakecloud-{service}-data-{scope_tag}");
    for part in parts {
        name.push('-');
        name.push_str(&sanitize(part));
    }
    name
}

/// The unscoped name a build before data-dir scoping gave the same volume:
/// `fakecloud-<service>-data-<parts...>`.
pub fn legacy_volume_name(service: &str, parts: &[&str]) -> String {
    let mut name = format!("fakecloud-{service}-data");
    for part in parts {
        name.push('-');
        name.push_str(&sanitize(part));
    }
    name
}

/// A stable incarnation id derived from immutable facts of one resource
/// incarnation (e.g. its ARN and creation timestamp): a delete and a
/// recreate under the same identifier get different ids, so runtime records,
/// container names and data volumes keyed by it never collide.
pub fn incarnation_id(parts: &[&str]) -> String {
    short_hash(parts.join("\n").as_bytes())
}

/// Bind a resource persisted without a binding (written by a pre-scoping
/// build, or never bound because the daemon couldn't be listed) against the
/// daemon's volumes: its scoped volume once that exists (it has been used
/// since), else the legacy volume a pre-scoping build left for it, else a new
/// scoped one.
pub fn resolve_binding(
    scoped: &str,
    legacy: &str,
    existing: &HashSet<String>,
) -> DataVolumeBinding {
    if !existing.contains(scoped) && existing.contains(legacy) {
        DataVolumeBinding::Legacy(legacy.to_string())
    } else {
        DataVolumeBinding::Scoped
    }
}

async fn run(cli: &str, args: &[&str]) -> Option<std::process::Output> {
    let fut = tokio::process::Command::new(cli)
        .args(args)
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(CLI_TIMEOUT, fut).await {
        Ok(Ok(out)) => Some(out),
        _ => None,
    }
}

/// Names of every volume on the daemon, or `None` when the CLI can't answer
/// (so a caller doesn't mistake an unreachable daemon for "no volumes").
pub async fn list_volumes(cli: &str) -> Option<HashSet<String>> {
    let out = run(cli, &["volume", "ls", "--format", "{{.Name}}"]).await?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// Whether the daemon has a volume named `name` (false if it can't answer).
pub async fn volume_exists(cli: &str, name: &str) -> bool {
    run(cli, &["volume", "inspect", name])
        .await
        .is_some_and(|o| o.status.success())
}

/// Create `name` labelled for `scope` unless it already exists (a volume from
/// an earlier run of the same scope, or an adopted legacy volume, is reused
/// as is). Best effort: if the create fails, the container's `-v` still
/// creates the volume, just without labels.
pub async fn ensure_volume(cli: &str, name: &str, scope: &VolumeScope, extra_labels: &[String]) {
    if volume_exists(cli, name).await {
        return;
    }
    let mut args: Vec<String> = vec!["volume".into(), "create".into()];
    for label in scope.labels().iter().chain(extra_labels) {
        args.push("--label".into());
        args.push(label.clone());
    }
    args.push(name.to_string());
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let created = run(cli, &argv).await;
    if !created.as_ref().is_some_and(|o| o.status.success()) {
        tracing::warn!(
            volume = name,
            "could not pre-create labelled data volume; the container will create it unlabelled"
        );
    }
}

/// Remove a volume, ignoring a missing one.
pub async fn remove_volume(cli: &str, name: &str) {
    let _ = run(cli, &["volume", "rm", "-f", name]).await;
}

/// On a clean shutdown in memory mode, remove every volume this process
/// created: its state is gone, so nothing can reattach them. Run after the
/// runtimes stopped their containers (a mounted volume can't be removed).
/// A no-op for a data-dir scope, whose volumes must outlive the process. A
/// killed process's volumes are left to the startup reaper instead.
pub async fn remove_process_volumes(cli: &str) {
    let scope = current_scope();
    if !matches!(scope, VolumeScope::Process { .. }) {
        return;
    }
    // Match by name rather than label: a volume whose labelled create failed
    // was auto-created unlabelled by the container's `-v`, and the scope tag
    // is part of every name this process gave.
    let Some(names) = list_volumes(cli).await else {
        return;
    };
    for name in names
        .iter()
        .filter(|n| is_scoped_volume_name(n, scope.tag()))
    {
        remove_volume(cli, name).await;
    }
}

/// Whether `name` is a volume [`scoped_volume_name`] produced for `scope_tag`.
pub fn is_scoped_volume_name(name: &str, scope_tag: &str) -> bool {
    name.strip_prefix("fakecloud-")
        .and_then(|rest| rest.split_once("-data-"))
        .is_some_and(|(_, rest)| {
            rest.strip_prefix(scope_tag)
                .is_some_and(|tail| tail.starts_with('-'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_scope_is_stable_per_dir_and_distinct_across_dirs() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let a1 = VolumeScope::for_data_dir(a.path()).unwrap();
        let a2 = VolumeScope::for_data_dir(a.path()).unwrap();
        let b1 = VolumeScope::for_data_dir(b.path()).unwrap();
        assert_eq!(a1, a2, "same data dir must map to the same scope");
        assert_ne!(a1.tag(), b1.tag(), "different data dirs must never share");
        assert!(a1.tag().starts_with('d'));
        assert_eq!(a1.tag().len(), 13);
        // The id lives in the data dir.
        let stored = std::fs::read_to_string(a.path().join(SCOPE_FILE)).unwrap();
        assert!(valid_id(stored.trim()), "{stored:?}");
    }

    #[test]
    fn wiped_data_dir_at_the_same_path_gets_a_new_scope() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("data");
        std::fs::create_dir(&dir).unwrap();
        let before = VolumeScope::for_data_dir(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::create_dir(&dir).unwrap();
        let after = VolumeScope::for_data_dir(&dir).unwrap();
        assert_ne!(before.tag(), after.tag());
    }

    #[test]
    fn copied_data_dir_gets_its_own_scope() {
        let parent = tempfile::tempdir().unwrap();
        let from = parent.path().join("from");
        let to = parent.path().join("to");
        std::fs::create_dir(&from).unwrap();
        std::fs::create_dir(&to).unwrap();
        let original = VolumeScope::for_data_dir(&from).unwrap();
        std::fs::copy(from.join(SCOPE_FILE), to.join(SCOPE_FILE)).unwrap();
        let copy = VolumeScope::for_data_dir(&to).unwrap();
        assert_ne!(original.tag(), copy.tag());
    }

    #[test]
    fn same_path_with_a_different_id_gets_its_own_scope() {
        // Two fakecloud containers mounting different host dirs at the same
        // in-container --data-path.
        let dir = tempfile::tempdir().unwrap();
        let first = VolumeScope::for_data_dir(dir.path()).unwrap();
        std::fs::write(dir.path().join(SCOPE_FILE), format!("{}\n", random_id())).unwrap();
        let second = VolumeScope::for_data_dir(dir.path()).unwrap();
        assert_ne!(first.tag(), second.tag());
    }

    #[test]
    fn data_dir_scope_canonicalizes_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("data");
        std::fs::create_dir(&sub).unwrap();
        let dotted = sub.join("..").join("data");
        assert_eq!(
            VolumeScope::for_data_dir(&sub).unwrap().tag(),
            VolumeScope::for_data_dir(&dotted).unwrap().tag()
        );
    }

    #[test]
    fn data_dir_scope_rejects_a_corrupt_tag_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SCOPE_FILE), "../../etc").unwrap();
        assert!(VolumeScope::for_data_dir(dir.path()).is_err());
    }

    #[test]
    fn data_dir_labels_name_the_scope_and_path() {
        let dir = tempfile::tempdir().unwrap();
        let scope = VolumeScope::for_data_dir(dir.path()).unwrap();
        let VolumeScope::DataDir { tag, path } = &scope else {
            panic!("expected a data-dir scope");
        };
        assert_eq!(
            path,
            &std::fs::canonicalize(dir.path())
                .unwrap()
                .to_string_lossy()
                .into_owned()
        );
        assert_eq!(
            scope.labels(),
            vec![
                format!("fakecloud-data-scope={tag}"),
                format!("fakecloud-data-path={path}"),
            ]
        );
        // No ownership label: the reaper must never remove durable volumes.
        assert!(!scope.labels().iter().any(|l| l.starts_with(INSTANCE_LABEL)));
    }

    #[test]
    fn process_scope_is_unique_and_reapable() {
        let a = VolumeScope::for_process();
        let b = VolumeScope::for_process();
        assert_ne!(a.tag(), b.tag(), "a reused pid must not reuse a scope");
        assert!(a.tag().starts_with('p'));
        let me = std::process::id();
        assert!(a
            .labels()
            .contains(&format!("fakecloud-instance=fakecloud-{me}")));
    }

    #[test]
    fn scoped_and_legacy_names() {
        assert_eq!(
            scoped_volume_name("rds", "dabc123def456", &["123456789012", "my-db"]),
            "fakecloud-rds-data-dabc123def456-123456789012-my-db"
        );
        assert_eq!(
            scoped_volume_name("elasticache", "dabc", &["weird/id:1"]),
            "fakecloud-elasticache-data-dabc-weird-id-1"
        );
        // The legacy names match what builds before scoping created.
        assert_eq!(
            legacy_volume_name("rds", &["123456789012", "my-db"]),
            "fakecloud-rds-data-123456789012-my-db"
        );
        assert_eq!(
            legacy_volume_name("elasticache", &["my-cache"]),
            "fakecloud-elasticache-data-my-cache"
        );
    }

    #[test]
    fn persistent_mode_refuses_a_process_scope_already_in_place() {
        assert!(require_data_dir_scope(&VolumeScope::for_process()).is_err());
        let dir = tempfile::tempdir().unwrap();
        let scope = VolumeScope::for_data_dir(dir.path()).unwrap();
        assert!(require_data_dir_scope(&scope).is_ok());
    }

    #[test]
    fn scoped_volume_names_are_recognised_by_tag() {
        let name = scoped_volume_name("elasticache", "pabc123", &["123456789012", "c"]);
        assert!(is_scoped_volume_name(&name, "pabc123"));
        assert!(!is_scoped_volume_name(&name, "pabc12"));
        assert!(!is_scoped_volume_name(&name, "pother"));
        assert!(!is_scoped_volume_name(
            "fakecloud-elasticache-data-c",
            "pabc123"
        ));
        assert!(!is_scoped_volume_name(
            "someone-else-data-pabc123-x",
            "pabc123"
        ));
    }

    #[test]
    fn incarnation_ids_differ_per_incarnation() {
        let a = incarnation_id(&[
            "arn:aws:elasticache:us-east-1:1:cluster:c",
            "2026-01-01T00:00:00.1Z",
        ]);
        let b = incarnation_id(&[
            "arn:aws:elasticache:us-east-1:1:cluster:c",
            "2026-01-01T00:00:00.2Z",
        ]);
        assert_ne!(a, b);
        assert_eq!(
            a,
            incarnation_id(&[
                "arn:aws:elasticache:us-east-1:1:cluster:c",
                "2026-01-01T00:00:00.1Z"
            ])
        );
        assert_eq!(a.len(), 12);
    }

    #[test]
    fn resolve_binding_prefers_scoped_then_legacy() {
        let set = |names: &[&str]| names.iter().map(|n| n.to_string()).collect();
        assert_eq!(
            resolve_binding("s", "l", &set(&[])),
            DataVolumeBinding::Scoped
        );
        assert_eq!(
            resolve_binding("s", "l", &set(&["l"])),
            DataVolumeBinding::Legacy("l".into())
        );
        // A scoped volume in use wins over a legacy one lying around.
        assert_eq!(
            resolve_binding("s", "l", &set(&["s", "l"])),
            DataVolumeBinding::Scoped
        );
        assert_eq!(
            resolve_binding("s", "l", &set(&["s"])),
            DataVolumeBinding::Scoped
        );
    }

    #[test]
    fn binding_serializes_stably() {
        assert_eq!(
            serde_json::to_string(&DataVolumeBinding::Scoped).unwrap(),
            "\"scoped\""
        );
        assert_eq!(
            serde_json::to_string(&DataVolumeBinding::Legacy("v".into())).unwrap(),
            "{\"legacy\":\"v\"}"
        );
    }
}
