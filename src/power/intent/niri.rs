//! niri compositor IPC subscriber — emits
//! [`IntentEvent::FocusedAppChanged`] when the user's focused window
//! changes. **Strips the raw window title at the parser** per SPEC
//! §4 "Privacy": only `app_id` ever crosses the snapshot boundary.
//!
//! Mechanism (niri ≥ 26.04, IPC schema validated against the local
//! `niri msg --json event-stream`):
//! 1. Connect to the niri JSON-line socket at
//!    `$XDG_RUNTIME_DIR/niri.wayland-*.sock` (path also exposed via
//!    `$NIRI_SOCKET`).
//! 2. Send `"EventStream"` as a single JSON line — niri responds with
//!    a one-shot reply followed by a stream of event frames, one
//!    JSON object per line.
//! 3. For each line, run [`parse_event`] (a pure function) and push
//!    the resulting `IntentEvent` into a `Mutex<Option<…>>` slot
//!    drained by `poll()`.
//!
//! The parser is deliberately written as a free function so the
//! tests can exercise it against fixture lines without spinning up
//! niri or any socket. The thread + socket plumbing is exercised by
//! the daemon-level integration test in Step 10.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use serde::Deserialize;

use super::{IntentChannel, IntentEvent};

/// Niri IPC request payload that opens the event stream. Sent as one
/// JSON line; niri replies with one ACK line, then streams events.
const NIRI_EVENT_STREAM_REQ: &str = "\"EventStream\"";

/// Errors returned by [`NiriChannel::new`]. None are fatal — the
/// daemon treats every variant as "channel disabled, keep running",
/// mirroring `LogindChannel::BusUnreachable` and
/// `PsiChannel::Unavailable`.
#[derive(Debug)]
pub enum NiriError {
    /// `$NIRI_SOCKET` is unset / the socket file does not exist.
    /// Common on CI runners and any session without niri running.
    SocketUnavailable,
    /// `connect()` or initial `write()` to the socket failed.
    SocketIo,
}

impl std::fmt::Display for NiriError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NiriError::SocketUnavailable => write!(f, "niri socket unavailable"),
            NiriError::SocketIo => write!(f, "niri socket I/O failed"),
        }
    }
}

impl std::error::Error for NiriError {}

/// Subset of niri's `Window` IPC type that we deserialise. **`title`
/// is intentionally NOT a field** — destructuring drops it at the
/// boundary so it can never be carried downstream by accident.
#[derive(Debug, Clone, Deserialize)]
struct NiriWindow {
    app_id: Option<String>,
    #[serde(default)]
    is_focused: bool,
}

/// Top-level niri event envelope. Niri emits one JSON object per
/// line keyed by event-type — we only decode the two variants that
/// carry focus information.
#[derive(Debug, Clone, Deserialize)]
enum NiriEvent {
    /// Initial snapshot + every subsequent windows-set change.
    WindowsChanged { windows: Vec<NiriWindow> },
    /// Per-window open / focus / property change.
    WindowOpenedOrChanged { window: NiriWindow },
}

/// Pure parser: one JSON event line in → at most one `IntentEvent`
/// out. Returns `None` for any line we don't care about (most
/// events: `WorkspacesChanged`, `KeyboardLayoutsChanged`, …) and for
/// any line that doesn't decode as a known variant. **Strips title
/// at the boundary** — `NiriWindow` has no `title` field.
pub fn parse_event(line: &str) -> Option<IntentEvent> {
    let ev: NiriEvent = serde_json::from_str(line).ok()?;
    match ev {
        NiriEvent::WindowOpenedOrChanged { window } if window.is_focused => window
            .app_id
            .map(|app_id| IntentEvent::FocusedAppChanged { app_id }),
        NiriEvent::WindowsChanged { windows } => windows
            .into_iter()
            .find(|w| w.is_focused)
            .and_then(|w| w.app_id)
            .map(|app_id| IntentEvent::FocusedAppChanged { app_id }),
        _ => None,
    }
}

/// Resolve the niri IPC socket path. Honours `$NIRI_SOCKET` (niri's
/// own override env var); falls back to scanning `$XDG_RUNTIME_DIR`
/// for the first `niri.wayland-*.sock`. Returns
/// `Err(SocketUnavailable)` when no candidate exists.
fn locate_socket() -> Result<PathBuf, NiriError> {
    if let Ok(p) = std::env::var("NIRI_SOCKET") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Ok(pb);
        }
    }
    let runtime = std::env::var("XDG_RUNTIME_DIR").map_err(|_| NiriError::SocketUnavailable)?;
    let dir = std::fs::read_dir(&runtime).map_err(|_| NiriError::SocketUnavailable)?;
    for entry in dir.flatten() {
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if s.starts_with("niri.wayland-") && s.ends_with(".sock") {
            return Ok(entry.path());
        }
    }
    Err(NiriError::SocketUnavailable)
}

/// Background-thread niri IPC subscriber. Pushes the **most recent**
/// `FocusedAppChanged` event into a `Mutex<Option<…>>` slot that
/// `poll()` drains — older events get overwritten if `poll()` lags,
/// matching the SPEC's "1 Hz panel sampler" cadence (the daemon
/// doesn't want a stale queue of focus changes).
pub struct NiriChannel {
    slot: Arc<Mutex<Option<IntentEvent>>>,
    /// Kept so the worker thread is joined on Drop; otherwise the
    /// thread outlives the channel and the slot becomes a dangling
    /// reference (clippy-clean, but bad hygiene).
    _worker: thread::JoinHandle<()>,
}

impl NiriChannel {
    /// Connect to the niri socket, kick off the event-stream worker.
    /// Returns `Err(SocketUnavailable)` when niri isn't running —
    /// the daemon degrades cleanly (same shape as
    /// `LogindChannel::BusUnreachable`).
    pub fn new() -> Result<Self, NiriError> {
        let path = locate_socket()?;
        let mut stream = UnixStream::connect(&path).map_err(|_| NiriError::SocketIo)?;
        stream
            .write_all(format!("{NIRI_EVENT_STREAM_REQ}\n").as_bytes())
            .map_err(|_| NiriError::SocketIo)?;
        let slot: Arc<Mutex<Option<IntentEvent>>> = Arc::new(Mutex::new(None));
        let slot_w = Arc::clone(&slot);
        let worker = thread::spawn(move || {
            let reader = BufReader::new(stream);
            for line in reader.lines().map_while(Result::ok) {
                if let Some(ev) = parse_event(&line) {
                    if let Ok(mut g) = slot_w.lock() {
                        *g = Some(ev);
                    }
                }
            }
        });
        Ok(Self {
            slot,
            _worker: worker,
        })
    }
}

impl IntentChannel for NiriChannel {
    fn poll(&mut self) -> Option<IntentEvent> {
        self.slot.lock().ok().and_then(|mut g| g.take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC §4 "Privacy" hard guarantee: a niri event line that
    /// carries `"title": "secret-prep-doc.pdf"` must produce an
    /// `IntentEvent::FocusedAppChanged` with the `app_id` only.
    /// `IntentEvent::FocusedAppChanged` has no `title` field — the
    /// privacy floor is enforced by the type, not by runtime
    /// scrubbing.
    #[test]
    fn title_is_stripped() {
        let line = r#"{"WindowOpenedOrChanged":{"window":{"id":42,"title":"secret-prep-doc.pdf","app_id":"firefox","pid":1,"workspace_id":1,"is_focused":true,"is_floating":false,"is_urgent":false}}}"#;
        let ev = parse_event(line).expect("focused-window event parses");
        match ev {
            IntentEvent::FocusedAppChanged { app_id } => {
                assert_eq!(app_id, "firefox");
                // No `title` field on the variant by construction —
                // if someone adds one, this test file stops compiling.
            }
            other => panic!("expected FocusedAppChanged, got {other:?}"),
        }
    }

    /// `WindowsChanged` is niri's initial snapshot — the parser must
    /// also pick out the focused entry from the array (otherwise the
    /// daemon would miss the boot-time focused app until the next
    /// focus change).
    #[test]
    fn windows_changed_picks_focused_entry() {
        let line = r#"{"WindowsChanged":{"windows":[{"id":1,"title":"a","app_id":"foot","is_focused":false},{"id":2,"title":"b","app_id":"firefox","is_focused":true}]}}"#;
        assert_eq!(
            parse_event(line),
            Some(IntentEvent::FocusedAppChanged {
                app_id: "firefox".into(),
            })
        );
    }

    /// Events without a focused window (e.g. niri's
    /// `KeyboardLayoutsChanged`, or a `WindowOpenedOrChanged` for a
    /// non-focused window) must return `None` rather than firing a
    /// spurious focus event.
    #[test]
    fn unrelated_events_return_none() {
        let layout = r#"{"KeyboardLayoutsChanged":{"keyboard_layouts":{"names":["English (US)"],"current_idx":0}}}"#;
        assert_eq!(parse_event(layout), None);

        let unfocused = r#"{"WindowOpenedOrChanged":{"window":{"id":1,"title":"t","app_id":"firefox","is_focused":false}}}"#;
        assert_eq!(parse_event(unfocused), None);
    }

    /// Malformed JSON returns `None` rather than crashing the daemon
    /// worker thread. (Niri shouldn't emit malformed lines; this is
    /// a guard against future schema drift.)
    #[test]
    fn malformed_line_returns_none() {
        assert_eq!(parse_event(""), None);
        assert_eq!(parse_event("{not json"), None);
        assert_eq!(parse_event(r#"{"WhatIsThis":{}}"#), None);
    }
}
