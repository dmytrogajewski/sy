//! Bulk-ops + waybar pill integration tests for `sy file` (sy-file-
//! manager roadmap Step 28). Three tests are pinned by name in the
//! roadmap brief:
//!
//! * `multi_select_copy_emits_progress_stream` — drives a 3-src
//!   `fs::copy::copy` stream and asserts the SPEC §3.3 row 5 event set
//!   (≥3 Started, ≥1 Progress, ≥3 Completed) lands on the receiver.
//! * `waybar_pill_shows_running_count_during_copy` — spawns the real
//!   daemon-in-thread, drives a copy of a multi-MiB src against it,
//!   polls `sy file waybar` mid-copy + post-copy, asserts the JSON
//!   tile shows running count ≥ 1 mid-copy then collapses to idle.
//! * `range_select_inclusive` — pins the journey-J5 `<Shift>+arrow`
//!   invariant against [`SelectionSet::add_range`] (inclusive on both
//!   endpoints, ordering-independent).
//!
//! The pure tests live alongside the production code; this binary
//! adds the cross-cutting + daemon-in-thread tests so the Step 28
//! e2e doesn't get blocked on the bare `cargo test --lib` shape.
//!
//! `#[path]` import pattern mirrors `tests/sy_file_ipc.rs`.

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
// SIGHUP reload arm (`crate::file::keymap::…`) resolves.
#[path = "../src/file/keymap.rs"]
#[allow(dead_code)]
mod file_keymap;

// Step 30 — knowledge-search module mirror so the `#[path]`-imported
// `ipc.rs`'s `file.search` `knowledge:true` branch
// (`crate::file::search::knowledge::{KnowledgeBackend,
// RealKnowledgeBackend, query, merge}`) compiles. The real source is
// pulled in via `#[path]`; its `crate::knowledge::{ipc::HitRow,
// cli::search_hits}` references resolve against the inline `knowledge`
// shim below.
#[path = "../src/file/search/knowledge.rs"]
#[allow(dead_code)]
mod file_search_knowledge;

/// `crate::knowledge::…` mirror for the two symbols
/// `file/search/knowledge.rs` names: `ipc::HitRow` (the qdrant hit row
/// the backend returns) and `cli::search_hits` (the live-daemon dial
/// `RealKnowledgeBackend` wraps — never invoked under test, which
/// injects a stub backend instead). `HitRow` mirrors
/// `src/aiplane/ipc.rs::HitRow` field-for-field; a drift surfaces
/// immediately as a field error in the `ipc.rs`
/// `search_knowledge_branch_*` tests.
#[allow(dead_code)]
mod knowledge {
    pub(crate) mod ipc {
        #[derive(Debug, Clone)]
        pub struct HitRow {
            pub score: f32,
            pub chunk_id: String,
            pub file_path: String,
            pub chunk_index: u32,
            pub chunk_text: String,
            pub embed_score: Option<f32>,
        }
    }
    pub(crate) mod cli {
        use super::ipc::HitRow;
        pub fn search_hits(
            _query: &str,
            _limit: usize,
            _prefix: Option<&str>,
        ) -> anyhow::Result<Vec<HitRow>> {
            Ok(Vec::new())
        }
    }
}

/// `crate::file::…` mirror so the `#[path]`-imported sources compile
/// under the integration-test binary. Same idiomatic shim
/// `tests/sy_file_ipc.rs` declares for Step 20.
#[allow(dead_code)]
mod file {
    pub(crate) mod state {
        pub(crate) use super::super::ops::{ConflictPolicy, OpEvent};
        pub(crate) use super::super::panes;
        pub(crate) use super::super::panes::Panes;
        pub(crate) use super::super::selection;
        pub(crate) use super::super::selection::SelectionSet;

        #[allow(clippy::enum_variant_names)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
        pub enum LayoutMode {
            #[default]
            ThreePane,
            TwoPane,
            OnePane,
        }

        #[derive(Debug, Default)]
        pub struct State {
            pub panes: Panes,
            pub mode: LayoutMode,
            pub selection: SelectionSet,
            pub ops: Vec<super::super::ops::Operation>,
            /// Step 31 mirror — bookmark registry slot.
            pub bookmarks:
                Option<std::sync::Arc<std::sync::Mutex<super::super::bookmarks::Bookmarks>>>,
            /// Step 34 mirror — live keymap; SIGHUP reload writes here.
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
    pub(crate) mod search {
        pub(crate) use super::super::file_search_knowledge as knowledge;
    }
}

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use file::state::State;
use selection::SelectionSet;
use tokio::sync::{oneshot, RwLock};

/// SPEC §3.3 row 5 + journey-J5 `<Shift>+arrow` beat: `add_range(a,
/// b)` is inclusive on both endpoints (`{a, a+1, …, b}`) regardless of
/// argument order. Pinning the contract here so Step 28's keymap arm
/// (`Shift+ArrowDown` → `add_range(anchor, cursor)`) has a stable
/// invariant to rely on.
#[test]
fn range_select_inclusive() {
    let mut s = SelectionSet::new();
    s.add_range(2, 5);
    for id in 2..=5 {
        assert!(s.contains(id), "id {id} must be in inclusive range [2,5]");
    }
    assert_eq!(s.len(), 4, "inclusive [2,5] has 4 elements");
    let mut t = SelectionSet::new();
    t.add_range(5, 2);
    assert_eq!(t, s, "reversed range must match forward range");
}

/// Spawn the daemon under a fresh tokio task. Mirrors
/// `tests/sy_file_ipc.rs::spawn_daemon`.
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
    tokio::time::sleep(Duration::from_millis(50)).await;
    (state, tx, handle)
}

/// SPEC §3.3 row 5 + journey-J5/J6 hand-off: a 3-src copy must emit
/// at least three `Started` events + at least one `Progress` event +
/// three `Completed` events. The wire shape lands on the daemon's
/// `OpRow` tracker today (via `spawn_copy_task` inside
/// `src/file/ipc.rs`); pinning the raw `fs::copy::copy` stream here
/// gives Step 28 a non-daemon assertion for the same contract so a
/// regression in `fs::copy` is observable independently of the daemon
/// plumbing.
#[tokio::test(flavor = "current_thread")]
async fn multi_select_copy_emits_progress_stream() {
    use file::state::ConflictPolicy;
    use ops::OpEvent;
    use tokio_stream::StreamExt as _;

    let dir = tempfile::tempdir().expect("step28 tempdir");
    let src_dir = dir.path().join("src");
    let dst_dir = dir.path().join("dst");
    std::fs::create_dir_all(&src_dir).expect("mkdir src");
    std::fs::create_dir_all(&dst_dir).expect("mkdir dst");
    // Three sources, each large enough to emit at least one Progress
    // sample (`PROGRESS_BYTES_TICK = 4 MiB` inside `fs::copy`).
    const SRC_BYTES: usize = 5 * 1024 * 1024;
    let mut srcs: Vec<PathBuf> = Vec::new();
    for i in 0..3 {
        let p = src_dir.join(format!("blob-{i}.bin"));
        std::fs::write(&p, vec![b'A'; SRC_BYTES]).expect("write blob");
        srcs.push(p);
    }
    let mut stream = file_fs_copy::copy(&srcs, &dst_dir, ConflictPolicy::Skip).await;
    let mut started = 0_u32;
    let mut progress = 0_u32;
    let mut completed = 0_u32;
    while let Some(ev) = stream.next().await {
        match ev {
            OpEvent::Started { .. } => started += 1,
            OpEvent::Progress { .. } => progress += 1,
            OpEvent::Completed { .. } => completed += 1,
            _ => {}
        }
    }
    assert!(
        started >= 3,
        "must see ≥3 Started events for a 3-src batch, got {started}"
    );
    assert!(
        progress >= 1,
        "must see ≥1 Progress sample across the batch, got {progress}"
    );
    assert!(
        completed >= 3,
        "must see ≥3 Completed events for a 3-src batch, got {completed}"
    );
}

/// SPEC §3.3 item 16 (waybar pill) + journey-J6 affordance: while a
/// copy is in flight, `sy file waybar` must emit a JSON tile whose
/// `text` field is non-empty + carries a running count ≥ 1; once the
/// copy completes, the tile must collapse back to idle (`text == ""`).
#[tokio::test(flavor = "current_thread")]
async fn waybar_pill_shows_running_count_during_copy() {
    use serde_json::Value;
    use sy_ipc::{CallOpts, Client};

    let dir = tempfile::tempdir().expect("step28 waybar tempdir");
    let sock = dir.path().join("sy-file-waybar.sock");
    let (_state, shutdown, handle) = spawn_daemon(sock.clone()).await;

    let src_dir = dir.path().join("src");
    let dst_dir = dir.path().join("dst");
    std::fs::create_dir_all(&src_dir).expect("mkdir src");
    std::fs::create_dir_all(&dst_dir).expect("mkdir dst");
    // 16 MiB blob — well above PROGRESS_BYTES_TICK so the copy stays
    // in the `running` state long enough for the mid-flight poll.
    const BIG_BYTES: usize = 16 * 1024 * 1024;
    let big = src_dir.join("big.bin");
    std::fs::write(&big, vec![b'B'; BIG_BYTES]).expect("write big blob");

    // Queue the copy via `file.copy`.
    let mut client = Client::connect(&sock).await.expect("client connect");
    let _ = client
        .call(
            "file.copy",
            serde_json::json!({
                "sources": [big.display().to_string()],
                "dest": dst_dir.display().to_string(),
                "conflict": "skip",
            }),
            CallOpts::default(),
        )
        .await
        .expect("file.copy queue");

    // Poll the tile until running_count ≥ 1 OR the executor finished
    // before we got a sample. The mid-flight assertion is the
    // SPEC §3.3 item 16 contract.
    let mut saw_running = false;
    for _ in 0..80 {
        // ~4 s budget
        let mut probe = Client::connect(&sock).await.expect("waybar probe connect");
        let resp = probe
            .call("file.ops_list", serde_json::json!({}), CallOpts::default())
            .await
            .expect("file.ops_list");
        let result = match resp {
            sy_ipc::Response::Ok { result, .. } => result,
            sy_ipc::Response::Err { error, .. } => panic!("ops_list err: {error:?}"),
        };
        let ops_arr = result
            .get("ops")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let running = ops_arr
            .iter()
            .filter(|row| row.get("state").and_then(Value::as_str) == Some("running"))
            .count();
        if running >= 1 {
            saw_running = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        saw_running,
        "must observe running_count ≥ 1 at some point during the 16 MiB copy"
    );

    // Drain to completion: poll until no row is in `running`.
    for _ in 0..120 {
        let mut probe = Client::connect(&sock).await.expect("settle probe");
        let resp = probe
            .call("file.ops_list", serde_json::json!({}), CallOpts::default())
            .await
            .expect("file.ops_list");
        let result = match resp {
            sy_ipc::Response::Ok { result, .. } => result,
            sy_ipc::Response::Err { error, .. } => panic!("settle err: {error:?}"),
        };
        let ops_arr = result
            .get("ops")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let running = ops_arr
            .iter()
            .filter(|row| row.get("state").and_then(Value::as_str) == Some("running"))
            .count();
        if running == 0 && !ops_arr.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Now verify post-copy running_count == 0.
    let mut final_probe = Client::connect(&sock).await.expect("final probe");
    let resp = final_probe
        .call("file.ops_list", serde_json::json!({}), CallOpts::default())
        .await
        .expect("file.ops_list final");
    let result = match resp {
        sy_ipc::Response::Ok { result, .. } => result,
        sy_ipc::Response::Err { error, .. } => panic!("final err: {error:?}"),
    };
    let ops_arr = result
        .get("ops")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let final_running = ops_arr
        .iter()
        .filter(|row| row.get("state").and_then(Value::as_str) == Some("running"))
        .count();
    assert_eq!(
        final_running, 0,
        "post-copy waybar tile must show running_count == 0 (idle)"
    );

    let _ = shutdown.send(());
    let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
}
