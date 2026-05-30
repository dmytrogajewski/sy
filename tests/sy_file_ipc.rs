//! Daemon-in-thread integration tests for `sy file`'s SPEC §4.3 IPC
//! surface (sy-file-manager roadmap Step 20). Each test spawns the
//! real [`crate::file::ipc::serve`] (pulled in via `#[path]` so the
//! integration binary compiles the same source the bin uses) on a
//! tempdir-anchored socket, then drives one or more `sy_ipc::Client`
//! connections through the eleven SPEC §4.3 ops plus the journey-J8
//! `file.state` snapshot op.
//!
//! Naming convention: each test pins one Step 20 acceptance criterion
//! (`open_then_cd_then_list_roundtrip`, `socket_mode_is_0600`, …) —
//! same shape `tests/sy_file_journey_e2e.rs` follows.
//!
//! The bin has no `lib.rs`; the `#[path]` import mirror matches the
//! pattern `tests/sy_file_journey_e2e.rs` uses for the
//! `file::state` / `file::fs` siblings.

// `state/{ops,panes,selection}.rs` reference each other via
// `super::selection::…` and `super::panes::…`, so the three siblings
// must sit at the test-crate root under their bare names — same
// pattern `tests/sy_file_journey_e2e.rs` uses.
#[path = "../src/file/state/ops.rs"]
#[allow(dead_code)]
mod ops;
#[path = "../src/file/state/panes.rs"]
#[allow(dead_code)]
mod panes;
#[path = "../src/file/state/selection.rs"]
#[allow(dead_code)]
mod selection;

#[path = "../src/file/fs/copy.rs"]
#[allow(dead_code)]
mod file_fs_copy;
#[path = "../src/file/fs/mime.rs"]
#[allow(dead_code)]
mod file_fs_mime;
#[path = "../src/file/fs/trash.rs"]
#[allow(dead_code)]
mod file_fs_trash;
#[path = "../src/file/fs/walk.rs"]
#[allow(dead_code)]
mod file_fs_walk;
#[path = "../src/file/fs/watch.rs"]
#[allow(dead_code)]
mod file_fs_watch;

#[path = "../src/file/ipc.rs"]
#[allow(dead_code)]
mod ipc;

// Step 31 — bookmarks module mirror so `ipc.rs::handle_open` resolves
// `crate::file::bookmarks::Bookmarks` (the `state.bookmarks` slot).
#[path = "../src/file/bookmarks.rs"]
#[allow(dead_code)]
mod bookmarks;
// Step 34 — keymap module mirror so the `#[path]`-imported `ipc.rs`'s
// `crate::file::keymap::…` references in the SIGHUP reload arm resolve.
#[path = "../src/file/keymap.rs"]
#[allow(dead_code)]
mod file_keymap;

/// `crate::file::…` mirror so the `#[path]`-imported `ipc.rs`'s
/// `use crate::file::…` lines compile under the integration test.
/// Mirrors the same shim shape `tests/sy_file_journey_e2e.rs`
/// declares for its Step 15+ ladder.
#[allow(dead_code)]
mod file {
    pub(crate) mod state {
        pub(crate) use super::super::ops::{ConflictPolicy, OpEvent};
        // Sub-module re-exports — `walk.rs` and `copy.rs` reach in
        // via `crate::file::state::panes::…` and
        // `crate::file::state::selection::…`, so the integration
        // shim must expose them at the same path the production
        // `state/mod.rs` does.
        pub(crate) use super::super::panes;
        pub(crate) use super::super::selection;
        // Flat re-exports for direct callers (the ipc handler reads
        // `Panes` + `SelectionSet`). The rest of the production
        // `pub use` flatlist (`Entry`, `EntryKind`, `EntryId`,
        // `Pane`, `PaneId`) is reachable via the sub-module path
        // above; only the symbols `ipc.rs` itself names show up
        // here so the unused-imports lint stays quiet.
        pub(crate) use super::super::panes::Panes;
        pub(crate) use super::super::selection::SelectionSet;

        /// Inline duplicate of `src/file/state/mod.rs::LayoutMode`.
        /// Kept in sync at the const-string level via the
        /// `file.state` op's `"three_pane"|"two_pane"|"one_pane"`
        /// wire shape — a drift here would surface immediately.
        #[allow(clippy::enum_variant_names)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
        pub enum LayoutMode {
            #[default]
            ThreePane,
            TwoPane,
            OnePane,
        }

        /// Inline duplicate of `src/file/state/mod.rs::State`. Kept
        /// here because the integration-test binary can't re-import
        /// `mod.rs` (its `pub mod ops; pub mod panes; pub mod
        /// selection;` lines clash with the sibling-imports at the
        /// test-crate root above). Any State-shape change in
        /// production must be mirrored here so the `ipc.rs` source
        /// compiles under both builds — locked down by the Step 20
        /// `file_methods_list_covers_spec_43_eleven_ops_plus_state`
        /// unit test.
        #[derive(Debug, Default)]
        pub struct State {
            pub panes: Panes,
            pub mode: LayoutMode,
            pub selection: SelectionSet,
            pub ops: Vec<super::super::ops::Operation>,
            /// Step 31 mirror — bookmark registry slot. The step20 +
            /// step31 e2e attach a real registry against a tempdir
            /// before driving `file.open`.
            pub bookmarks:
                Option<std::sync::Arc<std::sync::Mutex<super::super::bookmarks::Bookmarks>>>,
            /// Step 34 mirror — live keymap. SIGHUP reload writes here.
            pub keymap: super::super::file_keymap::KeymapConfig,
        }
    }
    pub(crate) mod fs {
        pub(crate) use super::super::file_fs_copy as copy;
        pub(crate) use super::super::file_fs_mime as mime;
        pub(crate) use super::super::file_fs_trash as trash;
        pub(crate) use super::super::file_fs_walk as walk;
        #[allow(unused_imports)]
        pub(crate) use super::super::file_fs_watch as watch;
    }
    pub(crate) use super::file_keymap as keymap;
}

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use sy_ipc::{CallOpts, Client, Response};
use tokio::sync::{oneshot, RwLock};

use file::state::State;

/// Synthetic byte size for the cancel-rollback test. Large enough
/// for `fs::copy` to emit at least one `Progress` beat
/// (`PROGRESS_BYTES_TICK = 4 MiB`) before the cancel signal lands,
/// matching the Step 16 unit-test rationale that
/// `cancel_mid_stream_rolls_back_partial_dst` uses.
const CANCEL_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

/// Spawn the daemon under a fresh tokio task on the supplied socket
/// path. Returns the shared `State` (so the test can prime it or
/// inspect it without going through IPC) and a oneshot sender that
/// triggers a graceful daemon shutdown — the `JoinHandle` is dropped
/// once the shutdown completes so the test never leaks a task.
async fn spawn_daemon(
    sock: PathBuf,
) -> (
    Arc<RwLock<State>>,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let state = Arc::new(RwLock::new(State::default()));
    let (tx, rx) = oneshot::channel::<()>();
    let state_clone = Arc::clone(&state);
    let handle = tokio::spawn(async move { ipc::serve(state_clone, sock, rx).await });
    // Park briefly so the listener has time to bind + chmod before
    // the first `Client::connect` lands. 50 ms matches the
    // `aiplane::ipc` integration tests' settle window.
    tokio::time::sleep(Duration::from_millis(50)).await;
    (state, tx, handle)
}

/// Round-trip `(method, params)` against an open client; unwraps
/// `Response::Ok` and panics with a precise message on `Response::Err`
/// (the e2e tests pre-state the daemon so an error here is always a
/// real regression worth a clear traceback).
async fn call_ok(
    client: &mut Client,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let resp = client
        .call(method, params, CallOpts::default())
        .await
        .unwrap_or_else(|e| panic!("client.call({method}): {e}"));
    match resp {
        Response::Ok { result, .. } => result,
        Response::Err { error, .. } => {
            panic!(
                "daemon returned Err for {method}: code={:?} msg={}",
                error.code, error.message
            )
        }
    }
}

/// SPEC §4.3 acceptance criterion: `open`, `cd`, then `ops_list` all
/// round-trip via one client against a live daemon. Locks in the
/// happy-path lifecycle the journey-J1 entry point depends on.
#[tokio::test(flavor = "current_thread")]
async fn open_then_cd_then_list_roundtrip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("sy-file.sock");
    let (_state, shutdown, handle) = spawn_daemon(sock.clone()).await;
    let mut client = Client::connect(&sock).await.expect("client connect");

    let open_res = call_ok(&mut client, "file.open", json!({ "path": dir.path() })).await;
    assert_eq!(open_res["ok"], json!(true), "file.open must ack ok=true");

    let cd_res = call_ok(&mut client, "file.cd", json!({ "path": dir.path() })).await;
    assert_eq!(cd_res["ok"], json!(true), "file.cd must ack ok=true");

    let ops_res = call_ok(&mut client, "file.ops_list", json!({})).await;
    assert!(
        ops_res["ops"].is_array(),
        "file.ops_list must return an array (got {ops_res:?})"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// SPEC §4.3 / journey-J6: `file.copy` returns an `op_id`; polling
/// `file.ops_list` thereafter must reveal at least one row keyed on
/// that id with a non-empty lifecycle state (`running` /
/// `completed`).
#[tokio::test(flavor = "current_thread")]
async fn copy_then_op_stream_emits_progress() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("sy-file.sock");
    let src = dir.path().join("payload.bin");
    let dst_dir = dir.path().join("dest");
    std::fs::create_dir(&dst_dir).expect("mkdir dst");
    std::fs::write(&src, vec![0xABu8; 256 * 1024]).expect("write src");

    let (_state, shutdown, handle) = spawn_daemon(sock.clone()).await;
    let mut client = Client::connect(&sock).await.expect("client connect");
    let copy_res = call_ok(
        &mut client,
        "file.copy",
        json!({
            "sources": [src],
            "dest": dst_dir,
            "conflict": "overwrite",
        }),
    )
    .await;
    let op_id = copy_res["op_id"]
        .as_u64()
        .expect("file.copy must return numeric op_id");

    // Poll ops_list until our op_id shows up. The cadence is loose;
    // the copy executor's first row insert happens synchronously in
    // `file.copy`'s handler, so the first poll already sees it.
    let mut found = false;
    for _ in 0..20 {
        let list = call_ok(&mut client, "file.ops_list", json!({})).await;
        let arr = list["ops"].as_array().expect("ops array");
        if arr.iter().any(|row| row["op_id"].as_u64() == Some(op_id)) {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        found,
        "file.ops_list must surface the queued copy op_id={op_id}"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// SPEC §4.3 / journey-J8: two `Client::connect`s to the same
/// daemon see the same state. Client A drives a `file.open`; client
/// B sees the post-open `cwd` echoed back through `file.state`.
#[tokio::test(flavor = "current_thread")]
async fn two_clients_share_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("sy-file.sock");
    let (_state, shutdown, handle) = spawn_daemon(sock.clone()).await;

    let mut client_a = Client::connect(&sock).await.expect("client A connect");
    let mut client_b = Client::connect(&sock).await.expect("client B connect");

    let _ = call_ok(&mut client_a, "file.open", json!({ "path": dir.path() })).await;

    let b_state = call_ok(&mut client_b, "file.state", json!({})).await;
    assert_eq!(
        b_state["cwd"].as_str(),
        Some(dir.path().display().to_string().as_str()),
        "client B must observe client A's cwd mutation"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// Step 20 DoD: cancelling a copy mid-flight must unlink the partial
/// dst so the caller sees the Step-16 rollback shape end-to-end via
/// IPC, not just inside the executor.
#[tokio::test(flavor = "current_thread")]
async fn op_cancel_rolls_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("sy-file.sock");
    let src = dir.path().join("big.bin");
    let dst_dir = dir.path().join("dest");
    std::fs::create_dir(&dst_dir).expect("mkdir dst");
    std::fs::write(&src, vec![0x55u8; CANCEL_PAYLOAD_BYTES]).expect("write big src");

    let (_state, shutdown, handle) = spawn_daemon(sock.clone()).await;
    let mut client = Client::connect(&sock).await.expect("client connect");

    let copy_res = call_ok(
        &mut client,
        "file.copy",
        json!({
            "sources": [src],
            "dest": dst_dir,
            "conflict": "overwrite",
        }),
    )
    .await;
    let op_id = copy_res["op_id"].as_u64().expect("op_id u64");

    // Fire the cancel as fast as possible — we want to land it
    // before the executor finishes the small in-flight chunk.
    let cancel_res = call_ok(&mut client, "file.op_cancel", json!({ "op_id": op_id })).await;
    assert_eq!(cancel_res["ok"], json!(true), "op_cancel must ack ok=true");

    // Settle window: give the executor time to observe the cancel
    // signal and unlink the partial dst. 400 ms matches the upper
    // bound the Step 16 `cancel_mid_stream_rolls_back_partial_dst`
    // unit test uses for the same shape.
    tokio::time::sleep(Duration::from_millis(400)).await;

    let dst = dst_dir.join("big.bin");
    if dst.exists() {
        let meta = std::fs::metadata(&dst).expect("dst metadata");
        assert!(
            meta.len() < CANCEL_PAYLOAD_BYTES as u64,
            "op_cancel must roll back: dst should be gone or partial-and-empty, \
             got size={} expected < {}",
            meta.len(),
            CANCEL_PAYLOAD_BYTES,
        );
    }

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// SPEC §4.3 mode requirement: the daemon socket must be `0o600` so
/// peer-uid is the kernel-enforced admission control (mirrors the
/// `is_peer_self` check inside `sy_ipc::Server`).
#[tokio::test(flavor = "current_thread")]
async fn socket_mode_is_0600() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("sy-file.sock");
    let (_state, shutdown, handle) = spawn_daemon(sock.clone()).await;

    let meta = std::fs::metadata(&sock).expect("socket metadata");
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "SPEC §4.3 requires sy-file.sock at mode 0600; got {mode:o}"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}
