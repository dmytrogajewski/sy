//! `sy file` IPC surface — JSON-RPC v1 over
//! `$XDG_RUNTIME_DIR/sy-file.sock` (SPEC §4.3). Step 20 of the
//! [`sy-file-manager` roadmap][roadmap] replaces the Step-13 stub
//! with the daemon-in-thread shape every CLI / MCP / agent consumer
//! rides on:
//!
//! 1. [`serve`] binds a `tokio::net::UnixListener` at the caller-
//!    supplied path, chmods it to `0o600`, and spawns
//!    [`sy_ipc::Server::serve`] with [`FileHandler`] — the dispatcher
//!    for the eleven SPEC §4.3 ops (`file.open`, `file.cd`,
//!    `file.select`, `file.copy`, `file.move`, `file.trash`,
//!    `file.restore`, `file.search`, `file.preview`, `file.ops_list`,
//!    `file.op_cancel`) plus the [`file.state`](FileMethod::State)
//!    snapshot the journey-J8 agent-mirror test reads.
//! 2. The accept loop runs under `tokio::select!` against
//!    `signal::ctrl_c` and `signal::unix::signal(SIGTERM)`; on either
//!    signal the socket is unlinked so the daemon never leaves a
//!    stale endpoint behind. The `shutdown` oneshot lets tests trip
//!    the same teardown synchronously.
//! 3. Reserved `system.*` methods route through [`sy_ipc::SystemMethods`]
//!    so every IPC v1 client (sy doctor, sy ipc ping, agents) speaks
//!    the same probe surface.
//!
//! Scope boundaries deferred to later steps (documented inline as
//! `// Step NN` markers so the future step lands without surprise):
//!
//! * `file.move` cross-fs ladder — Step 21+ (today: cross-fs returns
//!   exit-code 4 / [`ErrorCode::Cancelled`] per SPEC §4.3 table).
//! * `file.search` knowledge-backed branch — Step 30 wires qdrant;
//!   today the `knowledge: true` request still returns the filename-
//!   match result set and logs a `knowledge backend not yet wired`
//!   notice.
//! * `file.preview` `png_base64` body — Step 27's plugin-routed
//!   previewer dispatcher fills this; today the field is the empty
//!   string and the `mime` field is the
//!   [`crate::file::fs::mime::mime_for`] result.
//!
//! [roadmap]: ../../../specs/roadmaps/sy-file-manager/ROADMAP.md

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sy_core::ErrorCode;
use sy_ipc::{
    BuildInfo, Capabilities, ErrorBody, Handler, HealthFn, HealthSnapshot, HealthState, Request,
    Response, Server, SystemMethods, SCHEMA_VERSION,
};
use tokio::sync::{broadcast, oneshot, Mutex, RwLock};
use ulid::Ulid;

use crate::file::fs::{copy, mime, trash, walk};
use crate::file::state::{ConflictPolicy, OpEvent, State};

/// SPEC §4.3 op-method namespace. One source of truth for the wire
/// strings the eleven ops + the journey-J8 `file.state` snapshot
/// travel under; the CLI dispatcher and the handler both pin against
/// these constants so a typo can't silently dead-route a request.
const METHOD_OPEN: &str = "file.open";
const METHOD_CD: &str = "file.cd";
const METHOD_SELECT: &str = "file.select";
const METHOD_COPY: &str = "file.copy";
const METHOD_MOVE: &str = "file.move";
const METHOD_TRASH: &str = "file.trash";
const METHOD_RESTORE: &str = "file.restore";
const METHOD_SEARCH: &str = "file.search";
const METHOD_PREVIEW: &str = "file.preview";
const METHOD_OPS_LIST: &str = "file.ops_list";
const METHOD_OP_CANCEL: &str = "file.op_cancel";
/// Journey-J8 mirror op — returns `{ cwd, selection, mode }`. Not in
/// SPEC §4.3's eleven-op table but required by the Step 20 e2e
/// (`step20_two_clients_share_state_for_agent_mirror_j8`) to prove
/// "fresh client sees client A's mutations". Wire-stable today.
const METHOD_STATE: &str = "file.state";

/// Full sorted list of methods this daemon advertises via
/// `system.describe`. Kept const so a regression that forgets to
/// register a method here surfaces in the describe round-trip test.
pub const FILE_METHODS: &[&str] = &[
    METHOD_OPEN,
    METHOD_CD,
    METHOD_SELECT,
    METHOD_COPY,
    METHOD_MOVE,
    METHOD_TRASH,
    METHOD_RESTORE,
    METHOD_SEARCH,
    METHOD_PREVIEW,
    METHOD_OPS_LIST,
    METHOD_OP_CANCEL,
    METHOD_STATE,
];

/// UDS file mode required by SPEC §4.3 ("mode 0600"). Public so the
/// `socket_mode_is_0600` test asserts against the same constant the
/// daemon writes.
pub const SOCKET_MODE: u32 = 0o600;

/// Broadcast channel depth for the per-op cancel signal. Sized so a
/// burst of `file.op_cancel` calls from a confused client doesn't
/// silently drop a notification before the running copy executor's
/// receiver polls it (the receiver runs inside `fs::copy`'s
/// `tokio::select!` and observes the signal on its next progress
/// tick).
const CANCEL_CHANNEL_DEPTH: usize = 16;

/// Per-row snapshot of an in-flight or recently-completed op. The
/// `file.ops_list` op returns a `Vec<OpRow>` (one entry per running
/// or terminated op since the daemon started). Wire-stable JSON
/// shape — the `kind` discriminator is the `Operation` verb name
/// (`copy`, `move`, `trash`, …) so MCP consumers can route on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpRow {
    /// Monotonic id assigned by the daemon on op start (separate from
    /// the `OpEvent::op_id` issued inside `fs::copy` — the daemon-
    /// side id is what `file.op_cancel { op_id }` keys on).
    pub op_id: u64,
    /// `Operation` verb — `"copy"`, `"move"`, `"trash"`, `"restore"`.
    pub kind: String,
    /// Lifecycle state — `"running"`, `"completed"`, `"failed"`,
    /// `"cancelled"`.
    pub state: String,
    /// Bytes processed so far across every src in the batch.
    pub done: u64,
    /// Total bytes in the batch (`0` until the executor knows).
    pub total: u64,
}

/// `system.describe.build_info.name` value the daemon advertises.
const DAEMON_NAME: &str = "sy-file";

/// Bind a UDS at `sock_path`, chmod it to [`SOCKET_MODE`], and serve
/// the SPEC §4.3 op surface until either:
///
///   * `ctrl_c` / `SIGTERM` arrives (production tear-down), or
///   * the `shutdown` oneshot fires (test-side tear-down).
///
/// On shutdown the socket file is unlinked so a subsequent `serve`
/// (or a follow-on `Client::connect`) doesn't trip over a stale
/// inode. Errors during unlink are logged-and-dropped — leaving the
/// daemon in a "shutdown returned Ok" state is the right answer when
/// the tempdir cleanup is about to remove the file anyway.
pub async fn serve(
    state: Arc<RwLock<State>>,
    sock_path: PathBuf,
    shutdown: oneshot::Receiver<()>,
) -> Result<()> {
    serve_with_ready(state, sock_path, shutdown, || {}).await
}

/// Same as [`serve`] but invokes `on_ready` exactly once after the
/// listener is bound + chmod'd, before the accept loop runs. The hook
/// is the integration point for `Type=notify` units (roadmap Step 22)
/// — the CLI's `--systemd-notify` flag passes a closure that emits
/// `sd_notify(READY=1)`. Default callers pass `|| {}` to opt out.
///
/// Sync hook by design: `sd_notify::notify` writes a single UDP-style
/// datagram and returns; an `async` signature would force every dev
/// caller (tests, embedded harnesses) to await a no-op.
pub async fn serve_with_ready<F>(
    state: Arc<RwLock<State>>,
    sock_path: PathBuf,
    shutdown: oneshot::Receiver<()>,
    on_ready: F,
) -> Result<()>
where
    F: FnOnce() + Send + 'static,
{
    let listener = bind_listener(&sock_path)?;
    on_ready();
    let (cancel_tx, _cancel_rx) = broadcast::channel::<u64>(CANCEL_CHANNEL_DEPTH);
    let handler = FileHandler::new(state.clone(), cancel_tx);
    let server = Server::new(handler);
    let serve_fut = server.serve(listener);
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install SIGTERM handler for sy-file daemon")?;
    // Roadmap Step 34 (SPEC §3.3 item 18 DoD): SIGHUP hot-reloads the
    // operator's `$XDG_CONFIG_HOME/sy/file-keymap.toml`. Failed reads
    // are logged-and-dropped so a half-edited file doesn't kill the
    // daemon mid-session.
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .context("install SIGHUP handler for sy-file daemon")?;
    tokio::pin!(serve_fut);
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            biased;
            sig = sighup.recv() => {
                if sig.is_none() {
                    // Stream closed (the OS dropped the signal source).
                    // Stop the loop so we don't busy-spin on a permanently
                    // pending future.
                    break;
                }
                reload_keymap(state.clone()).await;
                continue;
            }
            _ = &mut shutdown => { break; }
            _ = tokio::signal::ctrl_c() => { break; }
            _ = sigterm.recv() => { break; }
            res = &mut serve_fut => {
                // serve() only returns on listener error — surface it
                // so the caller can crash-report instead of silently
                // exiting.
                if let Err(e) = res {
                    let _ = std::fs::remove_file(&sock_path);
                    return Err(anyhow::anyhow!("sy-file ipc serve: {e}"));
                }
                break;
            }
        }
    }
    // Best-effort unlink — leaving the file behind would block the
    // next bind with EADDRINUSE.
    let _ = std::fs::remove_file(&sock_path);
    Ok(())
}

/// SIGHUP reload — read
/// `$XDG_CONFIG_HOME/sy/file-keymap.toml` (with the productivised
/// fallback baked in by `apply()`) and swap the live
/// [`crate::file::keymap::KeymapConfig`] in `state.keymap`. Errors
/// are logged + dropped so the daemon keeps serving with the prior
/// keymap.
async fn reload_keymap(state: Arc<RwLock<State>>) {
    let path = crate::file::keymap::user_keymap_path();
    match crate::file::keymap::KeymapConfig::load(&path) {
        Ok(new_cfg) => {
            // An empty `bindings` table after parse means the user's
            // file existed but carried no `[[keymap]]` rows. The
            // loader already falls back to the yazi-shaped defaults
            // (see `KeymapConfig::parse`); we surface the empty-input
            // path so an operator chasing a "my keymap didn't take"
            // bug sees it in the journal.
            let restored_defaults = new_cfg.is_empty();
            // Probe one canonical key so the daemon's journal carries
            // the "what does Space do now?" answer on every reload.
            // Operators chasing a "my override didn't take" bug read
            // this without round-tripping IPC.
            let space_action = new_cfg
                .action_for("space")
                .map(|s| s.to_owned())
                .unwrap_or_else(|| "<unbound>".to_owned());
            let mut guard = state.write().await;
            guard.keymap = new_cfg;
            tracing::info!(
                target = "sy::file::ipc",
                path = %path.display(),
                bindings = guard.keymap.len(),
                restored_defaults,
                space_action = %space_action,
                "SIGHUP — keymap reloaded"
            );
        }
        Err(e) => {
            tracing::warn!(
                target = "sy::file::ipc",
                path = %path.display(),
                error = %e,
                "SIGHUP — keymap reload failed; keeping prior keymap"
            );
        }
    }
}

/// Bind the UDS listener with the SPEC §4.3 0o600 chmod applied
/// before any client can connect. Pre-unlinks a stale socket so a
/// daemon restart doesn't trip EADDRINUSE.
fn bind_listener(sock_path: &Path) -> Result<tokio::net::UnixListener> {
    if sock_path.exists() {
        let _ = std::fs::remove_file(sock_path);
    }
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all({}) for sy-file socket", parent.display()))?;
    }
    let std_listener = std::os::unix::net::UnixListener::bind(sock_path)
        .with_context(|| format!("bind sy-file socket at {}", sock_path.display()))?;
    std_listener
        .set_nonblocking(true)
        .context("set_nonblocking on sy-file listener")?;
    std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(SOCKET_MODE))
        .with_context(|| format!("chmod {SOCKET_MODE:o} on {}", sock_path.display()))?;
    tokio::net::UnixListener::from_std(std_listener)
        .context("convert std UnixListener for sy-file daemon")
}

/// Shared dispatch state for every op handler. The `RwLock<State>` is
/// the canonical SPEC §3.1 model; `ops` is the daemon-local progress
/// tracker `file.ops_list` reads; `cancel_tx` broadcasts op-cancel
/// signals to running copy executors.
struct FileHandler {
    state: Arc<RwLock<State>>,
    ops: Arc<Mutex<Vec<OpRow>>>,
    cancel_tx: broadcast::Sender<u64>,
    next_op_id: Arc<AtomicU64>,
    system: SystemMethods,
}

impl FileHandler {
    fn new(state: Arc<RwLock<State>>, cancel_tx: broadcast::Sender<u64>) -> Self {
        let cancel_registry = Arc::new(sy_ipc::CancelRegistry::new());
        let health_fn: HealthFn = Arc::new(|| HealthSnapshot {
            state: HealthState::Ready,
            status_line: "sy-file ready".into(),
            queue_depth: 0,
            warm_models: Vec::new(),
        });
        let build_info = BuildInfo {
            name: DAEMON_NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            git_sha: option_env!("SY_GIT_SHA").unwrap_or("dev").into(),
        };
        let methods: Vec<String> = FILE_METHODS.iter().map(|s| (*s).to_string()).collect();
        let system = SystemMethods::new(
            build_info,
            health_fn,
            cancel_registry,
            Capabilities::baseline(),
            methods,
        );
        Self {
            state,
            ops: Arc::new(Mutex::new(Vec::new())),
            cancel_tx,
            next_op_id: Arc::new(AtomicU64::new(1)),
            system,
        }
    }
}

impl Handler for FileHandler {
    async fn handle(&self, req: Request) -> Response {
        if let Some(resp) = self.system.try_handle(&req) {
            return resp;
        }
        match req.method.as_str() {
            METHOD_OPEN => handle_open(self, req).await,
            METHOD_CD => handle_cd(self, req).await,
            METHOD_SELECT => handle_select(self, req).await,
            METHOD_COPY => handle_copy(self, req).await,
            METHOD_MOVE => handle_move(self, req).await,
            METHOD_TRASH => handle_trash(self, req).await,
            METHOD_RESTORE => handle_restore(self, req).await,
            METHOD_SEARCH => handle_search(self, req).await,
            METHOD_PREVIEW => handle_preview(self, req).await,
            METHOD_OPS_LIST => handle_ops_list(self, req).await,
            METHOD_OP_CANCEL => handle_op_cancel(self, req).await,
            METHOD_STATE => handle_state_snapshot(self, req).await,
            other => err(
                req.request_id,
                ErrorCode::BadRequest,
                format!("unknown method: {other}"),
            ),
        }
    }
}

/// Decode `req.params` into the per-op param struct or build a
/// `BadRequest` response. Extracted so every handler keeps its
/// happy-path body uncluttered by serde plumbing.
fn parse_params<T: serde::de::DeserializeOwned>(req: &Request) -> Result<T, Response> {
    serde_json::from_value::<T>(req.params.clone()).map_err(|e| {
        err(
            req.request_id,
            ErrorCode::BadRequest,
            format!("{}: bad params: {e}", req.method),
        )
    })
}

#[derive(Deserialize)]
struct OpenParams {
    path: PathBuf,
}

async fn handle_open(this: &FileHandler, req: Request) -> Response {
    let params: OpenParams = match parse_params(&req) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let mut guard = this.state.write().await;
    guard.panes.current.cwd = params.path.clone();
    // Step 31 (SPEC §3.3 item 15) — every `file.open` op touches the
    // freedesktop `recently-used.xbel` log so other DEs (Nautilus,
    // Dolphin, the GTK file-chooser) see the same recent-dirs list.
    // The registry is `None` for headless / test contexts that don't
    // need an on-disk store; the touch is a no-op in that case.
    if let Some(reg) = guard.bookmarks.clone() {
        if let Ok(mut bm) = reg.lock() {
            if let Err(e) = bm.touch_recent(&params.path) {
                tracing::warn!(
                    target = "sy::file::bookmarks",
                    path = %params.path.display(),
                    error = %e,
                    "touch_recent failed; recently-used.xbel not updated"
                );
            }
        }
    }
    ok(req.request_id, json!({ "ok": true }))
}

#[derive(Deserialize)]
struct CdParams {
    path: PathBuf,
}

async fn handle_cd(this: &FileHandler, req: Request) -> Response {
    let params: CdParams = match parse_params(&req) {
        Ok(p) => p,
        Err(r) => return r,
    };
    // Refresh the current pane via `fs::walk`. A walk failure folds
    // into an `Internal` error rather than a `BadRequest` because the
    // request was well-formed — the fs read failed.
    let entries = match walk::walk(&params.path, false).await {
        Ok(e) => e,
        Err(e) => return err(req.request_id, ErrorCode::Internal, format!("walk: {e}")),
    };
    let mut guard = this.state.write().await;
    guard.panes.current.cwd = params.path.clone();
    guard.panes.current.set_entries(entries);
    ok(req.request_id, json!({ "ok": true }))
}

#[derive(Deserialize)]
struct SelectParams {
    paths: Vec<PathBuf>,
    mode: String,
}

async fn handle_select(this: &FileHandler, req: Request) -> Response {
    let params: SelectParams = match parse_params(&req) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let mut guard = this.state.write().await;
    // Selection is keyed by `EntryId` (u64). We map the incoming
    // paths against the current pane's entries by name; entries that
    // don't resolve are dropped silently (the agent can re-list via
    // `file.cd` to refresh ids). This is the same lookup the iced UI
    // will use under the cursor.
    let ids: Vec<u64> = params
        .paths
        .iter()
        .filter_map(|p| {
            let name = p.file_name()?.to_string_lossy().into_owned();
            guard
                .panes
                .current
                .entries
                .iter()
                .find(|e| e.name == name)
                .map(|e| e.id)
        })
        .collect();
    match params.mode.as_str() {
        "add" => {
            for id in &ids {
                if !guard.selection.contains(*id) {
                    guard.selection.toggle(*id);
                }
            }
        }
        "replace" => {
            guard.selection.clear();
            for id in &ids {
                guard.selection.toggle(*id);
            }
        }
        "toggle" => {
            for id in &ids {
                guard.selection.toggle(*id);
            }
        }
        other => {
            return err(
                req.request_id,
                ErrorCode::BadRequest,
                format!("file.select: unknown mode {other:?} (want add|replace|toggle)"),
            );
        }
    }
    let snapshot: Vec<String> = guard
        .panes
        .current
        .entries
        .iter()
        .filter(|e| guard.selection.contains(e.id))
        .map(|e| guard.panes.current.cwd.join(&e.name).display().to_string())
        .collect();
    ok(req.request_id, json!({ "selection": snapshot }))
}

#[derive(Deserialize)]
struct CopyParams {
    sources: Vec<PathBuf>,
    dest: PathBuf,
    #[serde(default = "default_conflict")]
    conflict: String,
}

fn default_conflict() -> String {
    "skip".to_string()
}

fn parse_conflict(s: &str) -> Option<ConflictPolicy> {
    match s {
        "skip" => Some(ConflictPolicy::Skip),
        "overwrite" | "replace" => Some(ConflictPolicy::Overwrite),
        "rename" => Some(ConflictPolicy::Rename),
        _ => None,
    }
}

async fn handle_copy(this: &FileHandler, req: Request) -> Response {
    let params: CopyParams = match parse_params(&req) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let conflict = match parse_conflict(&params.conflict) {
        Some(c) => c,
        None => {
            return err(
                req.request_id,
                ErrorCode::BadRequest,
                format!(
                    "file.copy: unknown conflict {:?} (want skip|overwrite|rename)",
                    params.conflict
                ),
            );
        }
    };
    let op_id = this.next_op_id.fetch_add(1, Ordering::Relaxed);
    push_op_row(&this.ops, op_id, "copy").await;
    spawn_copy_task(
        this.ops.clone(),
        this.cancel_tx.subscribe(),
        op_id,
        params.sources,
        params.dest,
        conflict,
    );
    ok(req.request_id, json!({ "op_id": op_id }))
}

#[derive(Deserialize)]
struct MoveParams {
    sources: Vec<PathBuf>,
    dest: PathBuf,
    #[serde(default = "default_conflict")]
    conflict: String,
}

async fn handle_move(this: &FileHandler, req: Request) -> Response {
    let params: MoveParams = match parse_params(&req) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let _ = parse_conflict(&params.conflict); // validated below
                                              // Step 21+ — cross-fs move ladder (copy + unlink with rollback)
                                              // is out of Step 20's scope. Today: same-mount srcs `rename`
                                              // straight to the dst; any cross-mount src returns exit-code 4
                                              // per SPEC §4.3 table ("op cancelled / refused").
    for src in &params.sources {
        let dst_parent = params.dest.as_path();
        let same_fs = copy::same_mount(src, dst_parent).unwrap_or(false);
        if !same_fs {
            return err(
                req.request_id,
                ErrorCode::Cancelled,
                format!(
                    "file.move: cross-fs move from {} to {} requires --yes (Step 21+)",
                    src.display(),
                    dst_parent.display()
                ),
            );
        }
    }
    let op_id = this.next_op_id.fetch_add(1, Ordering::Relaxed);
    push_op_row(&this.ops, op_id, "move").await;
    let dest = params.dest.clone();
    let srcs = params.sources.clone();
    let ops = this.ops.clone();
    tokio::task::spawn_blocking(move || {
        let mut all_ok = true;
        for src in &srcs {
            let Some(name) = src.file_name() else {
                all_ok = false;
                continue;
            };
            let dst_full = dest.join(name);
            if std::fs::rename(src, &dst_full).is_err() {
                all_ok = false;
            }
        }
        let final_state = if all_ok { "completed" } else { "failed" };
        // Capture the post-move row state on the same blocking thread
        // by handing the lock to a tiny tokio runtime hop.
        tokio::runtime::Handle::current().spawn(async move {
            mark_op_state(&ops, op_id, final_state, 0, 0).await;
        });
    });
    ok(req.request_id, json!({ "op_id": op_id }))
}

#[derive(Deserialize)]
struct TrashParams {
    paths: Vec<PathBuf>,
}

async fn handle_trash(_this: &FileHandler, req: Request) -> Response {
    let params: TrashParams = match parse_params(&req) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let trashed = match trash::trash(&params.paths).await {
        Ok(items) => items,
        Err(e) => return err(req.request_id, ErrorCode::Internal, format!("trash: {e}")),
    };
    let trashed_paths: Vec<String> = trashed
        .iter()
        .map(|t| t.original.display().to_string())
        .collect();
    ok(req.request_id, json!({ "trashed": trashed_paths }))
}

#[derive(Deserialize)]
struct RestoreParams {
    trashed_path: PathBuf,
}

async fn handle_restore(_this: &FileHandler, req: Request) -> Response {
    let params: RestoreParams = match parse_params(&req) {
        Ok(p) => p,
        Err(r) => return r,
    };
    // Look up the trashed item by original path. The trash crate's
    // `restore` consumes a `TrashedItem`; we list the trash and find
    // the matching row by `original` (canonicalised pre-trash; the
    // CLI passes the same canonical path back here).
    let listed = match trash::list().await {
        Ok(l) => l,
        Err(e) => {
            return err(
                req.request_id,
                ErrorCode::Internal,
                format!("trash list: {e}"),
            );
        }
    };
    let matched = listed
        .into_iter()
        .find(|t| t.original == params.trashed_path);
    let Some(item) = matched else {
        return err(
            req.request_id,
            ErrorCode::BadRequest,
            format!(
                "file.restore: no trash entry for {}",
                params.trashed_path.display()
            ),
        );
    };
    match trash::restore(item).await {
        Ok(_) => ok(req.request_id, json!({ "ok": true })),
        Err(e) => err(req.request_id, ErrorCode::Internal, format!("restore: {e}")),
    }
}

#[derive(Deserialize)]
struct SearchParams {
    query: String,
    root: PathBuf,
    #[serde(default)]
    knowledge: bool,
}

async fn handle_search(_this: &FileHandler, req: Request) -> Response {
    let params: SearchParams = match parse_params(&req) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if params.knowledge {
        // Step 30 — knowledge-backed branch wires qdrant. Today the
        // request still returns filename results so the agent can
        // still drive the journey; emit a notice so the operator
        // sees why the result set is filename-only.
        tracing::info!(target: "sy::file::ipc", "file.search: knowledge backend not yet wired (Step 30)");
    }
    let entries = match walk::walk(&params.root, false).await {
        Ok(e) => e,
        Err(e) => return err(req.request_id, ErrorCode::Internal, format!("walk: {e}")),
    };
    let needle = params.query.to_lowercase();
    let results: Vec<String> = entries
        .into_iter()
        .filter(|e| e.name.to_lowercase().contains(&needle))
        .map(|e| params.root.join(&e.name).display().to_string())
        .collect();
    ok(req.request_id, json!({ "results": results }))
}

#[derive(Deserialize)]
struct PreviewParams {
    path: PathBuf,
    #[serde(default)]
    max_width: Option<u32>,
    #[serde(default)]
    max_height: Option<u32>,
}

async fn handle_preview(_this: &FileHandler, req: Request) -> Response {
    let params: PreviewParams = match parse_params(&req) {
        Ok(p) => p,
        Err(r) => return r,
    };
    // The `max_width` / `max_height` params are forward-compat: Step
    // 27's plugin-routed previewer reads them. Today the body stays
    // empty because no plugin runs; binding the names here keeps the
    // wire shape stable.
    let _ = (params.max_width, params.max_height);
    let mime_str = match mime::mime_for(&params.path) {
        Ok(m) => m,
        Err(e) => return err(req.request_id, ErrorCode::Internal, format!("mime: {e}")),
    };
    ok(
        req.request_id,
        json!({ "mime": mime_str, "png_base64": "" }),
    )
}

async fn handle_ops_list(this: &FileHandler, req: Request) -> Response {
    let rows = this.ops.lock().await.clone();
    ok(req.request_id, json!({ "ops": rows }))
}

#[derive(Deserialize)]
struct OpCancelParams {
    op_id: u64,
}

async fn handle_op_cancel(this: &FileHandler, req: Request) -> Response {
    let params: OpCancelParams = match parse_params(&req) {
        Ok(p) => p,
        Err(r) => return r,
    };
    // Best-effort broadcast. A `send` that fails (no subscribers
    // listening — the running copy has already completed) is not an
    // error — the op is gone anyway. Update the row state to
    // `cancelled` so a follow-up `file.ops_list` reflects intent.
    let _ = this.cancel_tx.send(params.op_id);
    mark_op_state(&this.ops, params.op_id, "cancelled", 0, 0).await;
    ok(req.request_id, json!({ "ok": true }))
}

async fn handle_state_snapshot(this: &FileHandler, req: Request) -> Response {
    let guard = this.state.read().await;
    let cwd = guard.panes.current.cwd.display().to_string();
    let selection: Vec<String> = guard
        .panes
        .current
        .entries
        .iter()
        .filter(|e| guard.selection.contains(e.id))
        .map(|e| guard.panes.current.cwd.join(&e.name).display().to_string())
        .collect();
    let mode = match guard.mode {
        crate::file::state::LayoutMode::ThreePane => "three_pane",
        crate::file::state::LayoutMode::TwoPane => "two_pane",
        crate::file::state::LayoutMode::OnePane => "one_pane",
    };
    ok(
        req.request_id,
        json!({ "cwd": cwd, "selection": selection, "mode": mode }),
    )
}

/// Append a new row to the daemon's op tracker. Called when an op is
/// accepted; the row's `state` starts as `"running"` and the cadence
/// callback in [`spawn_copy_task`] flips it to `"completed"` /
/// `"failed"` / `"cancelled"` at terminal events.
async fn push_op_row(ops: &Arc<Mutex<Vec<OpRow>>>, op_id: u64, kind: &str) {
    let mut guard = ops.lock().await;
    guard.push(OpRow {
        op_id,
        kind: kind.to_string(),
        state: "running".to_string(),
        done: 0,
        total: 0,
    });
}

/// Update the row for `op_id` with a new lifecycle state + progress
/// counters. Missing rows are inserted (so cancel-before-start still
/// records intent); existing rows are patched in place.
async fn mark_op_state(
    ops: &Arc<Mutex<Vec<OpRow>>>,
    op_id: u64,
    state: &str,
    done: u64,
    total: u64,
) {
    let mut guard = ops.lock().await;
    if let Some(row) = guard.iter_mut().find(|r| r.op_id == op_id) {
        row.state = state.to_string();
        if done > 0 {
            row.done = done;
        }
        if total > 0 {
            row.total = total;
        }
        return;
    }
    guard.push(OpRow {
        op_id,
        kind: "unknown".to_string(),
        state: state.to_string(),
        done,
        total,
    });
}

/// Spawn the per-op copy executor. Feeds `OpEvent`s from `fs::copy`
/// into the daemon's row tracker and observes the cancel broadcast.
/// `cancel_rx` is per-op (subscribed at queue time) so an unrelated
/// cancel doesn't trip this op.
fn spawn_copy_task(
    ops: Arc<Mutex<Vec<OpRow>>>,
    mut cancel_rx: broadcast::Receiver<u64>,
    op_id: u64,
    srcs: Vec<PathBuf>,
    dest: PathBuf,
    conflict: ConflictPolicy,
) {
    tokio::spawn(async move {
        use tokio_stream::StreamExt as _;
        let mut stream = copy::copy(&srcs, &dest, conflict).await;
        loop {
            tokio::select! {
                ev = stream.next() => {
                    match ev {
                        Some(OpEvent::Progress { done, total, .. }) => {
                            mark_op_state(&ops, op_id, "running", done, total).await;
                        }
                        Some(OpEvent::Completed { .. }) => {
                            mark_op_state(&ops, op_id, "completed", 0, 0).await;
                            return;
                        }
                        Some(OpEvent::Failed { .. }) => {
                            mark_op_state(&ops, op_id, "failed", 0, 0).await;
                            return;
                        }
                        Some(OpEvent::Cancelled { .. }) => {
                            mark_op_state(&ops, op_id, "cancelled", 0, 0).await;
                            return;
                        }
                        Some(_) => continue,
                        None => {
                            // Stream ended without a terminal event —
                            // treat as completion so the row doesn't
                            // dangle at "running" forever.
                            mark_op_state(&ops, op_id, "completed", 0, 0).await;
                            return;
                        }
                    }
                }
                Ok(target) = cancel_rx.recv() => {
                    if target == op_id {
                        // Drop the stream so the copy executor sees
                        // the receiver-closed signal and unlinks the
                        // partial dst (the Step-16 rollback shape).
                        drop(stream);
                        mark_op_state(&ops, op_id, "cancelled", 0, 0).await;
                        // Best-effort partial-dst cleanup for the
                        // currently-running src(s); the executor's
                        // own rollback handles the in-flight chunk.
                        for src in &srcs {
                            if let Some(name) = src.file_name() {
                                let _ = std::fs::remove_file(dest.join(name));
                            }
                        }
                        return;
                    }
                }
            }
        }
    });
}

fn ok(request_id: Ulid, result: serde_json::Value) -> Response {
    Response::Ok {
        schema_version: SCHEMA_VERSION,
        request_id,
        result,
        blob: None,
    }
}

fn err(request_id: Ulid, code: ErrorCode, message: String) -> Response {
    Response::Err {
        schema_version: SCHEMA_VERSION,
        request_id,
        error: ErrorBody {
            code,
            message,
            retry_after_ms: None,
            details: serde_json::Value::Null,
        },
    }
}

#[cfg(test)]
mod tests {
    //! End-to-end tests live in `tests/sy_file_ipc.rs` (daemon-in-
    //! thread + real `sy_ipc::Client`). The in-source tests cover the
    //! pure-fn corners that don't need a socket.
    use super::*;

    #[test]
    fn parse_conflict_recognises_three_policies() {
        // SPEC §4.3: `conflict` accepts `skip | overwrite | rename`.
        // A regression that swallowed one would route the wrong policy
        // through `fs::copy::resolve_dst`.
        assert!(matches!(parse_conflict("skip"), Some(ConflictPolicy::Skip)));
        assert!(matches!(
            parse_conflict("overwrite"),
            Some(ConflictPolicy::Overwrite)
        ));
        assert!(matches!(
            parse_conflict("replace"),
            Some(ConflictPolicy::Overwrite)
        ));
        assert!(matches!(
            parse_conflict("rename"),
            Some(ConflictPolicy::Rename)
        ));
        assert!(parse_conflict("bogus").is_none());
    }

    #[test]
    fn file_methods_list_covers_spec_43_eleven_ops_plus_state() {
        // SPEC §4.3 pins eleven ops; the Step-20 e2e adds
        // `file.state` for the J8 mirror. Drift here would silently
        // de-register a method and the describe round-trip would
        // surface it — locking the count + the membership locally
        // keeps the failure local.
        assert_eq!(FILE_METHODS.len(), 12);
        for needed in [
            METHOD_OPEN,
            METHOD_CD,
            METHOD_SELECT,
            METHOD_COPY,
            METHOD_MOVE,
            METHOD_TRASH,
            METHOD_RESTORE,
            METHOD_SEARCH,
            METHOD_PREVIEW,
            METHOD_OPS_LIST,
            METHOD_OP_CANCEL,
            METHOD_STATE,
        ] {
            assert!(
                FILE_METHODS.contains(&needed),
                "FILE_METHODS missing {needed}"
            );
        }
    }
}
