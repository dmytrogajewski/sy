//! `sy mon collect` aggregator runtime.
//!
//! Step 11 ships:
//! - the tokio multi-thread runtime that hosts the tick task,
//! - the 1 Hz host-sensor tick (cpu / mem / load) that writes one
//!   `f32` row into the mmap ring buffer from Step 7,
//! - the `sd_notify(READY=1)` handshake so `Type=notify` units enter
//!   `active (running)` once the first tick lands.
//!
//! Out of scope for Step 11 (lands in later sy-mon roadmap steps):
//! plane-UDS scrape (Step 12), IPC server (Step 13), `SystemSnapshot`
//! projection (Step 12), the popup (Steps 15-17), doctor checks
//! (Step 21).

pub mod ipc;
pub mod sample;
pub mod scrape;
pub mod snapshot;
pub mod tick;

use std::path::PathBuf;
use std::sync::Arc;

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use sy_core::mon::ring::Ring;
use sy_core::mon::snapshot::LatestSnapshot;
use tokio::sync::Mutex;

use super::cli::{default_bind_path, default_history_path, CollectOpts};

use ipc::MonHandler;

/// Ring buffer column count. Step 11 only writes cpu/mem/swap/load (4
/// columns); Step 12 expands the projection to the full
/// `SystemSnapshot` inventory. We pin the on-disk shape at 16 columns
/// today so Step 12 can grow into the reserved slots without forcing a
/// ring rebuild on every dev host.
pub const RING_METRICS: u32 = 16;

/// Entry point for `sy mon collect`. Caller wraps this in a tokio
/// runtime (see `mon::cli::dispatch`). All state lives inside this
/// function — the aggregator owns the ring, the tick interval, and
/// the error accumulator for the current tick.
pub async fn run(opts: CollectOpts) -> Result<()> {
    let _obs_guard = sy_core::obs::init(sy_core::obs::Mode::Daemon {
        name: "sy-mon-collect",
    })
    .context("install sy-mon-collect tracing subscriber")?;

    let history_path = match opts.history_path.clone() {
        Some(p) => p,
        None => default_history_path()?,
    };
    let bind_path = match opts.bind.clone() {
        Some(p) => p,
        None => default_bind_path()?,
    };
    ensure_parent_dir(&history_path)?;

    tracing::info!(
        target: "sy::mon::collect",
        history_path = %history_path.display(),
        bind_path = %bind_path.display(),
        history_size = opts.history_size,
        tick_ms = opts.tick_ms,
        "starting sy-mon-collect aggregator"
    );

    let ring = Ring::open_or_rebuild(&history_path, opts.history_size, RING_METRICS)
        .with_context(|| format!("open ring buffer at {}", history_path.display()))?;
    let ring = Arc::new(Mutex::new(ring));
    let latest = LatestSnapshot::new();
    let tick_tx = ipc::tick_channel();

    let plane_paths = resolve_known_plane_paths();

    // sy-mon Step 20: install the aggregator's own Prometheus UDS at
    // `$XDG_RUNTIME_DIR/sy/supervisor/metrics.sock`. The supervisor
    // "plane" doesn't have a dedicated daemon process — `sy mon
    // collect` is the only long-lived process with cross-plane
    // visibility, so it hosts the supervisor exposition surface
    // alongside its IPC server. The exporter renders the global
    // recorder, and Step 20's `supervision::emit_plane_state` writes
    // `sy_supervisor_plane_state{plane,state}` into that recorder on
    // every tick below. Bind failure is non-fatal — the aggregator
    // keeps running so the snapshot stream stays alive even when the
    // metrics surface is unavailable.
    #[cfg(feature = "mon-exporter")]
    let _supervisor_mon_exporter = match install_supervisor_mon_exporter().await {
        Ok(g) => Some(g),
        Err(e) => {
            tracing::warn!(
                target: "sy::mon::collect",
                error = %format!("{e:#}"),
                "supervisor mon-exporter failed to bind; continuing without metrics socket"
            );
            None
        }
    };

    // Land one tick before notifying readiness so the first IPC client
    // (Step 13) sees a populated ring instead of an empty one.
    let initial_errors = run_one_tick(&ring, &latest, &plane_paths, &tick_tx).await;
    log_tick_errors(&initial_errors);

    // Bind the sy-ipc UDS and spawn the accept loop in its own tokio
    // task. Step 13: serve `system.mon.{snapshot,subscribe,history}`
    // plus the reserved `system.{describe,health,cancel}` surface
    // composed via `SystemMethods`. The aggregator is a single-process
    // owner of the socket; a stale file from a previous crash gets
    // unlinked inside `ipc::bind_uds`.
    let cancel_registry = Arc::new(sy_ipc::CancelRegistry::new());
    let system = Arc::new(ipc::system_methods(
        aggregator_build_info(),
        cancel_registry,
    ));
    let handler = MonHandler::new(latest.clone(), ring.clone(), tick_tx.clone(), system);
    let listener = ipc::bind_uds(&bind_path)
        .with_context(|| format!("bind sy-ipc UDS at {}", bind_path.display()))?;
    tokio::spawn(async move {
        if let Err(e) = ipc::serve(handler, listener).await {
            tracing::error!(
                target: "sy::mon::collect",
                error = %e,
                "sy-mon-collect IPC server exited unexpectedly",
            );
        }
    });

    // `Type=notify` handshake. No-op on dev hosts without NOTIFY_SOCKET
    // (see `sy_core::notify::ready`); essential under systemd so the
    // unit flips from `activating` to `active (running)`.
    sy_core::notify::ready();
    let _watchdog = sy_core::notify::spawn_watchdog();

    let mut interval = tokio::time::interval(Duration::from_millis(opts.tick_ms as u64));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume the immediate first tick (tokio's interval fires
    // instantly on first `tick().await`); we already sampled above.
    interval.tick().await;

    loop {
        interval.tick().await;
        let errors = run_one_tick(&ring, &latest, &plane_paths, &tick_tx).await;
        log_tick_errors(&errors);
    }
}

/// Drive one tick: lock the ring just for `tick::run_once`, publish the
/// resulting snapshot into [`LatestSnapshot`] so the IPC handlers see
/// it, and signal `subscribe` consumers via the broadcast channel.
/// The mutex scope is bounded to the tick itself — the ring lock is
/// dropped before the snapshot publish + broadcast send so IPC
/// `history` calls aren't blocked on the next tick's I/O.
async fn run_one_tick(
    ring: &Arc<Mutex<Ring>>,
    latest: &LatestSnapshot,
    plane_paths: &[(String, PathBuf)],
    tick_tx: &tokio::sync::broadcast::Sender<()>,
) -> Vec<sy_core::mon::snapshot::MonError> {
    let (snap, errors) = {
        let mut guard = ring.lock().await;
        tick::run_once(
            &mut guard,
            RING_METRICS as usize,
            sample::sample_host,
            plane_paths,
        )
        .await
    };
    // sy-mon Step 20: emit `sy_supervisor_plane_state{plane,state}`
    // from the tick's per-plane scrape outcomes. A scrape success
    // implies the plane is reachable → state "ready"; a scrape
    // failure under a known socket path → state "failed". This is
    // the in-tree "live indicator" pattern; richer state (systemd
    // ActiveState parity) is wired by Step 21's doctor surface.
    emit_supervisor_plane_states(plane_paths, &snap.errors);
    latest.store(snap);
    // `send` returns `Err` when no subscribers are attached — that's
    // the steady state when no `subscribe` client is connected, so it
    // is not an error condition.
    let _ = tick_tx.send(());
    errors
}

/// Derive a `(plane, state_token)` table from the known plane paths
/// and the current tick's error set, then hand it off to
/// `supervision::emit_plane_state` so the gauge reflects whichever
/// planes responded. Any plane whose path appears in `errors[]`
/// under `kind="scrape_failed"` gets the `failed` indicator; the
/// rest get `ready`.
fn emit_supervisor_plane_states(
    plane_paths: &[(String, PathBuf)],
    errors: &[sy_core::mon::snapshot::MonError],
) {
    let failed: std::collections::HashSet<&str> = errors
        .iter()
        .filter(|e| e.kind == "scrape_failed")
        .map(|e| e.plane.as_str())
        .collect();
    let records: Vec<(&str, &str)> = plane_paths
        .iter()
        .map(|(plane, _)| {
            let state = if failed.contains(plane.as_str()) {
                "failed"
            } else {
                "ready"
            };
            (plane.as_str(), state)
        })
        .collect();
    crate::supervision::emit_plane_state(&records);
}

/// sy-mon Step 20: install the aggregator's own Prometheus UDS for the
/// supervisor "plane". The mon-collect process is `sy mon`'s only
/// cross-plane vantage point — emitting `sy_supervisor_plane_state`
/// from anywhere else would either need a brand-new daemon (overkill)
/// or per-daemon broadcast (race-prone). Holding the bind here keeps
/// the supervisor panel's data path a single hop. Must be called from
/// the mon-collect tokio runtime — the shared installer's accept task
/// spawns onto it.
#[cfg(feature = "mon-exporter")]
async fn install_supervisor_mon_exporter() -> anyhow::Result<sy_core::obs::mon_exporter::UdsGuard> {
    let path = crate::mon_exporter::socket_path_for("supervisor")?;
    let guard = sy_core::obs::mon_exporter::install(path.clone()).map_err(|e| {
        anyhow::anyhow!("install supervisor mon-exporter at {}: {e}", path.display())
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(guard.path()) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(guard.path(), perms);
        }
    }
    tracing::info!(
        target: "sy::mon::collect",
        path = %guard.path().display(),
        "supervisor mon-exporter bound"
    );
    Ok(guard)
}

/// `BuildInfo` advertised over `system.describe` from the aggregator.
/// `version` tracks the workspace crate version; `git_sha` is left as
/// the env var the release pipeline injects, falling back to a fixed
/// dev tag so the wire shape is stable even in dev builds.
fn aggregator_build_info() -> sy_ipc::BuildInfo {
    sy_ipc::BuildInfo {
        name: "sy-mon-collect".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        git_sha: option_env!("SY_BUILD_GIT_SHA").unwrap_or("unknown").into(),
    }
}

/// Materialise the production plane-path slice from `tick::KNOWN_PLANES`.
/// Planes whose `XDG_RUNTIME_DIR` lookup fails (only possible when the
/// env var is unset/empty) are skipped — the scraper has nothing to
/// connect to on a non-session host.
fn resolve_known_plane_paths() -> Vec<(String, PathBuf)> {
    tick::KNOWN_PLANES
        .iter()
        .filter_map(|plane| tick::plane_socket_path(plane).map(|p| (plane.to_string(), p)))
        .collect()
}

/// Best-effort `mkdir -p` for the ring buffer's parent directory.
/// `$XDG_RUNTIME_DIR/sy/mon/` is typically absent on a freshly-booted
/// session; without this the first `Ring::open_or_rebuild` would fail
/// with ENOENT.
fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir -p {}", parent.display()))?;
        }
    }
    Ok(())
}

/// Emit one `tracing::warn!` line per tick error so journald carries
/// the per-source failure surface even before the IPC handlers ship.
fn log_tick_errors(errors: &[sy_core::mon::snapshot::MonError]) {
    for err in errors {
        tracing::warn!(
            target: "sy::mon::collect",
            plane = %err.plane,
            kind = %err.kind,
            message = %err.message,
            "tick error"
        );
    }
}
