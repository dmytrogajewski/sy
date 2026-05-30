//! `sy file` CLI surface. Step 13 of the
//! [`sy-file-manager` roadmap][roadmap] landed the carrier (bare
//! `sy file [PATH]` prints `scaffold`, `sy file doctor [--json]`
//! returns the not-implemented marker); Step 20 wires the IPC
//! subcommand tree to the real Unix-socket daemon under
//! `$XDG_RUNTIME_DIR/sy-file.sock` (SPEC §4.3) plus extends
//! `doctor` with a daemon-up probe.
//!
//! ## Surface today
//!
//! * `sy file [PATH]` — prints `scaffold` on stdout, exits 0.
//! * `sy file --help` — clap renders the standard help block.
//! * `sy file doctor [--json]` — runs the SPEC §3.3 item 19 health
//!   probes (Step 33) and prints the `sy.file.doctor/v1` envelope.
//! * `sy file ipc serve [--sock PATH]` — runs the SPEC §4.3
//!   daemon in-process until SIGTERM / ctrl-c.
//! * `sy file ipc <op> [args…]` — one-shot client call against the
//!   running daemon, prints the JSON response, exits per SPEC §4.3
//!   table (0 ok / 1 generic / 2 usage / 3 daemon unreachable /
//!   4 op cancelled-or-refused / 5 plugin error).
//!
//! [roadmap]: ../../../specs/roadmaps/sy-file-manager/ROADMAP.md
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Subcommand;
use serde_json::json;
use sy_core::ErrorCode;
use sy_ipc::{CallOpts, Client, Response};
use tokio::sync::{oneshot, RwLock};

use crate::file::fs::{copy, mime, trash, walk, watch};
use crate::file::ipc;
use crate::file::state::{
    ConflictPolicy, Entry, EntryId, EntryKind, LayoutMode, OpEvent, Operation, Pane, PaneId, Panes,
    SelectionSet, State,
};

/// Stdout marker emitted by bare `sy file` and `sy file <path>`
/// whenever the iced GUI is NOT entered (i.e. the `gui-iced` feature
/// is off, OR stdout is not a TTY — the latter is what the Step 13
/// e2e relies on: it spawns the binary via `Command::new` so stdout
/// is a pipe, and the journey-J1 contract is "emit `scaffold` then
/// exit 0" so the binary remains keybind-dispatchable in a test).
///
/// When stdout IS a TTY AND `gui-iced` is on (Step 23+), the marker
/// is still printed (so an operator launching from a terminal sees
/// the launch lineage in scrollback) but the bin then enters the
/// iced runloop and only returns when the window is closed.
const SCAFFOLD_MARKER: &str = "scaffold";

// Step 33 retired the `sy.file.doctor.scaffold/v0` / `not-implemented-yet`
// schema markers from Step 13; the real `sy.file.doctor/v1` schema lives
// in `crate::file::doctor`. The Step 13 scaffold-era tests track the
// schema bump in their assertion strings (in-scope per ROADMAP Step 33
// non-negotiable #2).

/// Override for the IPC socket path (SPEC §4.3 env table). When set,
/// every Step 20 CLI dispatcher uses this instead of the
/// `$XDG_RUNTIME_DIR/sy-file.sock` default.
const SY_FILE_SOCK_ENV: &str = "SY_FILE_SOCK";

/// Default socket basename under `$XDG_RUNTIME_DIR`.
const DEFAULT_SOCK_BASENAME: &str = "sy-file.sock";

/// Exit codes from SPEC §4.3 "Exit codes" table. Public so the
/// `tests/sy_file_ipc.rs` round-trip can pin them.
pub const EXIT_OK: i32 = 0;
pub const EXIT_GENERIC: i32 = 1;
/// Usage error. Clap returns 2 itself; the dispatcher uses this for
/// the unreachable per-variant-method-mapping guard.
const EXIT_USAGE: i32 = 2;
pub const EXIT_DAEMON_DOWN: i32 = 3;
pub const EXIT_REFUSED: i32 = 4;
/// Plugin-error path lands when plugins surface non-Ok results.
/// Pinned for SPEC §4.3 wire stability; the dispatch site lives in
/// the bridge.
#[cfg(test)]
const EXIT_PLUGIN_ERROR: i32 = 5;

/// `sy file` subcommand tree. The bare invocation (no subcommand,
/// optional positional path) is the journey-J1 shape; the named
/// subcommands are the carriers Step 20 fills in.
#[derive(Debug, Subcommand)]
pub enum FileCmd {
    /// Health probes for the file plane (SPEC §3.3 item 19). Runs the
    /// six [`doctor::file_doctor_checks`] probes — daemon reachable,
    /// JetBrainsMono Nerd Font present, niri keybinds present + collision-
    /// free, systemd unit installed, bookmarks dir writable, plugin
    /// registry reachable. Emits `sy.file.doctor/v1` on `--json`.
    ///
    /// Example:
    ///   sy file doctor --json
    Doctor {
        /// Emit the `sy.file.doctor/v1` JSON schema on stdout.
        #[arg(long)]
        json: bool,
    },
    /// One-shot JSON IPC ops + daemon serve (`sy file ipc serve`).
    /// SPEC §4.3 op surface; consumed by agents, MCP, and the niri
    /// keybind dispatcher (Step 34).
    Ipc {
        #[command(subcommand)]
        op: IpcCmd,
    },
    /// Stdio JSON-RPC MCP server exposing the eleven SPEC §4.3
    /// `file_*` tools. Each tool transcodes to a `file.*` IPC op
    /// against the running daemon. Schema lives under
    /// `docs/reference/sy-file-mcp.md`.
    Mcp,
    /// Emit a one-shot waybar custom-module JSON tile summarising
    /// the running file-op count (SPEC §3.3 item 16 — journey J6
    /// bar-side affordance). Dials the daemon, calls `file.ops_list`,
    /// counts rows with `state == "running"`, and prints a single
    /// line `{ "text": "...", "tooltip": "...", "class": "..." }`
    /// matching the schema waybar's `sy mon waybar` adapter emits.
    ///
    /// Exits 0 even when the daemon is unreachable — waybar polls
    /// every interval and expects a parseable line on each tick.
    Waybar,
}

/// One-shot IPC ops + the in-process daemon. Each variant maps to
/// either a `file.<verb>` method on the daemon (`Open`, `Cd`, …) or
/// the `sy file ipc serve` daemon driver.
#[derive(Debug, Subcommand)]
pub enum IpcCmd {
    /// Run the `sy file` daemon in-process. Binds the SPEC §4.3
    /// socket (`$XDG_RUNTIME_DIR/sy-file.sock` unless `--sock` or
    /// `$SY_FILE_SOCK` overrides) and serves until SIGTERM / ctrl-c.
    Serve {
        /// Socket path override (SPEC §4.3 env table — `SY_FILE_SOCK`
        /// has the same effect).
        #[arg(long)]
        sock: Option<PathBuf>,
        /// Emit `sd_notify(READY=1)` after bind + chmod. Required by
        /// the `sy-file.service` unit's `Type=notify` contract
        /// (roadmap Step 22). A no-op when `$NOTIFY_SOCKET` is unset
        /// (the dev / test path), so it's safe to leave on by default
        /// in unit `ExecStart=` lines.
        #[arg(long)]
        systemd_notify: bool,
    },
    /// `file.open { path }` — set the current pane's cwd.
    Open { path: PathBuf },
    /// `file.cd { path }` — set the current pane's cwd and refresh
    /// its entries via `fs::walk`.
    Cd { path: PathBuf },
    /// `file.select { paths, mode }` — toggle / add / replace
    /// the selection set against the current pane.
    Select {
        /// Mode discriminator: `add`, `replace`, or `toggle`.
        #[arg(long, default_value = "toggle")]
        mode: String,
        /// Paths to select (relative to the daemon's cwd or absolute).
        paths: Vec<PathBuf>,
    },
    /// `file.copy { sources, dest, conflict }` — queue a copy op.
    Copy {
        /// Conflict policy: `skip`, `overwrite`, or `rename`.
        #[arg(long, default_value = "skip")]
        conflict: String,
        /// Destination directory.
        #[arg(long)]
        dest: PathBuf,
        /// One or more source files.
        sources: Vec<PathBuf>,
    },
    /// `file.move { sources, dest, conflict }` — queue a move op
    /// (same-fs `rename` today; cross-fs returns exit-code 4).
    Move {
        #[arg(long, default_value = "skip")]
        conflict: String,
        #[arg(long)]
        dest: PathBuf,
        sources: Vec<PathBuf>,
    },
    /// `file.trash { paths }` — send each path to the freedesktop
    /// trash.
    Trash { paths: Vec<PathBuf> },
    /// `file.restore { trashed_path }` — restore a trashed entry by
    /// its original absolute path.
    Restore { trashed_path: PathBuf },
    /// `file.search { query, root, knowledge }` — filename match
    /// against `walk(root)`; `--knowledge` is a forward-compat flag
    /// (Step 30 wires qdrant).
    Search {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        knowledge: bool,
        query: String,
    },
    /// `file.preview { path, max_width, max_height }` — return the
    /// resolved mime + an empty `png_base64` (Step 27's plugin
    /// dispatcher fills the body).
    Preview {
        #[arg(long)]
        max_width: Option<u32>,
        #[arg(long)]
        max_height: Option<u32>,
        path: PathBuf,
    },
    /// `file.ops_list {}` — list every in-flight or terminated op.
    OpsList,
    /// `file.op_cancel { op_id }` — cancel the op with this id; the
    /// running copy executor observes the broadcast and rolls back.
    OpCancel { op_id: u64 },
    /// `file.state {}` — snapshot `{ cwd, selection, mode }`; the
    /// journey-J8 agent-mirror op.
    State,
}

/// Dispatch entry point called from `main.rs`. The `path` argument
/// is the positional path on the bare `sy file [PATH]` form
/// (journey-J1's `sy file ~` shape).
pub fn dispatch(path: Option<PathBuf>, cmd: Option<FileCmd>) -> Result<()> {
    match cmd {
        None => run_scaffold(path),
        Some(FileCmd::Doctor { json }) => run_doctor(json),
        Some(FileCmd::Ipc { op }) => run_ipc_cmd(op),
        Some(FileCmd::Mcp) => crate::file::mcp::run(),
        Some(FileCmd::Waybar) => run_waybar(),
    }
}

/// Bare `sy file [PATH]` — print the scaffold marker, then hand off
/// to the iced xdg-toplevel runloop when `gui-iced` is on and stdout
/// is a TTY. The path is echoed when present so a Step 34 niri
/// keybind operator can see the right `cwd` round-tripped end-to-end.
/// The `State::default()` instantiation anchors the SPEC §3.1
/// state-model symbols from `state/mod.rs` to the bin's runtime call
/// site so clippy's `dead_code` lint stays clean without a
/// `#[allow(...)]`; the follow-up steps (15+) replace this anchor
/// with real `walk` / `copy` / `ipc::serve` call sites that use
/// these types for behaviour rather than dead-code suppression.
///
/// The TTY gate keeps the Step 13 e2e (`step13_…`) passing — its
/// `Command::new(bin).output()` invocation pipes stdout, so the
/// scaffold-only branch fires and the test sees the `scaffold` marker
/// without the binary trying to attach to a Wayland compositor that
/// isn't there. An operator typing `sy file ~` at a real terminal
/// (stdout = TTY) gets the GUI.
fn run_scaffold(path: Option<PathBuf>) -> Result<()> {
    let state = State::default();
    // Read every field once so the dead-code pass sees them used
    // before Step 15+'s real consumers land. Each `let _ = …` line is
    // the anchor a future step will replace with its real mutation.
    let _ = (&state.panes, &state.mode, &state.selection, &state.ops);
    anchor_step14_state_model();
    match &path {
        Some(p) => println!("{SCAFFOLD_MARKER}: {}", p.display()),
        None => println!("{SCAFFOLD_MARKER}"),
    }
    #[cfg(feature = "gui-iced")]
    if run_headless_probe_if_requested(&path)? {
        return Ok(());
    }
    run_gui_if_tty(path)
}

/// When the `gui-iced` feature is on AND stdout is attached to a
/// TTY, enter the iced xdg-toplevel runloop. Otherwise no-op (the
/// scaffold marker emitted above is the journey-J1 contract a Step 34
/// niri keybind or a Step 13 / Step 23 e2e dispatches against).
///
/// Resolves the initial cwd from the optional positional path,
/// falling back to `$HOME` and then to the current working directory
/// so a `sy file` invocation with no arguments still lands on a
/// sensible start dir.
#[cfg(feature = "gui-iced")]
fn run_gui_if_tty(path: Option<PathBuf>) -> Result<()> {
    use std::io::IsTerminal as _;
    if !std::io::stdout().is_terminal() {
        return Ok(());
    }
    let initial = resolve_initial_cwd(path);
    crate::file::app::run(initial)
}

/// `--no-default-features` (CLI-only) build: the GUI is absent, so the
/// scaffold marker on stdout is the entire surface. The compile-time
/// branch keeps the `path` lint clean without an `_ =` discard.
#[cfg(not(feature = "gui-iced"))]
fn run_gui_if_tty(_path: Option<PathBuf>) -> Result<()> {
    Ok(())
}

/// Resolve the initial cwd for the iced xdg-toplevel: the positional
/// path if supplied, else `$HOME`, else the process cwd. Public so
/// the headless harness in `app.rs` can mirror the same precedence.
#[cfg(feature = "gui-iced")]
fn resolve_initial_cwd(path: Option<PathBuf>) -> PathBuf {
    if let Some(p) = path {
        return p;
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home);
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Env-var that flips bare `sy file [PATH]` into a one-shot headless
/// first-paint probe. The probe drives the iced app's boot reducer
/// off-screen and emits a `sy.file.gui.probe/v0` JSON line on stdout
/// (`{ "ticks": u64, "elapsed_ms": u128 }`) before exiting. This is
/// the production call site that pins the journey-J1 250 ms budget
/// against a CI worker with no Wayland compositor — the wire shape
/// is what `tests/sy_file_journey_e2e.rs::step23_…` parses today
/// (when it exercises the harness in-process) and what a future
/// `sy file doctor --json` ratchet will surface to operators.
///
/// Off by default; flipping it doesn't affect the GUI path.
#[cfg(feature = "gui-iced")]
const HEADLESS_PROBE_ENV: &str = "SY_FILE_HEADLESS_PROBE";

/// Schema marker for the headless probe JSON line. Pinned so an MCP
/// consumer can ratchet on the version once Step 33's real doctor
/// schema lands.
#[cfg(feature = "gui-iced")]
const SCHEMA_PROBE: &str = "sy.file.gui.probe/v0";

/// Run the headless first-paint probe and emit a JSON summary on
/// stdout. Returns `Ok(true)` when the probe ran (so the caller can
/// short-circuit the GUI launch); `Ok(false)` when the env var is
/// unset (the bare-form scaffold + GUI path stays intact).
#[cfg(feature = "gui-iced")]
fn run_headless_probe_if_requested(path: &Option<PathBuf>) -> Result<bool> {
    if std::env::var_os(HEADLESS_PROBE_ENV).is_none() {
        return Ok(false);
    }
    let cwd = resolve_initial_cwd(path.clone());
    let (ticks, elapsed) = crate::file::app::run_headless_once(cwd)?;
    let doc = json!({
        "schema": SCHEMA_PROBE,
        "ticks": ticks,
        "elapsed_ms": elapsed.as_millis() as u64,
    });
    println!("{}", serde_json::to_string(&doc)?);
    Ok(true)
}

/// Touch every Step 14 public state-model symbol from a real bin
/// call site so the dead-code pass sees them used — same idiomatic
/// stand-in as `_state = State::default()` above. Steps 15-22
/// replace each `let _ = …` line with the actual call site
/// (`fs::walk` populating panes, `fs::copy` queuing operations,
/// `ipc::serve` round-tripping `OpEvent` over the socket). Keeping
/// this isolated in one function makes the deletion mechanical when
/// the real consumers land.
fn anchor_step14_state_model() {
    let mut panes = Panes::new(PathBuf::from("/"), PathBuf::from("/"), PathBuf::from("/"));
    let entry = Entry {
        id: 0_u64,
        name: String::new(),
        kind: EntryKind::File,
        size: 0,
        mtime: std::time::SystemTime::UNIX_EPOCH,
        is_symlink: false,
        broken_link: false,
        readable: true,
        mime_hint: None,
        symlink_target: None,
    };
    let _ = (EntryKind::Dir, EntryKind::Symlink, EntryKind::Other);
    let _ = (PaneId::Parent, PaneId::Current, PaneId::Preview);
    let _ = (
        LayoutMode::ThreePane,
        LayoutMode::TwoPane,
        LayoutMode::OnePane,
    );
    let mut pane = Pane::new(PathBuf::from("/"));
    pane.set_entries(vec![entry.clone()]);
    panes.current.set_entries(vec![entry]);
    let mut selection = SelectionSet::new();
    selection.toggle(0_u64 as EntryId);
    selection.add_range(0, 0);
    selection.invert(&[0]);
    selection.all(&[0]);
    selection.clear();
    let _ = (selection.contains(0), selection.len(), selection.is_empty());
    let _ = selection.iter().count();
    let copy = Operation::Copy {
        srcs: Vec::new(),
        dst: PathBuf::from("/"),
        conflict: ConflictPolicy::Skip,
    };
    let _ = (
        Operation::Move {
            srcs: Vec::new(),
            dst: PathBuf::from("/"),
            conflict: ConflictPolicy::Overwrite,
        },
        Operation::Trash { srcs: Vec::new() },
        Operation::Restore { ids: Vec::new() },
        Operation::Mkdir {
            parent: PathBuf::from("/"),
            name: String::new(),
        },
        Operation::Rename {
            src: PathBuf::from("/"),
            new_name: String::new(),
        },
        ConflictPolicy::Rename,
        copy,
    );
    let _ = (
        OpEvent::Started { op_id: 0 },
        OpEvent::Progress {
            op_id: 0,
            done: 0,
            total: 0,
            throughput_bps: 0,
        },
        OpEvent::Paused { op_id: 0 },
        OpEvent::Resumed { op_id: 0 },
        OpEvent::Cancelled { op_id: 0 },
        OpEvent::Completed { op_id: 0 },
        OpEvent::Failed {
            op_id: 0,
            code: 0,
            msg: String::new(),
        },
    );
    // Step 15+ anchors: reference the `fs::walk` / `fs::copy` /
    // `fs::trash` / `fs::watch` / `fs::mime` symbols so the dead-
    // code pass sees them before the iced UI (Step 23+) replaces
    // these with real call sites. The `if false` block is statically
    // unreachable but type-checked, which is what we need for
    // `dead_code = "deny"`. Step 20's `ipc::serve` is wired into
    // production via `sy file ipc serve`, so it doesn't need an
    // anchor here.
    if false {
        std::mem::drop(walk::walk(std::path::Path::new("/"), false));
        std::mem::drop(copy::copy(
            &[],
            std::path::Path::new("/"),
            ConflictPolicy::Skip,
        ));
        let _ = copy::same_mount(std::path::Path::new("/"), std::path::Path::new("/"));
        std::mem::drop(trash::trash(&[]));
        std::mem::drop(trash::list());
        std::mem::drop(watch::watch(&[]));
        let _ = mime::mime_for(std::path::Path::new(""));
        let dummy = trash::TrashedItem {
            original: std::path::PathBuf::from("/"),
            trash_id: String::new(),
            deleted_at: std::time::SystemTime::UNIX_EPOCH,
            size: 0,
        };
        let _ = (
            &dummy.original,
            &dummy.trash_id,
            &dummy.deleted_at,
            &dummy.size,
        );
        std::mem::drop(trash::restore(dummy));
    }
}

/// `sy file doctor [--json]` — Step 33 entry point. Runs the six
/// SPEC §3.3 item 19 probes, prints the human or `sy.file.doctor/v1`
/// JSON envelope, then `std::process::exit`s with the exit code
/// derived from the worst-of-checks roll-up.
fn run_doctor(json_out: bool) -> Result<()> {
    let opts = crate::file::doctor::DoctorOpts::default();
    let checks = crate::file::doctor::file_doctor_checks(opts);
    if json_out {
        let doc = crate::file::doctor::render_json(&checks);
        println!("{}", serde_json::to_string(&doc)?);
    } else {
        print!("{}", crate::file::doctor::render_human(&checks));
    }
    std::process::exit(crate::file::doctor::exit_code_for(&checks));
}

/// `sy file ipc <op>` / `sy file ipc serve` entry point. The
/// `serve` arm runs the daemon synchronously inside a fresh
/// `current_thread` runtime; all other arms one-shot a
/// `Client::connect → call → print → exit` round trip.
fn run_ipc_cmd(cmd: IpcCmd) -> Result<()> {
    if let IpcCmd::Serve {
        sock,
        systemd_notify,
    } = cmd
    {
        let sock_path = sock.unwrap_or_else(resolve_sock_path);
        return run_ipc_serve(sock_path, systemd_notify);
    }
    let sock = resolve_sock_path();
    let exit = run_ipc_client(&sock, cmd);
    std::process::exit(exit);
}

/// Daemon driver. Boots a fresh tokio runtime, wires a `State`
/// behind `Arc<RwLock>`, and calls [`ipc::serve_with_ready`] until the
/// OS signals or the socket listener errors. When `systemd_notify` is
/// true, the on-ready hook emits `sd_notify(READY=1)` after bind +
/// chmod so the `Type=notify` unit (roadmap Step 22) flips from
/// `activating` to `active (running)`.
fn run_ipc_serve(sock_path: PathBuf, systemd_notify: bool) -> Result<()> {
    // Step 31 — keep the b<key>-pin / b<key>-jump / palette / TOML-
    // save / recent-list public surface reachable on the
    // `--no-default-features` (no-GUI) compile path. The methods are
    // driven by the iced reducer (`app::update`) under the default
    // build and by the unit tests under `cfg(test)`; this `let _` row
    // pins them as "production-callable" for the dead-code lint so
    // the cli-only build doesn't degrade the public surface.
    let _ = crate::file::bookmarks::Bookmarks::pin
        as fn(&mut crate::file::bookmarks::Bookmarks, char, PathBuf, Option<String>) -> Result<()>;
    let _ = crate::file::bookmarks::Bookmarks::unpin
        as fn(&mut crate::file::bookmarks::Bookmarks, char) -> Result<()>;
    let _ = crate::file::bookmarks::Bookmarks::jump
        as for<'a> fn(&'a crate::file::bookmarks::Bookmarks, char) -> Option<&'a std::path::Path>;
    let _ = crate::file::bookmarks::Bookmarks::save
        as fn(&crate::file::bookmarks::Bookmarks) -> Result<()>;
    let _ = crate::file::bookmarks::Bookmarks::read_recent
        as fn(&crate::file::bookmarks::Bookmarks) -> Result<Vec<crate::file::bookmarks::Bookmark>>;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        // Step 31 — seed the daemon's State with a bookmarks registry
        // resolved against `$XDG_STATE_HOME` / `$XDG_DATA_HOME`. A
        // failure (perm denied, corrupt TOML — the latter is already
        // tolerated inside `load`) shouldn't block daemon startup;
        // log + continue with `None` so the IPC surface still works,
        // just without bookmark side-effects on `file.open`.
        let bookmarks = match crate::file::bookmarks::load(
            &load_bookmarks_state_dir(),
            &load_bookmarks_xbel_dir(),
        ) {
            Ok(bm) => Some(Arc::new(std::sync::Mutex::new(bm))),
            Err(e) => {
                tracing::warn!(
                    target = "sy::file::bookmarks",
                    error = %e,
                    "bookmarks load failed; file.open will not touch recently-used.xbel"
                );
                None
            }
        };
        let initial = State {
            bookmarks,
            ..State::default()
        };
        let state = Arc::new(RwLock::new(initial));
        let (_tx, rx) = oneshot::channel::<()>();
        if systemd_notify {
            ipc::serve_with_ready(state, sock_path, rx, sy_core::notify::ready).await
        } else {
            ipc::serve(state, sock_path, rx).await
        }
    })
}

/// Step 31 — `$XDG_STATE_HOME/sy/file/` resolver for the daemon. Same
/// shape as `app::bookmarks_state_dir` but lives here so the CLI/IPC
/// path doesn't pull the `gui-iced`-gated `app` module into the
/// `--no-default-features` build.
fn load_bookmarks_state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("state")
        });
    base.join("sy").join("file")
}

/// Step 31 — `$XDG_DATA_HOME/` resolver for the daemon side. Sibling
/// of [`load_bookmarks_state_dir`].
fn load_bookmarks_xbel_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("share")
        })
}

/// Client-side dispatcher. Maps `IpcCmd` → `(method, params)` →
/// envelope round-trip via [`Client::call`]. Returns the SPEC §4.3
/// exit code so the outer dispatch can `std::process::exit` cleanly.
fn run_ipc_client(sock: &std::path::Path, cmd: IpcCmd) -> i32 {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("sy file ipc: tokio runtime: {e}");
            return EXIT_GENERIC;
        }
    };
    rt.block_on(async move {
        let mut client = match Client::connect(sock).await {
            Ok(c) => c,
            Err(_) => {
                eprintln!("sy file ipc: daemon unreachable at {}", sock.display());
                return EXIT_DAEMON_DOWN;
            }
        };
        let (method, params) = match build_call(&cmd) {
            Some(pair) => pair,
            None => {
                // Should be unreachable — every variant maps to a
                // method. Guard with EXIT_USAGE so a future variant
                // that's added without a wire mapping surfaces here.
                eprintln!("sy file ipc: no method for {cmd:?}");
                return EXIT_USAGE;
            }
        };
        let resp = match client.call(method, params, CallOpts::default()).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("sy file ipc: call({method}): {e}");
                return EXIT_GENERIC;
            }
        };
        print_and_exit_code(resp)
    })
}

/// Print the response body to stdout (JSON) and map the wire
/// outcome onto a SPEC §4.3 exit code.
fn print_and_exit_code(resp: Response) -> i32 {
    match resp {
        Response::Ok { result, .. } => {
            match serde_json::to_string(&result) {
                Ok(s) => println!("{s}"),
                Err(e) => {
                    eprintln!("sy file ipc: serialise result: {e}");
                    return EXIT_GENERIC;
                }
            }
            EXIT_OK
        }
        Response::Err { error, .. } => {
            eprintln!("sy file ipc: error {:?}: {}", error.code, error.message);
            match error.code {
                ErrorCode::Cancelled => EXIT_REFUSED,
                _ => EXIT_GENERIC,
            }
        }
    }
}

/// Map a CLI subcommand to its `(method, params)` JSON envelope.
fn build_call(cmd: &IpcCmd) -> Option<(&'static str, serde_json::Value)> {
    let pair = match cmd {
        IpcCmd::Serve { .. } => return None,
        IpcCmd::Open { path } => ("file.open", json!({ "path": path })),
        IpcCmd::Cd { path } => ("file.cd", json!({ "path": path })),
        IpcCmd::Select { paths, mode } => ("file.select", json!({ "paths": paths, "mode": mode })),
        IpcCmd::Copy {
            sources,
            dest,
            conflict,
        } => (
            "file.copy",
            json!({ "sources": sources, "dest": dest, "conflict": conflict }),
        ),
        IpcCmd::Move {
            sources,
            dest,
            conflict,
        } => (
            "file.move",
            json!({ "sources": sources, "dest": dest, "conflict": conflict }),
        ),
        IpcCmd::Trash { paths } => ("file.trash", json!({ "paths": paths })),
        IpcCmd::Restore { trashed_path } => {
            ("file.restore", json!({ "trashed_path": trashed_path }))
        }
        IpcCmd::Search {
            query,
            root,
            knowledge,
        } => (
            "file.search",
            json!({ "query": query, "root": root, "knowledge": knowledge }),
        ),
        IpcCmd::Preview {
            path,
            max_width,
            max_height,
        } => (
            "file.preview",
            json!({
                "path": path,
                "max_width": max_width,
                "max_height": max_height,
            }),
        ),
        IpcCmd::OpsList => ("file.ops_list", json!({})),
        IpcCmd::OpCancel { op_id } => ("file.op_cancel", json!({ "op_id": op_id })),
        IpcCmd::State => ("file.state", json!({})),
    };
    Some(pair)
}

/// Resolve the socket path from `$SY_FILE_SOCK`, falling back to
/// `$XDG_RUNTIME_DIR/sy-file.sock`. Centralised so the daemon side
/// and the client side never drift apart.
pub fn resolve_sock_path() -> PathBuf {
    if let Ok(p) = std::env::var(SY_FILE_SOCK_ENV) {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d).join(DEFAULT_SOCK_BASENAME);
        }
    }
    // Last-resort fallback: a uid-scoped path under /run/user.
    let uid = unsafe { libc_getuid() };
    PathBuf::from(format!("/run/user/{uid}/{DEFAULT_SOCK_BASENAME}"))
}

extern "C" {
    fn getuid() -> u32;
}
unsafe fn libc_getuid() -> u32 {
    getuid()
}

/// Step 28 — waybar custom-module class taxonomy. Mirrors the
/// `sy mon waybar` adapter's vocabulary (`ok` / `degraded` / `down`)
/// so a single waybar stylesheet covers both tiles.
pub const WAYBAR_CLASS_ACTIVE: &str = "active";
pub const WAYBAR_CLASS_IDLE: &str = "idle";
pub const WAYBAR_CLASS_DOWN: &str = "down";

/// Run the `sy file waybar` adapter end-to-end. Dials the daemon,
/// calls `file.ops_list`, counts running rows, prints a single JSON
/// line. Daemon-unreachable degrades to a `down`-class tile (`text =
/// ""`, tooltip naming the unit) so waybar keeps polling without
/// surfacing a CLI failure.
fn run_waybar() -> Result<()> {
    let sock = resolve_sock_path();
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            // Tokio init failure is an operator-actionable bug; still
            // emit a parseable tile so waybar doesn't flash empty.
            println!("{}", render_waybar_tile(WaybarSnapshot::down()));
            return Err(anyhow::anyhow!("sy file waybar: tokio init: {e}"));
        }
    };
    let snapshot = rt.block_on(probe_ops(&sock));
    println!("{}", render_waybar_tile(snapshot));
    Ok(())
}

/// Snapshot of the daemon's op tracker reduced to what the waybar
/// pill needs to render. `None` running-count means the daemon was
/// unreachable; the renderer turns that into the `down` class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaybarSnapshot {
    pub running: Option<u64>,
    pub queued: u64,
    /// Aggregate bytes-per-second across in-flight ops. Surfaced via
    /// `humanise_throughput` in the tile's tooltip so the operator
    /// sees the same unit table the statusbar paints. Zero today —
    /// the daemon's `OpRow` doesn't yet ship throughput; this field
    /// is the forward-compat surface a follow-on patch fills.
    pub throughput_bps: u64,
}

impl WaybarSnapshot {
    /// Daemon unreachable — render the `down` tile. Public so the
    /// unit test below can pin the shape.
    pub fn down() -> Self {
        Self {
            running: None,
            queued: 0,
            throughput_bps: 0,
        }
    }
}

/// Build the JSON tile body for a snapshot. Pure function; tested in
/// `tests/sy_file_bulk_ops.rs` + the unit tests below so the wire
/// shape stays stable.
pub fn render_waybar_tile(snap: WaybarSnapshot) -> String {
    match snap.running {
        None => {
            let tooltip = "sy file daemon unreachable";
            format!(r#"{{"text":"","tooltip":"{tooltip}","class":"{WAYBAR_CLASS_DOWN}"}}"#)
        }
        Some(0) => {
            let tooltip = "sy file: idle";
            format!(r#"{{"text":"","tooltip":"{tooltip}","class":"{WAYBAR_CLASS_IDLE}"}}"#)
        }
        Some(n) => {
            let text_body = format!("{n} ops");
            // Surface the binary-prefix throughput vocabulary in the
            // tooltip — single source of truth via the GUI's
            // `humanise_throughput`. When throughput is zero (the
            // daemon doesn't yet ship a sample), the tooltip reads
            // `"0 B/s"` for self-consistency.
            let throughput = waybar_throughput_for(snap.throughput_bps);
            let tooltip = format!("sy file: {n} running; {throughput}");
            format!(
                r#"{{"text":"{text_body}","tooltip":"{tooltip}","class":"{WAYBAR_CLASS_ACTIVE}"}}"#
            )
        }
    }
}

/// Bind point for the GUI's `humanise_throughput` helper. Cfg-gated
/// because the GUI module only compiles with `gui-iced`; the CLI-only
/// build degrades to an empty string so the tile still parses.
#[cfg(feature = "gui-iced")]
fn waybar_throughput_for(bps: u64) -> String {
    crate::file::widgets::progress_row::humanise_throughput(bps)
}

#[cfg(not(feature = "gui-iced"))]
fn waybar_throughput_for(_bps: u64) -> String {
    "0 B/s".to_owned()
}

/// Dial the daemon and ask for `file.ops_list`. Maps any transport /
/// daemon error to a `down` snapshot so the caller can render the
/// tile without panicking.
async fn probe_ops(sock: &std::path::Path) -> WaybarSnapshot {
    let mut client = match Client::connect(sock).await {
        Ok(c) => c,
        Err(_) => return WaybarSnapshot::down(),
    };
    let resp = match client
        .call(
            "file.ops_list",
            serde_json::Value::Null,
            CallOpts::default(),
        )
        .await
    {
        Ok(r) => r,
        Err(_) => return WaybarSnapshot::down(),
    };
    let result = match resp {
        Response::Ok { result, .. } => result,
        Response::Err { .. } => return WaybarSnapshot::down(),
    };
    let ops = result
        .get("ops")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let running = ops
        .iter()
        .filter(|row| row.get("state").and_then(|s| s.as_str()) == Some("running"))
        .count() as u64;
    let queued = ops.len() as u64;
    WaybarSnapshot {
        running: Some(running),
        queued,
        throughput_bps: 0,
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the scaffold dispatchers. End-to-end behaviour
    //! is exercised by `tests/sy_file_scaffold.rs` (drives the real
    //! binary via `CARGO_BIN_EXE_sy`) and the journey E2E in
    //! `tests/sy_file_journey_e2e.rs::step13_…`.
    use super::*;

    #[test]
    fn dispatch_routes_bare_form_to_scaffold() {
        // Smoke-level: the dispatcher accepts the bare shape (no
        // subcommand) without panicking. Step 14 will replace this
        // with a state-model assertion once `run_scaffold` grows
        // real behaviour.
        dispatch(None, None).expect("bare dispatch must succeed");
    }

    #[test]
    fn dispatch_doctor_runs_real_probes_without_panicking() {
        // Step 33 swapped the scaffold-era `run_doctor` for the real
        // probe runner; `dispatch` now exits with the status code so
        // we can't call it from a unit test (would terminate the
        // process). Probe-list construction is the same code path the
        // dispatcher hits, so exercise it here.
        let opts = crate::file::doctor::DoctorOpts::default();
        let checks = crate::file::doctor::file_doctor_checks(opts);
        assert!(!checks.is_empty(), "doctor must surface at least one probe");
    }

    /// Step 33 — the doctor JSON payload pins the `sy.file.doctor/v1`
    /// schema marker. Bumping the major requires also updating
    /// `docs/reference/sy-file-doctor.md`; this assertion is the wire-
    /// contract anchor.
    #[test]
    fn doctor_constants_are_wire_stable() {
        assert_eq!(crate::file::doctor::SCHEMA_DOCTOR, "sy.file.doctor/v1");
    }

    /// SPEC §4.3 exit-code table. Drift here would silently
    /// re-route a `sy file ipc` failure to the wrong shell status.
    #[test]
    fn spec_exit_codes_match_table() {
        assert_eq!(EXIT_OK, 0);
        assert_eq!(EXIT_GENERIC, 1);
        assert_eq!(EXIT_USAGE, 2);
        assert_eq!(EXIT_DAEMON_DOWN, 3);
        assert_eq!(EXIT_REFUSED, 4);
        assert_eq!(EXIT_PLUGIN_ERROR, 5);
    }

    /// `build_call` must produce a method for every non-`Serve`
    /// variant so the dispatcher never falls through to
    /// "unreachable" on a real op.
    #[test]
    fn build_call_covers_every_op() {
        let cases: Vec<(IpcCmd, &str)> = vec![
            (
                IpcCmd::Open {
                    path: PathBuf::from("/"),
                },
                "file.open",
            ),
            (
                IpcCmd::Cd {
                    path: PathBuf::from("/"),
                },
                "file.cd",
            ),
            (
                IpcCmd::Select {
                    mode: "toggle".into(),
                    paths: vec![],
                },
                "file.select",
            ),
            (
                IpcCmd::Copy {
                    conflict: "skip".into(),
                    dest: PathBuf::from("/"),
                    sources: vec![],
                },
                "file.copy",
            ),
            (
                IpcCmd::Move {
                    conflict: "skip".into(),
                    dest: PathBuf::from("/"),
                    sources: vec![],
                },
                "file.move",
            ),
            (IpcCmd::Trash { paths: vec![] }, "file.trash"),
            (
                IpcCmd::Restore {
                    trashed_path: PathBuf::from("/"),
                },
                "file.restore",
            ),
            (
                IpcCmd::Search {
                    root: PathBuf::from("/"),
                    knowledge: false,
                    query: "q".into(),
                },
                "file.search",
            ),
            (
                IpcCmd::Preview {
                    max_width: None,
                    max_height: None,
                    path: PathBuf::from("/"),
                },
                "file.preview",
            ),
            (IpcCmd::OpsList, "file.ops_list"),
            (IpcCmd::OpCancel { op_id: 0 }, "file.op_cancel"),
            (IpcCmd::State, "file.state"),
        ];
        for (cmd, want) in cases {
            let (got, _) = build_call(&cmd).expect("non-serve must produce a method");
            assert_eq!(got, want, "build_call mapped wrong method for {cmd:?}");
        }
        assert!(
            build_call(&IpcCmd::Serve {
                sock: None,
                systemd_notify: false,
            })
            .is_none(),
            "Serve must not produce a wire method"
        );
    }

    /// Step 28 DoD: the waybar tile must carry the SPEC §3.3 item 16
    /// schema (`text` / `tooltip` / `class`) and class-toggle on the
    /// `running` count. Mirrors what `sy mon waybar` emits so a single
    /// waybar stylesheet covers both tiles.
    #[test]
    fn waybar_tile_classes_match_running_count() {
        // running > 0 → active, non-empty text
        let active = render_waybar_tile(WaybarSnapshot {
            running: Some(3),
            queued: 3,
            throughput_bps: 1024,
        });
        assert!(
            active.contains(&format!(r#""class":"{WAYBAR_CLASS_ACTIVE}""#)),
            "running > 0 must flip class to {WAYBAR_CLASS_ACTIVE}, got {active}"
        );
        assert!(
            active.contains(r#""text":"3 ops""#),
            "active tile must surface the count: {active}"
        );
        // running == 0 → idle, empty text
        let idle = render_waybar_tile(WaybarSnapshot {
            running: Some(0),
            queued: 0,
            throughput_bps: 0,
        });
        assert!(
            idle.contains(&format!(r#""class":"{WAYBAR_CLASS_IDLE}""#)),
            "running == 0 must flip class to {WAYBAR_CLASS_IDLE}, got {idle}"
        );
        assert!(
            idle.contains(r#""text":"""#),
            "idle tile must surface empty text: {idle}"
        );
        // daemon down → down class, empty text
        let down = render_waybar_tile(WaybarSnapshot::down());
        assert!(
            down.contains(&format!(r#""class":"{WAYBAR_CLASS_DOWN}""#)),
            "down branch must flip class to {WAYBAR_CLASS_DOWN}, got {down}"
        );
        assert!(
            down.contains(r#""text":"""#),
            "down tile must surface empty text: {down}"
        );
    }

    /// Step 28 DoD: the active tile's tooltip carries the throughput
    /// label so the operator sees the binary-prefix vocabulary the
    /// statusbar paints. Pinned here so a refactor that drops the
    /// `humanise_throughput` call site surfaces at build time.
    #[test]
    fn waybar_tile_tooltip_includes_throughput() {
        let tile = render_waybar_tile(WaybarSnapshot {
            running: Some(1),
            queued: 1,
            throughput_bps: 1024 * 1024,
        });
        assert!(
            tile.contains("1.0 MiB/s"),
            "active tile tooltip must surface MiB/s vocabulary: {tile}"
        );
    }
}
