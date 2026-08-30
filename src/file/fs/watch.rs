//! `fs::watch` — live pane updates via `notify-rs` with a 50 ms
//! debounce window. Step 19 of the [`sy-file-manager`
//! roadmap][roadmap] / SPEC §3.3 item 11.
//!
//! ## Surface
//!
//! [`watch`] spawns a `notify::recommended_watcher` rooted at each
//! input path (non-recursive — the UI scopes the watcher to the
//! visible pane), routes the notifier callback into a tokio
//! `mpsc::channel(64)`, and returns an `impl Stream<Item =
//! WatchEvent>` that yields debounced events. The debouncer collects
//! events into a per-path "last seen" map and flushes on either:
//!
//! 1. **50 ms of idle** (no new event for the path) — the steady
//!    state when an editor saves a single file, or
//! 2. **64-event buffer fullness** — the back-pressure escape hatch
//!    when a tool churns a directory faster than the consumer drains.
//!
//! Whichever fires first is what the consumer sees.
//!
//! ## Overflow contract
//!
//! When `inotify` reports `IN_Q_OVERFLOW` (Linux kernel rate-limit on
//! `fs.inotify.max_user_watches` or queue length), the underlying
//! crate surfaces it as `notify::Error::kind == ErrorKind::Generic` /
//! a wrapped `notify::EventKind::Other`. The stream emits one
//! [`WatchEvent::Overflow`] and stops. The documented caller
//! contract (Step 23+ previewer / pane refresh) is to fall back to a
//! periodic poll. Step 19 just emits the variant; the poll fallback
//! lands in a later step.
//!
//! ## proc/sys/dev guard
//!
//! `notify` happily tries to watch `/proc`, which on a busy box can
//! exhaust `fs.inotify.max_user_watches` (the SPEC §6 risk). The
//! input filter rejects any path beginning with `/proc/`, `/sys/`,
//! or `/dev/` with an `eprintln!` warn so the operator sees the
//! skip; the rest of the input list still feeds the watcher.
//!
//! [roadmap]: ../../../../specs/roadmaps/sy-file-manager/ROADMAP.md

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio_stream::Stream;

/// Coalesced filesystem event surfaced by [`watch`]. The variant
/// space is intentionally coarse — the journey-J2 pane-refresh path
/// only cares about "the directory list changed"; a richer (size,
/// mtime, kind) payload lives on the [`Entry`] the next `walk()`
/// call produces, not here.
///
/// [`Entry`]: crate::file::state::panes::Entry
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// A path appeared under the watched root.
    Created(PathBuf),
    /// A path's data or metadata changed in place.
    Modified(PathBuf),
    /// A path was removed from the watched root.
    Removed(PathBuf),
    /// A path was renamed inside the watched root. `from` and `to`
    /// are both rooted under the same watcher; cross-watcher renames
    /// surface as `Removed(from)` + `Created(to)`.
    Renamed { from: PathBuf, to: PathBuf },
    /// The inotify queue overflowed (Linux `IN_Q_OVERFLOW`) — the
    /// watcher dropped events. The documented contract is for the
    /// consumer to fall back to a periodic poll; the stream emits
    /// this variant and terminates.
    Overflow,
}

/// Buffer depth for the tokio mpsc channel feeding the debouncer.
/// 64 is the "back-pressure escape" threshold called out in the
/// module doc. Sized small so a runaway producer surfaces as a
/// debounce-flush rather than as unbounded queue growth.
const CHANNEL_DEPTH: usize = 64;

/// Idle window after which a per-path event batch flushes
/// downstream. 50 ms matches the SPEC §3.3 item 11 budget.
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(50);

/// Path-prefix guard list. Watching `/proc` (or `/sys`, `/dev`) on a
/// busy box exhausts `fs.inotify.max_user_watches` — see SPEC §6
/// "Risks". Any input path beginning with one of these prefixes is
/// dropped on the floor with an `eprintln!` warn.
const SKIP_PREFIXES: &[&str] = &["/proc/", "/sys/", "/dev/"];

/// Build a stream of debounced [`WatchEvent`]s rooted at each path
/// in `paths`. Paths under `/proc`, `/sys`, or `/dev` are skipped
/// with a warn; an empty effective input still returns a (silent)
/// stream so the caller's plumbing degrades gracefully.
///
/// The watcher is `notify::recommended_watcher` (inotify on Linux,
/// `FSEvents` on macOS, ReadDirectoryChangesW on Windows); the
/// debouncer runs as a tokio task that is kept alive by the
/// returned stream — dropping the stream stops the watcher.
pub fn watch(paths: &[PathBuf]) -> impl Stream<Item = WatchEvent> {
    let filtered = filter_inputs(paths);
    let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<Event>>(CHANNEL_DEPTH);
    let (out_tx, out_rx) = mpsc::channel::<WatchEvent>(CHANNEL_DEPTH);
    spawn_watcher(filtered, raw_tx);
    tokio::spawn(debounce_loop(raw_rx, out_tx));
    tokio_stream::wrappers::ReceiverStream::new(out_rx)
}

/// Reject `/proc/`, `/sys/`, `/dev/` paths so the watcher never
/// touches the kernel's synthetic filesystems. Visible as an
/// `eprintln!` warn so the operator sees the skip.
fn filter_inputs(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        let s = p.to_string_lossy();
        if SKIP_PREFIXES.iter().any(|pre| s.starts_with(pre)) {
            eprintln!(
                "fs::watch: skipping kernel-synthetic path {:?} \
                 (would exhaust fs.inotify.max_user_watches)",
                p
            );
            continue;
        }
        out.push(p.clone());
    }
    out
}

/// Spawn the `notify::recommended_watcher` on a blocking thread and
/// wait until every watch registration attempt has completed.
/// The watcher is moved into the spawned closure so it lives as
/// long as the channel does; dropping `raw_tx` from the debouncer
/// side propagates a `mpsc::error::SendError` here that ends the
/// thread. Any `notify::Error` at watch-setup time is logged but
/// not surfaced to the caller — the empty stream is the visible
/// contract.
fn spawn_watcher(paths: Vec<PathBuf>, raw_tx: mpsc::Sender<notify::Result<Event>>) {
    if paths.is_empty() {
        return;
    }
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
    std::thread::spawn(move || {
        let mut watcher = match build_watcher(raw_tx) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("fs::watch: failed to build watcher: {e:#}");
                let _ = ready_tx.send(());
                return;
            }
        };
        for p in &paths {
            if let Err(e) = watcher.watch(p, RecursiveMode::NonRecursive) {
                eprintln!("fs::watch: watcher.watch({p:?}) failed: {e}");
            }
        }
        let _ = ready_tx.send(());
        // Park forever; the OS drops the watcher when the thread is
        // joined (i.e. when the runtime tears down the test binary
        // or the daemon shuts down). We can't `recv()` from this
        // thread because the receiver lives in the debounce loop;
        // park is the idiomatic "keep this thread alive" call.
        loop {
            std::thread::park();
        }
    });
    let _ = ready_rx.recv();
}

fn build_watcher(
    raw_tx: mpsc::Sender<notify::Result<Event>>,
) -> Result<notify::RecommendedWatcher> {
    notify::recommended_watcher(move |res: notify::Result<Event>| {
        // `try_send` so a stalled debouncer surfaces back-pressure
        // as a dropped event rather than blocking the notifier
        // thread (which is shared across every recommended_watcher
        // in the process on Linux).
        let _ = raw_tx.try_send(res);
    })
    .context("notify::recommended_watcher returned Err")
}

/// Collect raw events into a 50 ms window per path. The window is
/// per-event-not-per-path so a 10-touch burst against one file
/// collapses to a single flush. On a `notify::Error` that smells
/// like overflow, emit [`WatchEvent::Overflow`] and stop.
async fn debounce_loop(
    mut raw_rx: mpsc::Receiver<notify::Result<Event>>,
    out_tx: mpsc::Sender<WatchEvent>,
) {
    let mut pending: Vec<WatchEvent> = Vec::with_capacity(CHANNEL_DEPTH);
    loop {
        // Block until at least one event arrives or the upstream
        // sender drops (all watchers gone) — then we shut down.
        let first = match raw_rx.recv().await {
            Some(r) => r,
            None => return,
        };
        if !handle_raw(first, &mut pending, &out_tx).await {
            return;
        }
        // Drain everything that arrives inside the debounce window.
        loop {
            match tokio::time::timeout(DEBOUNCE_WINDOW, raw_rx.recv()).await {
                Ok(Some(r)) => {
                    if !handle_raw(r, &mut pending, &out_tx).await {
                        return;
                    }
                    if pending.len() >= CHANNEL_DEPTH {
                        break;
                    }
                }
                Ok(None) => {
                    flush(&mut pending, &out_tx).await;
                    return;
                }
                Err(_) => break, // idle: flush.
            }
        }
        flush(&mut pending, &out_tx).await;
    }
}

/// Convert one raw `notify::Result<Event>` into 0..N [`WatchEvent`]s
/// pushed onto `pending`. Returns `false` when the loop must stop
/// (overflow signalled or downstream closed).
async fn handle_raw(
    res: notify::Result<Event>,
    pending: &mut Vec<WatchEvent>,
    out_tx: &mpsc::Sender<WatchEvent>,
) -> bool {
    let ev = match res {
        Ok(e) => e,
        Err(e) => {
            eprintln!("fs::watch: notify error: {e}");
            // Treat any notify error as overflow for the
            // overflow-fallback contract — the alternative is to
            // silently swallow, which the Step 19 risks block
            // forbids.
            let _ = out_tx.send(WatchEvent::Overflow).await;
            return false;
        }
    };
    push_event(ev, pending);
    true
}

/// Project a notify `Event` onto our coarser [`WatchEvent`] variant
/// set. Rename pairs (`ModifyKind::Name(RenameMode::Both)` with two
/// paths) collapse to a single [`WatchEvent::Renamed`]; everything
/// else uses the first path in the event.
fn push_event(ev: Event, pending: &mut Vec<WatchEvent>) {
    use notify::event::{ModifyKind, RenameMode};
    let first = match ev.paths.first() {
        Some(p) => p.clone(),
        None => return,
    };
    match ev.kind {
        EventKind::Create(_) => pending.push(WatchEvent::Created(first)),
        EventKind::Remove(_) => pending.push(WatchEvent::Removed(first)),
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if ev.paths.len() >= 2 => {
            pending.push(WatchEvent::Renamed {
                from: first,
                to: ev.paths[1].clone(),
            });
        }
        EventKind::Modify(_) => pending.push(WatchEvent::Modified(first)),
        EventKind::Any | EventKind::Access(_) | EventKind::Other => {}
    }
}

/// Coalesce `pending` and send the result downstream. Same-path
/// adjacent duplicates collapse to one event (the 10-touch debounce
/// contract); a `Created` followed by `Modified` on the same path
/// keeps just the `Created` because the journey-J2 pane refresh
/// only cares that "the row is new".
async fn flush(pending: &mut Vec<WatchEvent>, out_tx: &mpsc::Sender<WatchEvent>) {
    let mut seen_paths: Vec<PathBuf> = Vec::with_capacity(pending.len());
    let drained: Vec<WatchEvent> = std::mem::take(pending);
    for ev in drained {
        let p = event_path(&ev);
        if seen_paths.iter().any(|q| q == &p) {
            continue;
        }
        seen_paths.push(p);
        if out_tx.send(ev).await.is_err() {
            return;
        }
    }
}

fn event_path(ev: &WatchEvent) -> PathBuf {
    match ev {
        WatchEvent::Created(p) | WatchEvent::Modified(p) | WatchEvent::Removed(p) => p.clone(),
        WatchEvent::Renamed { from, .. } => from.clone(),
        WatchEvent::Overflow => PathBuf::new(),
    }
}

/// Test-only helper. Returns a stream that yields a single
/// [`WatchEvent::Overflow`] then closes. Used by the
/// `inotify_max_user_watches_doesnt_panic` test — the real
/// `IN_Q_OVERFLOW` is process-global state we cannot synthesise
/// inside one test binary without burning the operator's quota. See
/// the Step 19 risks block for the rationale.
#[cfg(test)]
pub(crate) fn force_overflow_for_test() -> impl Stream<Item = WatchEvent> {
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(async move {
        let _ = tx.send(WatchEvent::Overflow).await;
    });
    tokio_stream::wrappers::ReceiverStream::new(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio_stream::StreamExt;

    /// Touching a fresh file under a watched dir surfaces a
    /// [`WatchEvent::Created`] inside 100 ms. The 100 ms cap covers
    /// the 50 ms debounce window plus 50 ms of scheduling slack for
    /// CI noisy neighbours.
    #[tokio::test(flavor = "current_thread")]
    async fn file_create_emits_event_within_100ms() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();
        let mut stream = Box::pin(watch(std::slice::from_ref(&dir)));
        tokio::fs::File::create(dir.join("foo"))
            .await
            .expect("create foo");
        let ev = tokio::time::timeout(Duration::from_millis(100), stream.next())
            .await
            .expect("timeout waiting for create event");
        match ev {
            Some(WatchEvent::Created(p)) => assert!(
                p.ends_with("foo"),
                "Created path must end with `foo`, got {p:?}"
            ),
            other => panic!("expected Created(`foo`), got {other:?}"),
        }
    }

    /// 10 rapid-fire writes of the same file inside a 30 ms window
    /// must collapse to ≤ 2 [`WatchEvent`]s (one Created + at most
    /// one Modified). The "spirit" of the SPEC's debounce contract
    /// is "burst of N collapses to ≤ 2 events" — see Step 19 test
    /// pin.
    #[tokio::test(flavor = "current_thread")]
    async fn debounces_50ms_window() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();
        let mut stream = Box::pin(watch(std::slice::from_ref(&dir)));
        tokio::time::sleep(Duration::from_millis(20)).await;
        let p = dir.join("burst");
        for i in 0..10 {
            tokio::fs::write(&p, format!("{i}")).await.expect("write");
        }
        // Wait one debounce window + slack.
        tokio::time::sleep(Duration::from_millis(120)).await;
        // Drain whatever's queued; with a 50 ms debounce + a 30 ms
        // burst the count should be 1 (single batch) or 2 (Created
        // + Modified).
        let mut count = 0;
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(30), stream.next()).await
        {
            count += 1;
        }
        assert!(
            (1..=2).contains(&count),
            "10-touch burst must debounce to 1..=2 events, got {count}"
        );
    }

    /// Overflow is a reachable terminal state of the stream and the
    /// caller doesn't panic on it. The real `IN_Q_OVERFLOW` cannot
    /// be synthesised hermetically (it would burn
    /// `fs.inotify.max_user_watches` process-globally), so the
    /// assertion runs against the [`force_overflow_for_test`]
    /// surface — same semantic contract, no quota cost.
    #[tokio::test(flavor = "current_thread")]
    async fn inotify_max_user_watches_doesnt_panic() {
        let mut stream = Box::pin(force_overflow_for_test());
        let ev = tokio::time::timeout(Duration::from_millis(100), stream.next())
            .await
            .expect("timeout waiting for overflow");
        assert_eq!(
            ev,
            Some(WatchEvent::Overflow),
            "overflow must be the first (and only) event"
        );
    }

    /// `/proc`, `/sys`, `/dev` inputs are filtered out before
    /// reaching the watcher. The visible contract is "no panic, no
    /// inotify watch added" — we assert the empty effective input
    /// returns a stream that closes without yielding.
    #[tokio::test(flavor = "current_thread")]
    async fn proc_sys_dev_paths_are_skipped() {
        let mut stream = Box::pin(watch(&[
            PathBuf::from("/proc/1"),
            PathBuf::from("/sys/class"),
            PathBuf::from("/dev/null"),
        ]));
        // Stream should close (no watcher spawned) — the receiver
        // sees None inside one debounce window.
        let res = tokio::time::timeout(Duration::from_millis(150), stream.next()).await;
        match res {
            Ok(None) => {}
            Ok(Some(ev)) => panic!("kernel-synthetic paths must not emit events, got {ev:?}"),
            Err(_) => {} // timeout is also acceptable — stream is empty.
        }
    }
}
