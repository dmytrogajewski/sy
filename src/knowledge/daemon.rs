//! `sy knowledge daemon` — supervises Qdrant, watches sources, runs the
//! schedule loop. Long-lived foreground process spawned by niri.
//!
//! Lifecycle:
//!   1. Spawn qdrant child with explicit storage + log paths.
//!   2. Wait for qdrant /readyz.
//!   3. Ensure the `sy_knowledge` collection exists.
//!   4. Bind IPC socket; spawn a thread translating `ipc::Op` into the
//!      daemon's internal `DaemonOp` channel.
//!   5. Enumerate `qdr.toml` manifests (shallow `$HOME` ≤ 2 + each
//!      `mode = "discover"` source) into `active_manifests`.
//!   6. Build the hybrid watcher set: shallow-home (NonRecursive) +
//!      discover roots (Recursive) + explicit sources (Recursive) + each
//!      manifested folder (Recursive). Watcher events split by basename:
//!      `qdr.toml` → DiscoveryTickle, otherwise FsTickle.
//!   7. Initial index pass (covers explicit sources + manifested folders).
//!   8. Schedule loop. FS-triggered passes are gated by a 30 s anti-thrash
//!      floor; scheduled passes fire on every interval tick. Any
//!      DiscoveryTickle re-runs `manifest::discover_all`, diffs the active
//!      set, and triggers a watcher rebuild + qdrant cleanup for retired
//!      manifests.
//!   9. SIGTERM/SIGINT → terminate qdrant, remove socket, exit 0.

use std::{
    collections::HashSet,
    fs::OpenOptions,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use notify::RecursiveMode;
use notify_debouncer_mini::new_debouncer;

use super::{
    QDRANT_PORT, calibrate, cli, embed, ipc, manifest, qdrant, query, repair, runctx::RunCtx,
    sources, sparse, state, status,
};
use sources::SourceMode;

/// Floor between consecutive FS-triggered index passes. Scheduled ticks
/// are not gated by this — they always run.
const FS_TICKLE_FLOOR: Duration = Duration::from_secs(30);

/// What woke up the daemon's main loop. The IPC layer speaks `ipc::Op`;
/// the watcher speaks tickles. We multiplex into one channel so the loop
/// can coalesce them in a single recv-window.
enum DaemonOp {
    Ipc(ipc::Op),
    /// Content change inside a known indexed folder.
    FsTickle,
    /// `qdr.toml` create/modify/delete somewhere we watch.
    DiscoveryTickle,
}

pub fn run() -> Result<()> {
    // SPEC §4.6 / arch-observability Step 1: install the daemon's
    // tracing subscriber (journald + rolling JSONL + stderr fmt)
    // before any other startup work so even the migration warning
    // and qdrant-spawn errors below land on every sink. The
    // `WorkerGuard` is held for the daemon's lifetime via this
    // binding — dropping it would risk losing buffered log lines.
    let _obs_guard = sy_core::obs::init(sy_core::obs::Mode::Daemon {
        name: "sy-knowledge",
    })?;
    set_process_priority();
    if let Err(e) = crate::aiplane::status::migrate_state_dir() {
        // Best-effort: a missing or unwritable XDG_STATE_HOME shouldn't
        // block daemon startup, but the warning surfaces under
        // `journalctl --user -u sy-aiplane`.
        tracing::warn!(
            target: "sy::knowledge::daemon",
            error = %format!("{e:#}"),
            "state-dir migration warning"
        );
    }
    // SPEC §4.5: when run under systemd, `sy-qdrant.service` owns the
    // qdrant process. Probing the HTTP port first lets us coexist with
    // that unit instead of fighting it for the port — the second
    // spawn would silently die ([qdrant] <defunct>) and the daemon's
    // HTTP client would still hit the unit-managed one, but pointed
    // at the WRONG storage path, leaving `index.json` claiming chunks
    // that don't exist in qdrant. Bug repro that lost ~60k embeddings.
    let qdrant_child: Option<Child> = if qdrant::wait_ready(1).is_ok() {
        tracing::info!(
            target: "sy::knowledge::daemon",
            "qdrant already up (likely sy-qdrant.service), skipping internal spawn"
        );
        None
    } else {
        // BUG-20260524-2203: scrub the storage tree before spawning so a
        // single segment with an empty / unparseable JSON file (the
        // failure mode after an ungraceful shutdown) doesn't panic
        // qdrant during `Collection::load` and brick the plane until a
        // human runs `find -size 0` by hand. The systemd-managed path
        // gets the same scrub via `ExecStartPre=` on
        // `sy-qdrant.service`, but daemon spawns happen here.
        match state::qdrant_storage_dir() {
            Ok(storage) => match repair::quarantine_corrupt_segments(&storage) {
                Ok(report) if !report.quarantined.is_empty() || report.swept_atomicwrite > 0 => {
                    tracing::warn!(
                        target: "sy::knowledge::daemon",
                        storage = %storage.display(),
                        quarantined = report.quarantined.len(),
                        swept_atomicwrite = report.swept_atomicwrite,
                        shards_scanned = report.shards_scanned,
                        "qdrant repair pass mutated storage before spawn"
                    );
                    for q in &report.quarantined {
                        tracing::warn!(
                            target: "sy::knowledge::daemon",
                            collection = %q.collection,
                            shard = %q.shard,
                            segment = %q.segment_id,
                            new_path = %q.new_path.display(),
                            reason = %q.reason,
                            "quarantined corrupt segment"
                        );
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(
                    target: "sy::knowledge::daemon",
                    error = %format!("{e:#}"),
                    "qdrant repair pass errored; continuing to spawn anyway"
                ),
            },
            Err(e) => tracing::warn!(
                target: "sy::knowledge::daemon",
                error = %format!("{e:#}"),
                "qdrant storage_dir unresolved; skipping repair pass"
            ),
        }
        let child = spawn_qdrant().context("spawn qdrant")?;
        if let Err(e) = qdrant::wait_ready(20) {
            let mut c = child;
            let _ = c.kill();
            return Err(e);
        }
        Some(child)
    };
    qdrant::ensure_collection()?;

    // qdrant ≥ 1.16 guard (knowledge-retrieval-iter1 cross-cutting DoD): the
    // hybrid Universal Query sets `query.rrf.k = 60`, but qdrant < 1.16
    // silently ignores configurable RRF `k` and hybrid search regresses. Warn
    // loudly (no hard crash — dense-only still works); see
    // `qdrant_version_warning`.
    if let Some(w) = qdrant_version_warning(qdrant::server_version()) {
        tracing::warn!(target: "sy::knowledge::daemon", "{w}");
    }

    // Schema v2 migration (knowledge-retrieval-iter1 Step 3): if the live
    // collection predates the named `dense`+`sparse` schema, queue a resumable
    // `FullResync` once the daemon loop is up so it is dropped, recreated at
    // SCHEMA_VERSION, and re-embedded. `ensure_collection` only *creates* a
    // missing collection (always at v2), so a pre-v2 collection survives the
    // call above and is still detectable here.
    let migrate_schema = schema_migration_needed(qdrant::collection_info().as_ref());
    if migrate_schema {
        tracing::info!(
            target: "sy::knowledge::daemon",
            schema_version = crate::knowledge::SCHEMA_VERSION,
            "collection predates schema v2; queuing full resync"
        );
    }

    // Start the aiplane supervisor. Each NPU workload (embed, rerank)
    // runs in its own subprocess so each can own a /dev/accel/accel0
    // HW context — XDNA limits to one HW context per process. The
    // supervisor is required: if it can't bring the workers up the
    // daemon refuses to start rather than degrading silently.
    init_aiplane_supervisor().context("init aiplane supervisor")?;

    // sy-mon Step 10: bind the aiplane plane's Prometheus UDS
    // exposition surface at $XDG_RUNTIME_DIR/sy/aiplane/metrics.sock.
    // The shared installer in `sy_core::obs::mon_exporter` requires
    // an active tokio runtime, so the wrapper module owns a dedicated
    // runtime thread; the returned guard holds the install handle for
    // the daemon's lifetime and unlinks the socket on Drop. Gated on
    // `mon-exporter` so `--no-default-features` builds skip the bind.
    #[cfg(feature = "mon-exporter")]
    let _aiplane_mon_exporter = match crate::aiplane::mon_exporter::spawn() {
        Ok(g) => {
            tracing::info!(
                target: "sy::knowledge::daemon",
                path = %g.path().display(),
                "aiplane mon-exporter bound"
            );
            Some(g)
        }
        Err(e) => {
            // Bind failure is non-fatal: the aggregator (Step 12)
            // tolerates a missing per-plane socket as a zero-metric
            // source, and `sy mon doctor` (Step 21) is the surface
            // for raising the alarm. Refusing to start the daemon
            // just because the metrics socket failed would be a
            // disproportionate response.
            tracing::warn!(
                target: "sy::knowledge::daemon",
                error = %format!("{e:#}"),
                "aiplane mon-exporter failed to bind; continuing without metrics socket"
            );
            None
        }
    };

    // sy-mon Step 20: expose the knowledge plane at
    // `$XDG_RUNTIME_DIR/sy/knowledge/metrics.sock`. The knowledge
    // daemon and the aiplane plane share one OS process — sy-core's
    // `mon_exporter::install` sets a *process-global* `metrics`
    // recorder, so a second `install` call would fail with
    // `AlreadyInstalled` and unlink its socket. Instead, expose the
    // knowledge plane as a symlink onto the aiplane UDS that was
    // bound above: every metric emitted from anywhere in this process
    // (including `knowledge::*` call sites) flows through the same
    // global recorder, so the exposition body at either path is
    // identical. The aggregator (Step 12) `connect()`s through the
    // symlink and `sy mon doctor` (Step 21) sees a healthy
    // `knowledge` plane.
    #[cfg(feature = "mon-exporter")]
    let _knowledge_mon_exporter_symlink =
        match install_knowledge_metrics_symlink(_aiplane_mon_exporter.as_ref().map(|g| g.path())) {
            Ok(Some(p)) => {
                tracing::info!(
                    target: "sy::knowledge::daemon",
                    path = %p.display(),
                    "knowledge mon-exporter symlink installed"
                );
                Some(p)
            }
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(
                    target: "sy::knowledge::daemon",
                    error = %format!("{e:#}"),
                    "knowledge mon-exporter symlink failed; continuing without metrics socket"
                );
                None
            }
        };

    let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonOp>();

    // Queue the schema-v2 migration detected above. Reuses the existing
    // `FullResync` machinery (drop + recreate-at-v2 + re-embed); the op lands
    // in `daemon_rx` and is honoured on the first loop iteration. Resumable:
    // re-index is idempotent on point id.
    if migrate_schema {
        let _ = daemon_tx.send(DaemonOp::Ipc(ipc::Op::FullResync));
    }

    // User-controlled flags shared with the per-pass `RunCtx` and with the
    // IPC bridge thread (so Pause / Cancel take effect *during* a pass —
    // the main loop is blocked inside run_index while a pass runs and
    // would otherwise only honour these on the next tick).
    let paused = Arc::new(AtomicBool::new(false));
    let cancel = Arc::new(AtomicBool::new(false));

    // IPC bridge: translate ipc::Op → DaemonOp::Ipc, and side-channel
    // control ops (Pause/Resume/TogglePause/Cancel) directly into the
    // shared atomics so an in-flight pass cancels immediately. The
    // second channel (req_rx) carries request-response ops; we spawn
    // a worker below that owns it.
    let (ipc_tx, ipc_rx) = mpsc::channel::<ipc::Op>();
    let (req_tx, req_rx) = mpsc::channel::<(ipc::Req, tokio::sync::oneshot::Sender<ipc::Resp>)>();
    ipc::serve(ipc_tx, req_tx, cancel.clone()).context("ipc serve")?;
    spawn_req_worker(req_rx);
    let bridge_tx = daemon_tx.clone();
    let bridge_paused = paused.clone();
    let bridge_cancel = cancel.clone();
    thread::spawn(move || {
        while let Ok(op) = ipc_rx.recv() {
            match &op {
                ipc::Op::Pause => {
                    bridge_paused.store(true, Ordering::SeqCst);
                    bridge_cancel.store(true, Ordering::SeqCst);
                    tracing::info!(
                        target: "sy::knowledge::daemon",
                        "paused (cancelling in-flight pass)"
                    );
                }
                ipc::Op::Resume => {
                    if bridge_paused.swap(false, Ordering::SeqCst) {
                        tracing::info!(target: "sy::knowledge::daemon", "resumed");
                        let _ = bridge_tx.send(DaemonOp::Ipc(ipc::Op::IndexNow));
                    }
                }
                ipc::Op::TogglePause => {
                    let now_paused = !bridge_paused.load(Ordering::SeqCst);
                    bridge_paused.store(now_paused, Ordering::SeqCst);
                    if now_paused {
                        bridge_cancel.store(true, Ordering::SeqCst);
                        tracing::info!(
                            target: "sy::knowledge::daemon",
                            "paused (cancelling in-flight pass)"
                        );
                    } else {
                        tracing::info!(target: "sy::knowledge::daemon", "resumed");
                        let _ = bridge_tx.send(DaemonOp::Ipc(ipc::Op::IndexNow));
                    }
                }
                ipc::Op::Cancel => {
                    bridge_cancel.store(true, Ordering::SeqCst);
                    tracing::info!(target: "sy::knowledge::daemon", "cancel requested");
                }
                other => {
                    let _ = bridge_tx.send(DaemonOp::Ipc(other.clone()));
                }
            }
        }
    });

    let mut active_manifests = manifest::discover_all();
    let mut active_folders = enabled_folders(&active_manifests);
    let mut last_pass = PassStats::new();
    let mut interval = parse_schedule_or_default();

    // Heartbeat thread: while a pass is running the main loop is blocked
    // inside `cli::run_index`, so the per-tick status writes don't fire
    // and the file goes stale after ~90s. Waybar reads "daemon down" and
    // hides the tile. The heartbeat re-reads status.json every 3s,
    // refreshes ts_unix + the live qdrant point count, and writes back —
    // keeping the tile visible and showing live progress.
    let heartbeat_paused = paused.clone();
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(3));
            let mut s = match status::load() {
                Ok(s) => s,
                Err(_) => continue,
            };
            if !s.daemon_running {
                // Shutdown was the last write — stop heartbeating.
                return;
            }
            s.ts_unix = state::now_secs();
            s.points = qdrant::point_count().unwrap_or(s.points);
            s.qdrant_ready = qdrant::is_ready();
            s.paused = heartbeat_paused.load(Ordering::SeqCst);
            let _ = status::save(&s);
        }
    });

    // Initial watcher set + initial index pass. We build the watcher
    // first so file events that land between pass-completion and
    // loop-entry aren't lost.
    let watch_handle = Arc::new(parking_lot_like_mutex::Mutex::new(build_watcher_set(
        daemon_tx.clone(),
        &active_manifests,
    )?));

    let mut last_run = Instant::now();
    let mut last_fs_pass = Instant::now() - FS_TICKLE_FLOOR;
    // The initial catch-up pass runs as the first loop iteration (see
    // `first_pass` below), NOT synchronously here. Gating `READY=1` on a
    // potentially long reprocess (e.g. re-transcribing a large Telegram
    // export) would blow `TimeoutStartSec` and trip a systemd restart loop
    // that never converges — the daemon would be killed mid-pass, restart,
    // and re-start the same long pass forever.
    let mut first_pass = true;

    let shutdown = Arc::new(AtomicBool::new(false));
    install_signal_handlers(shutdown.clone());

    // SPEC §4.5 / arch-supervision Step 4: announce `READY=1` once qdrant is
    // up, the IPC socket is bound, and the aiplane workers have loaded —
    // BEFORE the (potentially long) initial index pass, which runs as the
    // first iteration of the loop below. After this point `systemctl --user
    // status sy-knowledge.service` shows `active (running)` and the
    // `WatchdogSec=30s` timer arms. The watchdog ping thread fires
    // unconditionally (returns `None` on non-notify dev hosts, so the dev run
    // incurs no extra thread) and keeps pinging throughout the initial pass.
    sy_core::notify::ready();
    let _watchdog = sy_core::notify::spawn_watchdog();

    tracing::info!(
        target: "sy::knowledge::daemon",
        qdrant_url = %qdrant::base_url(),
        schedule_secs = interval.as_secs(),
        manifests = active_manifests.iter().filter(|m| m.enabled).count(),
        throttle_ms = sources::cpu_throttle().as_millis() as u64,
        cpu_cap = %sources::cpu_max_percent().map(|p| format!("{p}%")).unwrap_or_else(|| "off".into()),
        "daemon ready"
    );

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        let mut want_index_user = false;
        let mut want_index_fs = false;
        let mut want_rescan = false;
        let mut want_refresh = false;
        let mut want_full_resync = false;
        let mut want_schedule_reload = false;
        let mut want_pause = false;
        let mut want_resume = false;
        let mut want_toggle_pause = false;
        let mut want_cancel = false;
        let mut want_shutdown = false;

        match daemon_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(op) => apply_op(
                op,
                &mut want_index_user,
                &mut want_index_fs,
                &mut want_rescan,
                &mut want_refresh,
                &mut want_full_resync,
                &mut want_schedule_reload,
                &mut want_pause,
                &mut want_resume,
                &mut want_toggle_pause,
                &mut want_cancel,
                &mut want_shutdown,
            ),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(op) = daemon_rx.try_recv() {
            apply_op(
                op,
                &mut want_index_user,
                &mut want_index_fs,
                &mut want_rescan,
                &mut want_refresh,
                &mut want_full_resync,
                &mut want_schedule_reload,
                &mut want_pause,
                &mut want_resume,
                &mut want_toggle_pause,
                &mut want_cancel,
                &mut want_shutdown,
            );
        }

        if want_shutdown {
            break;
        }
        // First loop iteration performs the initial catch-up pass. Moved out
        // of startup (before `READY=1`) so a large reprocess runs in the
        // background instead of gating service-start under `TimeoutStartSec`.
        // Treated as user-driven (unthrottled) to match the old eager pass.
        if first_pass {
            want_index_user = true;
            first_pass = false;
        }
        // Pause / Resume / TogglePause / Cancel are handled directly in
        // the IPC bridge thread (so they take effect *during* a pass).
        // The main loop just observes the resulting atomic flags below.
        let _ = (want_pause, want_resume, want_toggle_pause, want_cancel);

        if want_schedule_reload {
            interval = parse_schedule_or_default();
            tracing::info!(
                target: "sy::knowledge::daemon",
                schedule_secs = interval.as_secs(),
                "schedule reloaded"
            );
        }
        if want_rescan {
            // Re-walk discovery roots; figure out which folders went away
            // (or got disabled) and drop their points from qdrant.
            let new_manifests = manifest::discover_all();
            let new_folders = enabled_folders(&new_manifests);
            let added: Vec<PathBuf> = new_folders.difference(&active_folders).cloned().collect();
            let retired: Vec<PathBuf> = active_folders.difference(&new_folders).cloned().collect();
            for r in &retired {
                let label = r.display().to_string();
                if let Err(e) = qdrant::delete_by_source(&label) {
                    tracing::error!(
                        target: "sy::knowledge::daemon",
                        label = %label,
                        error = %e,
                        "delete_by_source failed"
                    );
                } else {
                    tracing::info!(
                        target: "sy::knowledge::daemon",
                        label = %label,
                        "retired manifest"
                    );
                }
                purge_index_subtree(r);
            }
            for a in &added {
                tracing::info!(
                    target: "sy::knowledge::daemon",
                    path = %a.display(),
                    "discovered manifest"
                );
            }
            active_manifests = new_manifests;
            active_folders = new_folders;
            // Only kick a watcher rebuild + immediate pass when the
            // manifest set actually changed — otherwise rescan-on-tickle
            // would loop forever (rebuilding watchers can synthesise
            // events under $HOME, which fires DiscoveryTickle again).
            if !added.is_empty() || !retired.is_empty() {
                want_refresh = true;
                want_index_user = true;
                // Manifest-count changed → push a fresh status to waybar.
                save_snapshot(
                    false,
                    paused.load(Ordering::SeqCst),
                    false,
                    &last_pass,
                    interval,
                    last_run,
                    &active_manifests,
                );
            }
        }
        if want_refresh {
            match build_watcher_set(daemon_tx.clone(), &active_manifests) {
                Ok(w) => *watch_handle.lock() = w,
                Err(e) => tracing::error!(
                    target: "sy::knowledge::daemon",
                    error = %e,
                    "rebuild watchers failed"
                ),
            }
        }
        // Skip pass-firing while paused. FS-tickles still set `want_index_fs`,
        // but we don't honour them — on resume the catch-up `IndexNow` op
        // queued by the IPC bridge will re-walk and pick up everything.
        if paused.load(Ordering::SeqCst) {
            // Refresh the status TS so the waybar tile stays fresh
            // (>90s stale = "down"). Reflect the actual paused flag.
            save_snapshot(
                false,
                true,
                false,
                &last_pass,
                interval,
                last_run,
                &active_manifests,
            );
        } else if want_full_resync {
            let _ = run_full_resync(
                &mut last_pass,
                interval,
                last_run,
                &active_manifests,
                &paused,
                &cancel,
            );
            last_run = Instant::now();
            last_fs_pass = Instant::now();
        } else {
            let scheduled_due = last_run.elapsed() >= interval;
            let fs_due = want_index_fs && last_fs_pass.elapsed() >= FS_TICKLE_FLOOR;
            if want_index_user || scheduled_due || fs_due {
                let throttle = scheduled_due || fs_due; // never throttle user-driven
                let _ = run_one_pass(
                    false,
                    throttle,
                    &mut last_pass,
                    interval,
                    last_run,
                    &active_manifests,
                    &paused,
                    &cancel,
                );
                last_run = Instant::now();
                if fs_due {
                    last_fs_pass = Instant::now();
                }
            }
        }
    }

    // SPEC §4.5 Step 4: emit `STOPPING=1 STATUS="draining"` before
    // teardown so siblings linked via `BindsTo=sy-qdrant.service`
    // see a clean shutdown rather than `Result=signal`.
    sy_core::notify::stopping();
    tracing::info!(target: "sy::knowledge::daemon", "shutting down");
    write_shutdown_status(&last_pass, interval, last_run, &active_manifests);
    if let Some(supv) = crate::aiplane::supervisor::current() {
        tracing::info!(target: "sy::knowledge::daemon", "stopping aiplane workers");
        supv.shutdown(Duration::from_secs(5));
    }
    if let Some(mut child) = qdrant_child {
        shutdown_qdrant(&mut child);
    }
    let _ = std::fs::remove_file(ipc::socket_path());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_op(
    op: DaemonOp,
    want_index_user: &mut bool,
    want_index_fs: &mut bool,
    want_rescan: &mut bool,
    want_refresh: &mut bool,
    want_full_resync: &mut bool,
    want_schedule_reload: &mut bool,
    want_pause: &mut bool,
    want_resume: &mut bool,
    want_toggle_pause: &mut bool,
    want_cancel: &mut bool,
    want_shutdown: &mut bool,
) {
    match op {
        DaemonOp::Ipc(ipc::Op::IndexNow) => *want_index_user = true,
        DaemonOp::Ipc(ipc::Op::RefreshSources) => {
            *want_refresh = true;
            *want_rescan = true;
            *want_index_user = true;
        }
        DaemonOp::Ipc(ipc::Op::FullResync) => *want_full_resync = true,
        DaemonOp::Ipc(ipc::Op::ReloadSchedule) => *want_schedule_reload = true,
        DaemonOp::Ipc(ipc::Op::RescanDiscovery) => *want_rescan = true,
        DaemonOp::Ipc(ipc::Op::Pause) => *want_pause = true,
        DaemonOp::Ipc(ipc::Op::Resume) => *want_resume = true,
        DaemonOp::Ipc(ipc::Op::TogglePause) => *want_toggle_pause = true,
        DaemonOp::Ipc(ipc::Op::Cancel) => *want_cancel = true,
        DaemonOp::Ipc(ipc::Op::Shutdown) => *want_shutdown = true,
        DaemonOp::FsTickle => *want_index_fs = true,
        DaemonOp::DiscoveryTickle => *want_rescan = true,
    }
}

/// Decide whether daemon startup should queue a `FullResync` to migrate the
/// live collection to schema v2 (named `dense`+`sparse`). Pure so the trigger
/// is unit-testable without a live qdrant.
///
/// `info` is the parsed collection-info body, or `None` when the collection is
/// absent / qdrant unreachable. A missing collection needs no migration: it is
/// (re)created directly at v2 by `ensure_collection`. A present collection that
/// predates the named-vector schema must be dropped + re-embedded.
fn schema_migration_needed(info: Option<&serde_json::Value>) -> bool {
    info.is_some_and(qdrant::schema_is_pre_v2)
}

/// Build the "qdrant too old" startup warning, or `None` when the live
/// version is adequate (≥ `MIN_HYBRID_VERSION`) or unknown. Pure so the
/// boundary is unit-testable without a live qdrant.
///
/// Below 1.16 qdrant silently ignores the hybrid query's configurable RRF
/// `k` (Step 5), so hybrid search regresses without any error. We warn
/// loudly rather than refuse to start: a degraded daemon that still serves
/// dense-only search is safer than a daemon that won't boot, and the
/// operator gets an unmissable journal line pointing at `sy apply`.
fn qdrant_version_warning(server: Option<(u32, u32)>) -> Option<String> {
    let v = server?;
    if qdrant::meets_min_version(v, qdrant::MIN_HYBRID_VERSION) {
        return None;
    }
    let (min_major, min_minor) = qdrant::MIN_HYBRID_VERSION;
    Some(format!(
        "live qdrant is {}.{} but hybrid RRF `k` requires qdrant ≥ {min_major}.{min_minor}; \
         hybrid search will silently regress — run `sy apply` to upgrade qdrant",
        v.0, v.1
    ))
}

fn enabled_folders(manifests: &[manifest::QdrManifest]) -> HashSet<PathBuf> {
    manifests
        .iter()
        .filter(|m| m.enabled)
        .map(|m| m.folder.clone())
        .collect()
}

fn purge_index_subtree(folder: &Path) {
    let mut idx = match state::load() {
        Ok(i) => i,
        Err(_) => return,
    };
    let prefix = folder.display().to_string();
    let stale: Vec<String> = idx
        .files
        .keys()
        .filter(|k| k.starts_with(&prefix))
        .cloned()
        .collect();
    for k in stale {
        idx.files.remove(&k);
    }
    let _ = state::save(&idx);
}

/// Last-finished pass stats fed into the status snapshot.
struct PassStats {
    at_unix: u64,
    ms: u64,
    indexed: usize,
    skipped: usize,
    deleted: usize,
    chunks: usize,
    throughput: Option<f32>,
    error: Option<String>,
}

impl PassStats {
    fn new() -> Self {
        Self {
            at_unix: 0,
            ms: 0,
            indexed: 0,
            skipped: 0,
            deleted: 0,
            chunks: 0,
            throughput: None,
            error: None,
        }
    }
}

fn build_status(
    indexing: bool,
    last: &PassStats,
    interval: Duration,
    last_run: Instant,
    active_manifests: &[manifest::QdrManifest],
) -> status::Status {
    let now = state::now_secs();
    let elapsed_secs = last_run.elapsed().as_secs();
    let interval_secs = interval.as_secs();
    let next_run_unix = if elapsed_secs >= interval_secs {
        now
    } else {
        now + (interval_secs - elapsed_secs)
    };
    let section = sources::load().unwrap_or_default();
    let sources_explicit = section
        .sources
        .iter()
        .filter(|s| s.enabled && s.mode == SourceMode::Explicit)
        .count();
    let sources_discover = section
        .sources
        .iter()
        .filter(|s| s.enabled && s.mode == SourceMode::Discover)
        .count();
    let manifests_active = active_manifests.iter().filter(|m| m.enabled).count();
    let manifests_disabled = active_manifests.len() - manifests_active;
    let points = qdrant::point_count().unwrap_or(0);
    let qdrant_ready = qdrant::is_ready();
    status::Status {
        ts_unix: now,
        daemon_running: true,
        qdrant_ready,
        schedule_secs: interval_secs,
        next_run_unix,
        sources_explicit,
        sources_discover,
        manifests_active,
        manifests_disabled,
        points,
        indexing,
        paused: false,     // overwritten by callers that know the flag
        cancelling: false, // overwritten by callers
        embed_backend: embed::current_backend().to_string(),
        embed_hardware: embed::current_hardware(),
        last_throughput_chunks_per_s: last.throughput,
        cpu_max_percent: sources::cpu_max_percent(),
        last_index_at_unix: last.at_unix,
        last_index_ms: last.ms,
        last_index_indexed: last.indexed,
        last_index_skipped: last.skipped,
        last_index_deleted: last.deleted,
        last_index_chunks: last.chunks,
        last_error: last.error.clone(),
        // Per-workload health, one row per kind the supervisor is
        // managing. Empty until the supervisor has performed at
        // least one health poll (sub-second after init).
        workloads: supervisor_health(),
        // SPEC §4.3 admission caps + live per-class depths.
        queue_caps: status::default_queue_caps(),
        queue_depths: supervisor_queue_depths(),
        // SPEC §4.3 warm-pool roster — empty until the supervisor
        // has touched at least one workload (boot-time `ensure`).
        warm_models: supervisor_warm_models(),
        // SPEC §4.3 inflight count: workers currently mid-RunBatch.
        inflight: supervisor_inflight(),
    }
}

/// Snapshot the supervisor's warm-pool roster for the status writer.
/// Returns the empty list when the supervisor isn't running so
/// legacy CPU-fallback callers (Zone 1) don't crash.
fn supervisor_warm_models() -> Vec<String> {
    crate::aiplane::supervisor::current()
        .map(|s| s.warm_models())
        .unwrap_or_default()
}

/// Count workers currently mid-`RunBatch` (SPEC §4.3 NPU `inflight`).
/// Derived from the supervisor's last poll of each child's
/// `WorkerHealth.inflight_request_id`. 0 when the supervisor isn't
/// running or no children are loaded.
fn supervisor_inflight() -> usize {
    let Some(sup) = crate::aiplane::supervisor::current() else {
        return 0;
    };
    sup.all_health()
        .into_values()
        .filter(|h| {
            h.as_ref()
                .and_then(|wh| wh.inflight_request_id.as_ref())
                .is_some()
        })
        .count()
}

/// Snapshot the per-class scheduler queue depths for the status
/// writer. Reads the bridge's `Scheduler::queue_depths()` via the
/// process-wide handle installed by the IPC bridge at boot. Returns
/// the default all-zeros map when no bridge is running (e.g. CLI
/// fallback path, daemon shutting down).
fn supervisor_queue_depths() -> std::collections::HashMap<String, usize> {
    match crate::aiplane::ipc::current_scheduler() {
        Some(s) => s
            .queue_depths()
            .into_iter()
            .map(|(k, v)| (k.as_str().to_string(), v))
            .collect(),
        None => status::default_queue_depths(),
    }
}

/// Convert the supervisor's `HashMap<WorkloadKind, Option<WorkerHealth>>`
/// into the status snapshot's `HashMap<String, WorkloadHealth>` shape.
/// `None` (no poll yet) maps to a `Loading` row so the waybar can
/// distinguish "worker not yet ready" from "worker not registered".
fn supervisor_health() -> std::collections::HashMap<String, crate::aiplane::registry::WorkloadHealth>
{
    use crate::aiplane::registry::{WorkloadHealth, WorkloadState};
    let Some(sup) = crate::aiplane::supervisor::current() else {
        return std::collections::HashMap::new();
    };
    sup.all_health()
        .into_iter()
        .map(|(k, maybe_h)| {
            let h = match maybe_h {
                Some(wh) => WorkloadHealth {
                    state: wh.state.clone(),
                    loaded: wh.state.is_ready(),
                    last_call_unix: wh.ready_at_unix,
                    ema_ms: wh.ema_ms,
                    calls: wh.calls,
                    errors: wh.errors,
                    backend: match &wh.state {
                        WorkloadState::Ready { backend } => backend.clone(),
                        _ => String::new(),
                    },
                },
                None => WorkloadHealth {
                    state: WorkloadState::Loading,
                    ..Default::default()
                },
            };
            (k.as_str().to_string(), h)
        })
        .collect()
}

/// Spin up the aiplane supervisor, spawn embed + rerank worker
/// children, block until each reaches `Ready`, install the
/// process-shared handle, and start a background poll thread.
/// Returns `Err(_)` if any of the configured workers fails to load —
/// the daemon's caller catches that and falls back to the legacy
/// in-process path (search keeps working on the embed side via
/// `knowledge::embed::embed_one`; rerank goes unavailable).
fn init_aiplane_supervisor() -> Result<()> {
    use crate::aiplane::registry::WorkloadKind;
    use crate::aiplane::supervisor::{self, Supervisor};
    use std::sync::Arc;

    let supv = Arc::new(Supervisor::new());

    // Worker startup deadline: 30 min covers a first-time VAIP
    // compile from a cold cache (xlm-roberta-large at (32, 512)
    // measured ~12–15 min of AIE codegen end-to-end). Warm-cache
    // loads finish in seconds; the long budget only matters on the
    // first install or after a cache wipe. If you hit this deadline,
    // run `prep_npu_workload.py` manually so it warms the cache
    // outside the daemon's hot path.
    let ready_deadline = Duration::from_secs(1800);
    for kind in [WorkloadKind::Embed, WorkloadKind::Rerank] {
        tracing::info!(
            target: "sy::knowledge::daemon",
            kind = %kind,
            deadline_secs = ready_deadline.as_secs(),
            "ensuring worker is Ready"
        );
        match supv.ensure(kind, ready_deadline) {
            Ok(h) => {
                let backend = match &h.state {
                    crate::aiplane::registry::WorkloadState::Ready { backend } => backend.as_str(),
                    _ => "?",
                };
                tracing::info!(
                    target: "sy::knowledge::daemon",
                    kind = %kind,
                    pid = h.pid,
                    backend = %backend,
                    "worker Ready"
                );
            }
            Err(e) => {
                supv.shutdown(Duration::from_secs(5));
                return Err(e);
            }
        }
    }

    // Background poll: every second, probe each child's Health and
    // detect dead children for restart. Keeps the daemon's status
    // snapshot fresh and recovers from worker crashes without
    // user intervention.
    let supv_for_poll = supv.clone();
    thread::spawn(move || {
        loop {
            supv_for_poll.poll_once();
            thread::sleep(Duration::from_secs(1));
        }
    });

    supervisor::set_current(supv);
    Ok(())
}

/// Write a status snapshot to disk, blending the indexing flag, the
/// daemon-owned `paused` atomic, and the most recent PassStats.
fn save_snapshot(
    indexing: bool,
    paused_flag: bool,
    cancelling_flag: bool,
    last: &PassStats,
    interval: Duration,
    last_run: Instant,
    active_manifests: &[manifest::QdrManifest],
) {
    let mut s = build_status(indexing, last, interval, last_run, active_manifests);
    s.paused = paused_flag;
    s.cancelling = cancelling_flag;
    let _ = status::save(&s);
}

fn write_shutdown_status(
    last: &PassStats,
    interval: Duration,
    last_run: Instant,
    active_manifests: &[manifest::QdrManifest],
) {
    let mut s = build_status(false, last, interval, last_run, active_manifests);
    s.daemon_running = false;
    let _ = status::save(&s);
}

#[allow(clippy::too_many_arguments)]
fn run_one_pass(
    quiet: bool,
    throttle: bool,
    last: &mut PassStats,
    interval: Duration,
    last_run: Instant,
    active_manifests: &[manifest::QdrManifest],
    paused: &Arc<AtomicBool>,
    cancel: &Arc<AtomicBool>,
) -> Result<()> {
    cancel.store(false, Ordering::SeqCst);
    let throttle_d = if throttle {
        sources::cpu_throttle()
    } else {
        Duration::ZERO
    };
    let ctx = RunCtx::for_daemon_pass(cancel.clone(), throttle_d);
    save_snapshot(
        true,
        paused.load(Ordering::SeqCst),
        false,
        last,
        interval,
        last_run,
        active_manifests,
    );
    let mut idx = state::load().unwrap_or_default();
    match cli::run_index(&mut idx, None, false, &ctx) {
        Ok(report) => {
            idx.last_sync_unix = state::now_secs();
            let _ = state::save(&idx);
            last.at_unix = state::now_secs();
            last.ms = report.elapsed_ms as u64;
            last.indexed = report.indexed;
            last.skipped = report.skipped;
            last.deleted = report.deleted;
            last.chunks = report.chunks;
            last.throughput = throughput(report.chunks, report.elapsed_ms);
            last.error = None;
            if !quiet && report.scanned > 0 {
                let cancelled = ctx.cancelled();
                tracing::info!(
                    target: "sy::knowledge::daemon",
                    scanned = report.scanned,
                    indexed = report.indexed,
                    skipped = report.skipped,
                    deleted = report.deleted,
                    latency_ms = report.elapsed_ms as u64,
                    throttled = throttle,
                    cancelled = cancelled,
                    "index pass complete"
                );
            }
        }
        Err(e) => {
            last.error = Some(format!("{e}"));
            last.at_unix = state::now_secs();
            tracing::error!(
                target: "sy::knowledge::daemon",
                error = %e,
                "index pass failed"
            );
        }
    }
    cancel.store(false, Ordering::SeqCst);
    save_snapshot(
        false,
        paused.load(Ordering::SeqCst),
        false,
        last,
        interval,
        last_run,
        active_manifests,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_full_resync(
    last: &mut PassStats,
    interval: Duration,
    last_run: Instant,
    active_manifests: &[manifest::QdrManifest],
    paused: &Arc<AtomicBool>,
    cancel: &Arc<AtomicBool>,
) -> Result<()> {
    cancel.store(false, Ordering::SeqCst);
    let ctx = RunCtx::for_daemon_pass(cancel.clone(), Duration::ZERO);
    save_snapshot(
        true,
        paused.load(Ordering::SeqCst),
        false,
        last,
        interval,
        last_run,
        active_manifests,
    );
    if let Err(e) = qdrant::recreate_collection() {
        last.error = Some(format!("recreate_collection: {e}"));
        tracing::error!(
            target: "sy::knowledge::daemon",
            error = %e,
            "full resync failed (recreate_collection)"
        );
        save_snapshot(
            false,
            paused.load(Ordering::SeqCst),
            false,
            last,
            interval,
            last_run,
            active_manifests,
        );
        return Err(e);
    }
    let mut idx = state::Index::default();
    match cli::run_index(&mut idx, None, true, &ctx) {
        Ok(report) => {
            idx.last_sync_unix = state::now_secs();
            let _ = state::save(&idx);
            last.at_unix = state::now_secs();
            last.ms = report.elapsed_ms as u64;
            last.indexed = report.indexed;
            last.skipped = report.skipped;
            last.deleted = report.deleted;
            last.chunks = report.chunks;
            last.throughput = throughput(report.chunks, report.elapsed_ms);
            last.error = None;
            tracing::info!(
                target: "sy::knowledge::daemon",
                chunks = report.chunks,
                latency_ms = report.elapsed_ms as u64,
                "full resync done"
            );
        }
        Err(e) => {
            last.error = Some(format!("{e}"));
            last.at_unix = state::now_secs();
            tracing::error!(
                target: "sy::knowledge::daemon",
                error = %e,
                "full resync failed"
            );
        }
    }
    cancel.store(false, Ordering::SeqCst);
    save_snapshot(
        false,
        paused.load(Ordering::SeqCst),
        false,
        last,
        interval,
        last_run,
        active_manifests,
    );
    Ok(())
}

/// sy-mon Step 20: create `$XDG_RUNTIME_DIR/sy/knowledge/metrics.sock`
/// as a symlink pointing at the aiplane mon-exporter socket bound in
/// the same process. Returns the symlink path on success, `None` when
/// the aiplane bind itself didn't happen (e.g. `XDG_RUNTIME_DIR` unset
/// in a stripped-down dev shell), and `Err` only when the path is
/// computable but the symlink call fails (EACCES on the runtime dir
/// or `sym_target` is itself missing).
///
/// The symlink is best-effort cleaned up on daemon shutdown by the
/// caller binding the return value into a guard — but it lives under
/// `$XDG_RUNTIME_DIR/sy/knowledge/`, which is a tmpfs subtree, so a
/// stale symlink across a reboot is harmless.
#[cfg(feature = "mon-exporter")]
fn install_knowledge_metrics_symlink(sym_target: Option<&Path>) -> Result<Option<PathBuf>> {
    let Some(target) = sym_target else {
        return Ok(None);
    };
    let link = match crate::mon_exporter::socket_path_for("knowledge") {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("mkdir -p {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(parent) {
                let mut perms = meta.permissions();
                perms.set_mode(0o700);
                let _ = std::fs::set_permissions(parent, perms);
            }
        }
    }
    // Replace any stale link/file from a previous run; we want the
    // current daemon's UDS, not a dangling pointer.
    let _ = std::fs::remove_file(&link);
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, &link)
        .with_context(|| format!("symlink {} -> {}", link.display(), target.display()))?;
    Ok(Some(link))
}

fn throughput(chunks: usize, ms: u128) -> Option<f32> {
    if chunks == 0 || ms == 0 {
        None
    } else {
        Some((chunks as f32) * 1000.0 / (ms as f32))
    }
}

fn set_process_priority() {
    let nice = sources::nice_level();
    // SAFETY: setpriority(2) is async-signal-safe. PRIO_PROCESS=0 + who=0
    // means "this process". Failure is non-fatal.
    unsafe {
        let _ = libc::setpriority(libc::PRIO_PROCESS, 0, nice);
    }
    // Best-effort ionice idle-class. Class 3 = idle (man ioprio_set).
    // We use the syscall directly to avoid an extra dep; failure is silent.
    const SYS_IOPRIO_SET: libc::c_long = 251; // x86_64
    const IOPRIO_WHO_PROCESS: libc::c_int = 1;
    const IOPRIO_CLASS_IDLE: libc::c_int = 3;
    let prio = (IOPRIO_CLASS_IDLE << 13) as libc::c_int; // class shifted into the high bits
    unsafe {
        let _ = libc::syscall(SYS_IOPRIO_SET, IOPRIO_WHO_PROCESS, 0, prio);
    }
    tracing::info!(
        target: "sy::knowledge::daemon",
        nice,
        ionice = "idle",
        "process priority applied"
    );
}

fn parse_schedule_or_default() -> Duration {
    let s = sources::schedule_interval();
    let secs = sources::parse_interval(&s).unwrap_or(900);
    Duration::from_secs(secs)
}

/// Build the hybrid watcher set: shallow-`$HOME` (NonRecursive) +
/// discover roots (Recursive) + explicit sources (Recursive) + each
/// enabled manifest folder (Recursive). One debouncer owns all watches.
fn build_watcher_set(
    tx: mpsc::Sender<DaemonOp>,
    manifests: &[manifest::QdrManifest],
) -> Result<notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>> {
    // Captured so the watcher closure can recognise events that landed
    // *inside* `$HOME` and trigger a rescan — a brand-new top-level dir
    // doesn't yet have its own non-recursive watch, so we use shallow-home
    // events as a "topology changed, re-walk" signal.
    let home_path: Option<PathBuf> = std::env::var("HOME").ok().map(PathBuf::from);
    let mut debouncer = new_debouncer(
        Duration::from_secs(1),
        move |res: notify_debouncer_mini::DebounceEventResult| {
            let events = match res {
                Ok(e) => e,
                Err(_) => return,
            };
            let mut saw_qdr = false;
            let mut saw_other = false;
            let mut saw_home_topology = false;
            for ev in &events {
                if ev.path.file_name().and_then(|n| n.to_str()) == Some(manifest::MANIFEST_FILENAME)
                {
                    saw_qdr = true;
                } else {
                    saw_other = true;
                }
                if let Some(home) = &home_path {
                    if let Some(parent) = ev.path.parent() {
                        if parent == home.as_path() {
                            saw_home_topology = true;
                        }
                    }
                }
            }
            if saw_qdr || saw_home_topology {
                let _ = tx.send(DaemonOp::DiscoveryTickle);
            }
            if saw_other {
                let _ = tx.send(DaemonOp::FsTickle);
            }
        },
    )
    .context("notify debouncer")?;

    let watcher = debouncer.watcher();

    if sources::discover_home_enabled() {
        if let Ok(home) = std::env::var("HOME") {
            let home = PathBuf::from(home);
            if home.is_dir() {
                let _ = watcher.watch(&home, RecursiveMode::NonRecursive);
                if let Ok(rd) = std::fs::read_dir(&home) {
                    for ent in rd.flatten() {
                        let p = ent.path();
                        if p.is_dir() {
                            let _ = watcher.watch(&p, RecursiveMode::NonRecursive);
                        }
                    }
                }
            }
        }
    }

    for r in sources::discover_roots().unwrap_or_default() {
        if r.exists() {
            if let Err(e) = watcher.watch(&r, RecursiveMode::Recursive) {
                tracing::error!(
                    target: "sy::knowledge::daemon",
                    source = "discover",
                    path = %r.display(),
                    error = %e,
                    "watch registration failed"
                );
            }
        }
    }

    for r in sources::enabled_paths().unwrap_or_default() {
        if r.exists() {
            if let Err(e) = watcher.watch(&r, RecursiveMode::Recursive) {
                tracing::error!(
                    target: "sy::knowledge::daemon",
                    source = "explicit",
                    path = %r.display(),
                    error = %e,
                    "watch registration failed"
                );
            }
        }
    }

    for m in manifests.iter().filter(|m| m.enabled) {
        if m.folder.exists() {
            if let Err(e) = watcher.watch(&m.folder, RecursiveMode::Recursive) {
                tracing::error!(
                    target: "sy::knowledge::daemon",
                    source = "manifest",
                    path = %m.folder.display(),
                    error = %e,
                    "watch registration failed"
                );
            }
        }
    }

    Ok(debouncer)
}

fn spawn_qdrant() -> Result<Child> {
    let storage = state::qdrant_storage_dir()?;
    let log_path = state::qdrant_log_path()?;
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;

    // qdrant reads QDRANT__SERVICE__HTTP_PORT and QDRANT__STORAGE__STORAGE_PATH
    // from env. Bind localhost only.
    let child = Command::new(qdrant_binary()?)
        .env("QDRANT__SERVICE__HTTP_PORT", QDRANT_PORT.to_string())
        .env("QDRANT__SERVICE__HOST", "127.0.0.1")
        .env("QDRANT__STORAGE__STORAGE_PATH", &storage)
        .env("QDRANT__STORAGE__SNAPSHOTS_PATH", storage.join("snapshots"))
        .env("QDRANT__TELEMETRY_DISABLED", "true")
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .context("spawn qdrant")?;
    Ok(child)
}

fn qdrant_binary() -> Result<PathBuf> {
    if let Ok(home) = std::env::var("HOME") {
        let p = Path::new(&home).join(".local/bin/qdrant");
        if p.exists() {
            return Ok(p);
        }
    }
    if crate::which("qdrant") {
        return Ok(PathBuf::from("qdrant"));
    }
    Err(super::KnowledgeError {
        code: super::exit::QDRANT_UNREACHABLE,
        msg: "qdrant binary not found — run `sy apply` to download it".into(),
    }
    .into())
}

fn shutdown_qdrant(child: &mut Child) {
    use std::os::unix::process::ExitStatusExt;
    let pid = child.id() as i32;
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(_) => break,
        }
        if Instant::now() > deadline {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _: std::process::ExitStatus = std::process::ExitStatus::from_raw(0);
}

fn install_signal_handlers(flag: Arc<AtomicBool>) {
    use std::os::raw::c_int;
    extern "C" fn handler(_: c_int) {
        SIGNAL_RECEIVED.store(true, Ordering::SeqCst);
    }
    unsafe {
        libc::signal(libc::SIGTERM, handler as *const () as usize);
        libc::signal(libc::SIGINT, handler as *const () as usize);
    }
    thread::spawn(move || {
        loop {
            if SIGNAL_RECEIVED.load(Ordering::SeqCst) {
                flag.store(true, Ordering::SeqCst);
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
}

static SIGNAL_RECEIVED: AtomicBool = AtomicBool::new(false);

/// Dedicated worker for request-response IPC. Owned channel + a
/// thread-pool-of-one suffices: NPU only handles one inference at a
/// time anyway (the underlying `Embedder` is a `Mutex<...>` in
/// embed.rs), and we don't want a flood of search requests to head-of-line
/// block the daemon's own indexing pass.
fn spawn_req_worker(req_rx: mpsc::Receiver<(ipc::Req, tokio::sync::oneshot::Sender<ipc::Resp>)>) {
    thread::spawn(move || {
        while let Ok((req, tx)) = req_rx.recv() {
            let resp = handle_req(req);
            // The receiving end is the IPC v1 bridge handler awaiting
            // the oneshot. If the connection went away mid-call the
            // send fails — that's expected, not an error worth logging.
            let _ = tx.send(resp);
        }
    });
}

fn handle_req(req: ipc::Req) -> ipc::Resp {
    match req {
        ipc::Req::Search {
            query,
            limit,
            prefix,
            priority,
            filter,
            abstain_threshold: _,
        } => {
            let vec = match embed::embed_one(&query, priority) {
                Ok(v) => v,
                Err(e) => {
                    return ipc::Resp::Error {
                        msg: format!("embed: {e}"),
                    };
                }
            };
            // Apply the SAME structured pre-filter the rerank path uses, so the
            // embed-only path honours `exclude_kinds` (the self-poisoning
            // default-exclude) / `from` / dates instead of dropping them.
            let mut filter = filter.unwrap_or_default();
            let today = chrono::Local::now().date_naive();
            query::maybe_fill_dates(&mut filter, &query, today);
            let pre_filter = qdrant::build_filter(&filter, prefix.as_deref());
            match qdrant::search_with_filter(&vec, limit, pre_filter.as_ref()) {
                Ok(hits) => ipc::Resp::Search {
                    hits: hits
                        .into_iter()
                        .map(|h| ipc::HitRow {
                            score: h.score,
                            chunk_id: crate::knowledge::chunk::point_id(
                                &h.payload.file_path,
                                h.payload.chunk_index,
                            ),
                            file_path: h.payload.file_path,
                            chunk_index: h.payload.chunk_index,
                            chunk_text: h.payload.chunk_text,
                            embed_score: None,
                        })
                        .collect(),
                    confidence: ipc::default_search_confidence(),
                    abstained: false,
                },
                Err(e) => ipc::Resp::Error {
                    msg: format!("qdrant search: {e}"),
                },
            }
        }
        ipc::Req::SearchRerank {
            query,
            limit,
            prefix,
            candidates,
            priority,
            // Step 7 compiles `filter` (+ the Step 5 `prefix`) into a qdrant
            // pre-filter inside `handle_search_rerank`. Step 12 applies
            // `abstain_threshold` to the calibrated confidence there.
            filter,
            abstain_threshold,
        } => handle_search_rerank(
            query,
            limit,
            prefix.as_deref(),
            candidates,
            priority,
            &filter.unwrap_or_default(),
            abstain_threshold,
        ),
        ipc::Req::GetChunk { chunk_id } => match qdrant::get_point(&chunk_id) {
            Ok(Some(p)) => ipc::Resp::Chunk {
                chunk: Some(ipc::ChunkRow {
                    chunk_id,
                    file_path: p.file_path,
                    chunk_index: p.chunk_index,
                    kind: p.kind,
                    source_name: p.source_name,
                    text: p.chunk_text,
                }),
            },
            Ok(None) => ipc::Resp::Chunk { chunk: None },
            Err(e) => ipc::Resp::Error {
                msg: format!("qdrant get_chunk: {e}"),
            },
        },
        ipc::Req::Run { workload, input } => {
            // Every NPU workload runs in its own worker subprocess.
            // The supervisor was started at daemon boot with embed +
            // rerank eagerly Ready; lookup any other kind goes to a
            // not-yet-spawned worker → clean error rather than
            // silent in-process fallback.
            let supv =
                crate::aiplane::supervisor::current().expect("aiplane supervisor must be running");
            match supv.run_batch(workload, vec![input]) {
                Ok(mut outputs) => match outputs.pop() {
                    Some(output) => ipc::Resp::Run { output },
                    None => ipc::Resp::Error {
                        msg: format!("{workload}: worker returned empty batch"),
                    },
                },
                Err(e) => ipc::Resp::Error {
                    msg: format!("{workload}: {e:#}"),
                },
            }
        }
    }
}

/// Two-stage retrieval: embed → qdrant top-`candidates` → bge-reranker
/// scores each pair → sort by rerank score and truncate to `limit`. The
/// rerank pass routes through `aiplane::workloads` (lazy-loaded on
/// first call), so the daemon owns the only NPU session for the
/// reranker model just like it does for embed. If the rerank model
/// isn't prepared on disk, returns an `Error` resp that the CLI/MCP
/// layer can translate into a fallback.
fn handle_search_rerank(
    query: String,
    limit: usize,
    prefix: Option<&str>,
    candidates: usize,
    priority: sy_core::Priority,
    filter: &ipc::SearchFilter,
    abstain_threshold: Option<f32>,
) -> ipc::Resp {
    use crate::aiplane::registry::{WorkloadInput, WorkloadKind, WorkloadOutput};
    use std::time::Instant;

    let t_total = Instant::now();
    let n_candidates = candidates.max(limit).max(1);

    // Stage 1: embed the query (knowledge::embed's hot singleton).
    let t = Instant::now();
    let qvec = match embed::embed_one(&query, priority) {
        Ok(v) => v,
        Err(e) => {
            return ipc::Resp::Error {
                msg: format!("embed: {e}"),
            };
        }
    };
    let ms_embed = t.elapsed().as_secs_f64() * 1000.0;

    // Stage 2: hybrid dense+sparse top-N from qdrant, fused by RRF. The
    // query-side sparse vector mirrors the index-time encoder (Step 4). The
    // structured `SearchFilter` (date range, from/kind/source any-of,
    // include/exclude sources) plus the Step 5 `prefix` file_path text-match
    // are compiled into one qdrant pre-filter applied to both prefetch legs.
    // Synonym expansion is sparse-side only (REQ-7): the dense embedding
    // above used the unmodified `query`; here we OR-in alias tokens from the
    // declarative `~/.config/sy-knowledge/synonyms.yaml` (installed by `sy
    // apply`) so the lexical leg gains precise BM25 matches without polluting
    // dense recall. A missing/empty table is a no-op.
    let qsparse = sparse::encode(&query::expand_synonyms(&query, &query::load_synonyms()));
    // REQ-8 (Step 18): when the caller supplied no date bound, auto-fill the
    // window from RU/EN natural-language time phrases in the query ("новогодние
    // праздники 2024", "in January", "last summer", generic English via
    // two_timer). Explicit `--date-from/--date-to` always win; an unrecognized
    // phrase is a logged no-op. `today` is read here so the expander stays pure.
    let mut filter = filter.clone();
    let today = chrono::Local::now().date_naive();
    query::maybe_fill_dates(&mut filter, &query, today);
    let pre_filter = qdrant::build_filter(&filter, prefix);
    let t = Instant::now();
    let raw_hits = match qdrant::query_hybrid(&qvec, &qsparse, pre_filter.as_ref(), n_candidates) {
        Ok(h) => h,
        Err(e) => {
            return ipc::Resp::Error {
                msg: format!("qdrant hybrid query: {e}"),
            };
        }
    };
    let ms_qdrant = t.elapsed().as_secs_f64() * 1000.0;

    if raw_hits.is_empty() {
        // No candidates → zero confidence; abstain when a threshold was set.
        let confidence = calibrate::confidence(&[]);
        return ipc::Resp::Search {
            hits: Vec::new(),
            confidence,
            abstained: abstain_threshold.is_some_and(|t| calibrate::should_abstain(confidence, t)),
        };
    }

    // Stage 3: rerank via the worker subprocess. The supervisor was
    // started at daemon boot with rerank eagerly Ready, so the run
    // can't be the cold-compile path here.
    let t = Instant::now();
    let supv = crate::aiplane::supervisor::current().expect("aiplane supervisor must be running");
    let inputs: Vec<WorkloadInput> = raw_hits
        .iter()
        .map(|h| WorkloadInput::TextPair {
            a: query.clone(),
            b: h.payload.chunk_text.clone(),
        })
        .collect();
    let rerank_scores: Vec<f32> = match supv.run_batch(WorkloadKind::Rerank, inputs) {
        Ok(outputs) => match outputs
            .into_iter()
            .map(|o| match o {
                WorkloadOutput::Score { score } => Ok(score),
                other => Err(format!("rerank: unexpected output {other:?}")),
            })
            .collect::<std::result::Result<Vec<_>, _>>()
        {
            Ok(v) => v,
            Err(msg) => return ipc::Resp::Error { msg },
        },
        Err(e) => {
            return ipc::Resp::Error {
                msg: format!("rerank worker: {e:#}"),
            };
        }
    };
    let ms_rerank = t.elapsed().as_secs_f64() * 1000.0;

    // Zip + sort by rerank score desc. `SearchHit` isn't `Clone`, so we
    // move it through a (score, hit) tuple.
    let mut scored: Vec<(f32, qdrant::SearchHit)> =
        rerank_scores.into_iter().zip(raw_hits).collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    // Stage 4: calibrate (REQ-6). The rerank scores are raw bge logits,
    // sorted desc above, so the top-1/top-2 spread feeds the margin term.
    // If the caller set an `abstain_threshold` and the calibrated
    // confidence is below it, return an empty, abstained response rather
    // than quoting background noise.
    let sorted_scores: Vec<f32> = scored.iter().map(|(s, _)| *s).collect();
    let confidence = calibrate::confidence(&sorted_scores);
    if let Some(threshold) = abstain_threshold {
        if calibrate::should_abstain(confidence, threshold) {
            return ipc::Resp::Search {
                hits: Vec::new(),
                confidence,
                abstained: true,
            };
        }
    }

    let take = limit.min(scored.len());
    let hits: Vec<ipc::HitRow> = scored
        .into_iter()
        .take(take)
        .map(|(rerank_score, h)| ipc::HitRow {
            score: rerank_score,
            chunk_id: crate::knowledge::chunk::point_id(
                &h.payload.file_path,
                h.payload.chunk_index,
            ),
            file_path: h.payload.file_path,
            chunk_index: h.payload.chunk_index,
            chunk_text: h.payload.chunk_text,
            embed_score: Some(h.score),
        })
        .collect();

    let ms_total = t_total.elapsed().as_secs_f64() * 1000.0;
    log_search_rerank_complete(n_candidates, take, ms_embed, ms_qdrant, ms_rerank, ms_total);

    ipc::Resp::Search {
        hits,
        confidence,
        abstained: false,
    }
}

/// Emit the SPEC §4.6 "workload completed" structured event for a
/// finished search-rerank call. Extracted from
/// [`handle_search_rerank`] so the arch-observability Step 2 test
/// (`info_workload_completed_carries_latency`) can exercise the
/// emission shape without standing up a full qdrant + NPU stack.
fn log_search_rerank_complete(
    candidates: usize,
    limit: usize,
    embed_ms: f64,
    qdrant_ms: f64,
    rerank_ms: f64,
    latency_ms: f64,
) {
    tracing::info!(
        target: "sy::knowledge::daemon",
        workload = "search-rerank",
        candidates,
        limit,
        embed_ms,
        qdrant_ms,
        rerank_ms,
        latency_ms,
        "workload completed"
    );
}

mod parking_lot_like_mutex {
    use std::sync::{Mutex as StdMutex, MutexGuard};
    pub struct Mutex<T>(StdMutex<T>);
    impl<T> Mutex<T> {
        pub fn new(v: T) -> Self {
            Self(StdMutex::new(v))
        }
        pub fn lock(&self) -> MutexGuard<'_, T> {
            self.0.lock().expect("poisoned")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// knowledge-retrieval-iter1 Step 3: a live collection that predates the
    /// v2 named-vector schema (old flat unnamed `{size,distance}` vectors) must
    /// trigger a `FullResync` on daemon startup; a v2 collection or an absent
    /// collection must not.
    #[test]
    fn stale_schema_triggers_full_resync() {
        let pre_v2 = serde_json::json!({
            "result": { "config": { "params": {
                "vectors": { "size": 768, "distance": "Cosine" }
            }}}
        });
        assert!(schema_migration_needed(Some(&pre_v2)));

        let v2 = serde_json::json!({
            "result": { "config": { "params": {
                "vectors": { "dense": { "size": 768, "distance": "Cosine" } }
            }}}
        });
        assert!(!schema_migration_needed(Some(&v2)));

        // Absent collection: created fresh at v2, no migration.
        assert!(!schema_migration_needed(None));
    }

    /// knowledge-retrieval-iter1 cross-cutting DoD: the daemon must warn
    /// loudly (but not crash) when the live qdrant predates 1.16 — below
    /// that, configurable RRF `k` is silently ignored and hybrid search
    /// regresses. An adequate or unknown version yields no warning.
    #[test]
    fn old_qdrant_version_warns() {
        // ≥1.16: silent.
        assert!(qdrant_version_warning(Some((1, 16))).is_none());
        assert!(qdrant_version_warning(Some((1, 18))).is_none());
        // <1.16: a loud, actionable warning naming the live version.
        let w = qdrant_version_warning(Some((1, 12))).expect("warning for old qdrant");
        assert!(w.contains("1.12"));
        assert!(w.contains("1.16"));
        assert!(w.contains("sy apply"));
        // Unknown (unreachable / unparseable): no warning — `ensure_collection`
        // already failed loudly upstream if qdrant were truly down.
        assert!(qdrant_version_warning(None).is_none());
    }

    /// arch-observability Step 2: the knowledge daemon's per-workload
    /// completion log must be a structured `tracing::info!` carrying
    /// `latency_ms` as a typed field (per SPEC §4.6 "Metrics"), not an
    /// `eprintln!` line with the ms baked into the message. Verifies
    /// the conversion from
    ///   `eprintln!("…total_ms={:.0}", ms_total)`
    /// to
    ///   `tracing::info!(latency_ms = ms_total, "workload completed")`.
    #[test]
    #[tracing_test::traced_test]
    fn info_workload_completed_carries_latency() {
        const CANDIDATES: usize = 32;
        const LIMIT: usize = 8;
        const EMBED_MS: f64 = 4.0;
        const QDRANT_MS: f64 = 2.5;
        const RERANK_MS: f64 = 17.0;
        const LATENCY_MS: f64 = 24.0;

        log_search_rerank_complete(
            CANDIDATES, LIMIT, EMBED_MS, QDRANT_MS, RERANK_MS, LATENCY_MS,
        );

        assert!(
            logs_contain("latency_ms=24"),
            "expected `latency_ms` structured field in captured tracing logs"
        );
        assert!(
            logs_contain("workload=\"search-rerank\""),
            "expected `workload` structured field tagging the call site"
        );
        assert!(
            logs_contain("sy::knowledge::daemon"),
            "expected the knowledge::daemon tracing target"
        );
        assert!(
            logs_contain("workload completed"),
            "expected the static \"workload completed\" message body"
        );
    }
}
