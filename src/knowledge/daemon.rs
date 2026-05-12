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
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use notify::RecursiveMode;
use notify_debouncer_mini::new_debouncer;

use super::{cli, embed, ipc, manifest, qdrant, runctx::RunCtx, sources, state, status, QDRANT_PORT};
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
    set_process_priority();
    let mut child = spawn_qdrant().context("spawn qdrant")?;

    if let Err(e) = qdrant::wait_ready(20) {
        let _ = child.kill();
        return Err(e);
    }
    qdrant::ensure_collection()?;

    let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonOp>();

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
    let (req_tx, req_rx) = mpsc::channel::<(ipc::Req, std::os::unix::net::UnixStream)>();
    ipc::serve(ipc_tx, req_tx).context("ipc serve")?;
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
                    eprintln!("sy knowledge daemon: paused (cancelling in-flight pass)");
                }
                ipc::Op::Resume => {
                    if bridge_paused.swap(false, Ordering::SeqCst) {
                        eprintln!("sy knowledge daemon: resumed");
                        let _ = bridge_tx.send(DaemonOp::Ipc(ipc::Op::IndexNow));
                    }
                }
                ipc::Op::TogglePause => {
                    let now_paused = !bridge_paused.load(Ordering::SeqCst);
                    bridge_paused.store(now_paused, Ordering::SeqCst);
                    if now_paused {
                        bridge_cancel.store(true, Ordering::SeqCst);
                        eprintln!("sy knowledge daemon: paused (cancelling in-flight pass)");
                    } else {
                        eprintln!("sy knowledge daemon: resumed");
                        let _ = bridge_tx.send(DaemonOp::Ipc(ipc::Op::IndexNow));
                    }
                }
                ipc::Op::Cancel => {
                    bridge_cancel.store(true, Ordering::SeqCst);
                    eprintln!("sy knowledge daemon: cancel requested");
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
    thread::spawn(move || loop {
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

    let _ = run_one_pass(
        false,
        false,
        &mut last_pass,
        interval,
        last_run,
        &active_manifests,
        &paused,
        &cancel,
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    install_signal_handlers(shutdown.clone());

    eprintln!(
        "sy knowledge daemon: ready (qdrant on {}, schedule {}s, manifests {}, throttle {}ms, cap {})",
        qdrant::base_url(),
        interval.as_secs(),
        active_manifests.iter().filter(|m| m.enabled).count(),
        sources::cpu_throttle().as_millis(),
        sources::cpu_max_percent().map(|p| format!("{p}%")).unwrap_or_else(|| "off".into()),
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
        // Pause / Resume / TogglePause / Cancel are handled directly in
        // the IPC bridge thread (so they take effect *during* a pass).
        // The main loop just observes the resulting atomic flags below.
        let _ = (want_pause, want_resume, want_toggle_pause, want_cancel);

        if want_schedule_reload {
            interval = parse_schedule_or_default();
            eprintln!("sy knowledge daemon: schedule = {}s", interval.as_secs());
        }
        if want_rescan {
            // Re-walk discovery roots; figure out which folders went away
            // (or got disabled) and drop their points from qdrant.
            let new_manifests = manifest::discover_all();
            let new_folders = enabled_folders(&new_manifests);
            let added: Vec<PathBuf> =
                new_folders.difference(&active_folders).cloned().collect();
            let retired: Vec<PathBuf> =
                active_folders.difference(&new_folders).cloned().collect();
            for r in &retired {
                let label = r.display().to_string();
                if let Err(e) = qdrant::delete_by_source(&label) {
                    eprintln!(
                        "sy knowledge daemon: delete_by_source({label}) failed: {e}"
                    );
                } else {
                    eprintln!("sy knowledge daemon: retired manifest {label}");
                }
                purge_index_subtree(r);
            }
            for a in &added {
                eprintln!("sy knowledge daemon: discovered manifest {}", a.display());
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
                Err(e) => eprintln!("sy knowledge daemon: rebuild watchers failed: {e}"),
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

    eprintln!("sy knowledge daemon: shutting down");
    write_shutdown_status(&last_pass, interval, last_run, &active_manifests);
    shutdown_qdrant(&mut child);
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
        paused: false,         // overwritten by callers that know the flag
        cancelling: false,     // overwritten by callers
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
    }
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
                eprintln!(
                    "sy knowledge daemon: scanned {} indexed {} skipped {} deleted {} ({}ms){}{}",
                    report.scanned,
                    report.indexed,
                    report.skipped,
                    report.deleted,
                    report.elapsed_ms,
                    if throttle { " [throttled]" } else { "" },
                    if cancelled { " [cancelled]" } else { "" }
                );
            }
        }
        Err(e) => {
            last.error = Some(format!("{e}"));
            last.at_unix = state::now_secs();
            eprintln!("sy knowledge daemon: index pass failed: {e}");
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
        eprintln!("sy knowledge daemon: full resync failed: {e}");
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
            eprintln!(
                "sy knowledge daemon: full resync done — indexed {} chunks ({}ms)",
                report.chunks, report.elapsed_ms
            );
        }
        Err(e) => {
            last.error = Some(format!("{e}"));
            last.at_unix = state::now_secs();
            eprintln!("sy knowledge daemon: full resync failed: {e}");
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
    eprintln!("sy knowledge daemon: nice = {nice}, ionice = idle");
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
    let mut debouncer = new_debouncer(Duration::from_secs(1), move |res: notify_debouncer_mini::DebounceEventResult| {
        let events = match res {
            Ok(e) => e,
            Err(_) => return,
        };
        let mut saw_qdr = false;
        let mut saw_other = false;
        let mut saw_home_topology = false;
        for ev in &events {
            if ev.path.file_name().and_then(|n| n.to_str()) == Some(manifest::MANIFEST_FILENAME) {
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
    })
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
                eprintln!(
                    "sy knowledge daemon: watch (discover) {} failed: {e}",
                    r.display()
                );
            }
        }
    }

    for r in sources::enabled_paths().unwrap_or_default() {
        if r.exists() {
            if let Err(e) = watcher.watch(&r, RecursiveMode::Recursive) {
                eprintln!(
                    "sy knowledge daemon: watch (explicit) {} failed: {e}",
                    r.display()
                );
            }
        }
    }

    for m in manifests.iter().filter(|m| m.enabled) {
        if m.folder.exists() {
            if let Err(e) = watcher.watch(&m.folder, RecursiveMode::Recursive) {
                eprintln!(
                    "sy knowledge daemon: watch (manifest) {} failed: {e}",
                    m.folder.display()
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
        .env(
            "QDRANT__STORAGE__SNAPSHOTS_PATH",
            storage.join("snapshots"),
        )
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
    thread::spawn(move || loop {
        if SIGNAL_RECEIVED.load(Ordering::SeqCst) {
            flag.store(true, Ordering::SeqCst);
            return;
        }
        thread::sleep(Duration::from_millis(100));
    });
}

static SIGNAL_RECEIVED: AtomicBool = AtomicBool::new(false);

/// Dedicated worker for request-response IPC. Owned channel + a
/// thread-pool-of-one suffices: NPU only handles one inference at a
/// time anyway (the underlying `Embedder` is a `Mutex<...>` in
/// embed.rs), and we don't want a flood of search requests to head-of-line
/// block the daemon's own indexing pass.
fn spawn_req_worker(
    req_rx: mpsc::Receiver<(ipc::Req, std::os::unix::net::UnixStream)>,
) {
    thread::spawn(move || {
        use std::io::Write;
        while let Ok((req, mut stream)) = req_rx.recv() {
            let resp = handle_req(req);
            let line = match serde_json::to_string(&resp) {
                Ok(s) => s,
                Err(e) => {
                    // We failed to serialise even our own response; nothing
                    // safe to send back. Drop the connection.
                    eprintln!("sy knowledge daemon: req serialise: {e}");
                    continue;
                }
            };
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            let _ = writeln!(stream, "{line}");
        }
    });
}

fn handle_req(req: ipc::Req) -> ipc::Resp {
    match req {
        ipc::Req::Embed { text } => match embed::embed_one(&text) {
            Ok(vec) => ipc::Resp::Embed { vector: vec },
            Err(e) => ipc::Resp::Error {
                msg: format!("embed: {e}"),
            },
        },
        ipc::Req::Search {
            query,
            limit,
            prefix,
        } => {
            let vec = match embed::embed_one(&query) {
                Ok(v) => v,
                Err(e) => {
                    return ipc::Resp::Error {
                        msg: format!("embed: {e}"),
                    };
                }
            };
            match qdrant::search(&vec, limit, prefix.as_deref()) {
                Ok(hits) => ipc::Resp::Search {
                    hits: hits
                        .into_iter()
                        .map(|h| ipc::HitRow {
                            score: h.score,
                            file_path: h.payload.file_path,
                            chunk_index: h.payload.chunk_index,
                            chunk_text: h.payload.chunk_text,
                        })
                        .collect(),
                },
                Err(e) => ipc::Resp::Error {
                    msg: format!("qdrant search: {e}"),
                },
            }
        }
    }
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
