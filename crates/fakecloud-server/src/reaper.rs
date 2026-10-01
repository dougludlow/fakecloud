//! Startup reaper for orphaned backing containers.
//!
//! fakecloud spawns docker containers for RDS (postgres), ElastiCache (redis),
//! Lambda (runtime images), EC2 and ECS tasks, and labels each one with
//! `fakecloud-instance=fakecloud-<server-pid>`. ECS awsvpc per-task networks
//! carry the same label. Normal shutdown runs `stop_all()` on each runtime,
//! but if the server was killed with SIGKILL (or crashed, or OOM'd) those
//! containers (and networks) outlive the process and pile up.
//!
//! On startup we list every container — then every network — carrying the
//! `fakecloud-instance` label, parse the owning PID out of the label value,
//! and remove any whose owner is no longer alive. Objects owned by the
//! currently-running fakecloud process are always skipped.
//!
//! Memory-mode data volumes (see `fakecloud_core::data_volume`) carry the same
//! label and are reaped the same way, after the containers that mount them.
//! Durable data-dir volumes carry no ownership label, so they are never
//! touched here.

/// Reap orphaned fakecloud-owned containers whose server PID is no longer alive.
///
/// Uses the same CLI detection policy as the runtimes: honors
/// `FAKECLOUD_CONTAINER_CLI` if set, otherwise tries `docker` then `podman`.
/// If no container CLI is available this is a silent no-op — fakecloud is
/// expected to start fine without docker.
pub fn reap_stale_containers() {
    // Uses the shared, *bounded+memoized* detection in `container_net`: an
    // unreachable/wedged daemon leaves a raw `docker info` blocked on connect
    // forever, and this reaper runs at startup, so a naive probe here would
    // hang the server (and every conformance `*_probe` test) indefinitely.
    let Some(cli) = fakecloud_core::container_net::detect_container_cli() else {
        return;
    };

    let reaped = reap_orphans(&cli, &["ps", "-a"], &["rm", "-f"]);
    if reaped > 0 {
        tracing::info!(count = reaped, "reaped orphaned backing containers");
    }

    // ECS awsvpc per-task networks carry the same ownership label. The
    // network driver refuses removal while a container is still attached,
    // so prune networks *after* containers. `network rm` is a no-op for an
    // already-gone network, so a partial container reap above doesn't wedge
    // this pass.
    let reaped_networks = reap_orphans(&cli, &["network", "ls"], &["network", "rm"]);
    if reaped_networks > 0 {
        tracing::info!(count = reaped_networks, "reaped orphaned backing networks");
    }

    // Process-scoped data volumes outlive a killed memory-mode server just like
    // its containers. Containers go first: a volume still mounted by one can't
    // be removed.
    let reaped_volumes = reap_orphan_volumes(&cli);
    if reaped_volumes > 0 {
        tracing::info!(count = reaped_volumes, "reaped orphaned data volumes");
    }
}

/// Remove volumes labelled `fakecloud-instance=<owner>` whose owner is gone.
/// `volume ls` can't print one label portably (docker formats `.Label`,
/// podman only has the `.Labels` map), so the owner is read per volume with
/// `volume inspect`, whose `.Labels` is a map on both engines.
fn reap_orphan_volumes(cli: &str) -> usize {
    let Some(listing) = fakecloud_core::container_net::bounded_output(
        cli,
        &[
            "volume",
            "ls",
            "--filter",
            "label=fakecloud-instance",
            "--format",
            "{{.Name}}",
        ],
    ) else {
        return 0;
    };
    let mut reaped = 0usize;
    for name in listing.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Some(owner) = fakecloud_core::container_net::bounded_output(
            cli,
            &[
                "volume",
                "inspect",
                "--format",
                "{{index .Labels \"fakecloud-instance\"}}",
                name,
            ],
        ) else {
            continue;
        };
        if !fakecloud_core::container_net::owned_by_dead_process(
            owner.trim(),
            fakecloud_core::container_net::pid_alive,
        ) {
            continue;
        }
        if fakecloud_core::container_net::bounded_status(cli, &["volume", "rm", name]) {
            reaped += 1;
        }
    }
    reaped
}

/// List objects carrying the `fakecloud-instance` label via
/// `<cli> <list_args> --filter label=fakecloud-instance`, then run
/// `<cli> <remove_args> <id>` for every object whose owning PID is no longer
/// alive (skipping the current process and live owners). Returns the number
/// removed. Shared by the container and network reap passes.
fn reap_orphans(cli: &str, list_args: &[&str], remove_args: &[&str]) -> usize {
    let mut args: Vec<&str> = list_args.to_vec();
    args.extend_from_slice(&[
        "--filter",
        "label=fakecloud-instance",
        "--format",
        "{{.ID}} {{.Label \"fakecloud-instance\"}}",
    ]);

    // Bounded: the liveness probe answering does not promise this call will,
    // and the reap runs synchronously before the server starts serving, so an
    // unbounded call here wedges startup rather than just the sweep.
    let Some(listing) = fakecloud_core::container_net::bounded_output(cli, &args) else {
        return 0;
    };

    let mut reaped = 0usize;

    for line in listing.lines() {
        let Some((id, label)) = line.split_once(' ') else {
            continue;
        };
        if !fakecloud_core::container_net::owned_by_dead_process(
            label,
            fakecloud_core::container_net::pid_alive,
        ) {
            continue;
        }
        let mut remove_argv = remove_args.to_vec();
        remove_argv.push(id);
        let removed = fakecloud_core::container_net::bounded_status(cli, &remove_argv);
        if removed {
            reaped += 1;
        }
    }

    reaped
}

#[cfg(all(test, unix))]
mod tests {
    use fakecloud_core::container_net::pid_alive;

    #[test]
    fn self_is_alive() {
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn init_is_alive() {
        assert!(pid_alive(1));
    }

    #[test]
    fn huge_pid_is_dead() {
        // Max u32 is far outside any reasonable PID range on any OS.
        assert!(!pid_alive(u32::MAX - 1));
    }
}
