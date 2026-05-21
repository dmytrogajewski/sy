//! Notification body sniffer (SPEC §4 Privacy: **discards body text
//! immediately**). The channel subscribes to
//! `org.freedesktop.Notifications.Notify` signals on the session
//! bus, extracts a single coarse `bool` per body — "did the user
//! complain about fan noise?" — and then **drops the body**. The
//! body string never enters a struct, never gets logged, never
//! crosses the snapshot boundary.
//!
//! The pure-fn classifier [`body_contains_fan_complaint`] is what
//! the daemon would feed each incoming body to; the channel just
//! wraps a `Mutex<bool>` flag set by the (future) signal subscriber.
//! The bus-bound side degrades silently — Step 7 ships the parser +
//! the channel scaffold; Step 10's daemon wires the actual zbus
//! signal listener.

use std::sync::{Arc, Mutex};

use super::{IntentChannel, IntentEvent};

/// Coarse keywords matched case-insensitively against the body. The
/// SPEC §2 example phrasing is "fan is loud" — three substrings
/// catch the common phrasings without LM inference.
const FAN_KEYWORDS: &[&str] = &["fan", "loud", "noisy"];

/// Errors `NotifyChannel::new` returns. Same shape as
/// `LogindError`/`NiriError`: every variant is "channel disabled,
/// keep running".
#[derive(Debug)]
pub enum NotifyError {
    /// `zbus::blocking::Connection::session()` failed — common on CI
    /// runners + hermetic containers.
    BusUnreachable,
}

impl std::fmt::Display for NotifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotifyError::BusUnreachable => write!(f, "session bus unreachable"),
        }
    }
}

impl std::error::Error for NotifyError {}

/// Pure-fn classifier: does `body` contain any fan-complaint
/// keyword? Returns just a `bool` — the body string is consumed by
/// reference and the caller is expected to drop it immediately
/// after this call returns. Test `fan_keyword_detected_body_discarded`
/// enforces "no body field anywhere downstream".
pub fn body_contains_fan_complaint(body: &str) -> bool {
    let body_lc = body.to_lowercase();
    FAN_KEYWORDS.iter().any(|k| body_lc.contains(k))
}

/// Stateful channel. Holds a `complained` flag that the (future)
/// bus subscriber sets to `true` whenever
/// `body_contains_fan_complaint` fired; `poll()` consumes the flag
/// (returns `Some(FanComplaint)` once, then `None` until the next
/// matching notification).
///
/// The struct deliberately has **no `body: String` field** — the
/// privacy invariant is enforced by the type layout, not by runtime
/// scrubbing. Step 10 will attach the zbus subscriber.
pub struct NotifyChannel {
    complained: Arc<Mutex<bool>>,
}

impl NotifyChannel {
    /// Connect to the session bus. Falls back to the Step-7 stub
    /// shape (no signal subscriber yet) — the flag stays `false`
    /// until Step 10 wires the listener thread. Returns
    /// `Err(BusUnreachable)` so the daemon's anti-dead-code probe
    /// degrades cleanly in CI.
    pub fn new() -> Result<Self, NotifyError> {
        let _ = zbus::blocking::Connection::session().map_err(|_| NotifyError::BusUnreachable)?;
        Ok(Self {
            complained: Arc::new(Mutex::new(false)),
        })
    }

    /// Test hook: simulate a notification body crossing the
    /// classifier without spinning up the session bus. Body is taken
    /// by reference and **immediately dropped** — only the bool
    /// derived from it is kept.
    pub fn ingest_body(&self, body: &str) {
        if body_contains_fan_complaint(body) {
            if let Ok(mut g) = self.complained.lock() {
                *g = true;
            }
        }
        // `body` falls out of scope here. By construction the channel
        // owns nothing string-shaped.
    }
}

impl IntentChannel for NotifyChannel {
    fn poll(&mut self) -> Option<IntentEvent> {
        let mut g = self.complained.lock().ok()?;
        if *g {
            *g = false;
            Some(IntentEvent::FanComplaint)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC §4 Privacy: the channel must extract a coarse bool from
    /// a notification body and **discard the body text immediately**.
    /// We assert (a) the keyword classifier fires, (b) the emitted
    /// `IntentEvent::FanComplaint` carries no `body` field by
    /// construction, and (c) the `NotifyChannel` struct itself has
    /// no string-shaped storage of the body — the only state is the
    /// `Mutex<bool>` flag.
    #[test]
    fn fan_keyword_detected_body_discarded() {
        assert!(body_contains_fan_complaint("the fan is so loud right now"));
        assert!(body_contains_fan_complaint("NOISY day at the office"));
        assert!(!body_contains_fan_complaint("just checking in"));

        // Construct a channel without a bus by bypassing `new()` —
        // we only need to exercise `ingest_body` + `poll`.
        let ch = NotifyChannel {
            complained: Arc::new(Mutex::new(false)),
        };
        ch.ingest_body("fan is loud, please throttle");
        // The IntentEvent variant has zero fields — pattern-matching
        // it asserts at compile time that no body string snuck through.
        let ev = {
            let mut ch = ch;
            <NotifyChannel as IntentChannel>::poll(&mut ch)
        };
        match ev {
            Some(IntentEvent::FanComplaint) => {}
            other => panic!("expected FanComplaint, got {other:?}"),
        }
    }
}
