//! xdg-portal `ScreenCast` session counter (SPEC §2 "12-signal panel":
//! the "screen share / call" channel).
//!
//! **Step 7 known limitation:** `org.freedesktop.portal.ScreenCast`
//! does not expose direct session enumeration. The realistic shape
//! is to subscribe to `Session.Closed` + count `CreateSession` reply
//! successes — a best-effort approximation. Today the channel ships
//! the **counter scaffold + pure-fn predicate** (`active > 0`); the
//! real signal subscription lands together with Step 10's daemon.
//! If the session bus is unreachable, the channel degrades to "always
//! 0 active", which is the correct no-snowflake fallback.
//!
//! Privacy: no session id, no source name, no source-app metadata
//! ever crosses the snapshot boundary — only the coarse "≥ 1 active"
//! bool surfaces as `IntentEvent::ScreenCastActive`.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use super::{IntentChannel, IntentEvent};

/// Errors `ScreenCastChannel::new` returns.
#[derive(Debug)]
pub enum PortalError {
    BusUnreachable,
}

impl std::fmt::Display for PortalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PortalError::BusUnreachable => write!(f, "session bus unreachable"),
        }
    }
}

impl std::error::Error for PortalError {}

/// Pure predicate: a non-zero session count fires the event.
pub fn has_active_session(active: usize) -> bool {
    active > 0
}

/// ScreenCast session counter. The signal subscriber (Step 10)
/// `fetch_add(1, …)` on each `CreateSession` reply success and
/// `fetch_sub(1, …)` on each `Session.Closed`. Today the counter
/// stays at zero in the absence of the subscriber — the channel
/// degrades silently, matching the rest of the intent panel.
pub struct ScreenCastChannel {
    active: Arc<AtomicUsize>,
    /// Dedup state: only emit on the 0→≥1 transition (or after the
    /// counter dropped back to zero and re-rose).
    last_emitted: bool,
}

impl ScreenCastChannel {
    pub fn new() -> Result<Self, PortalError> {
        let _ = zbus::blocking::Connection::session().map_err(|_| PortalError::BusUnreachable)?;
        Ok(Self {
            active: Arc::new(AtomicUsize::new(0)),
            last_emitted: false,
        })
    }

    /// Borrow the counter so the Step-10 signal subscriber can
    /// `fetch_add` / `fetch_sub` without holding a reference to the
    /// channel.
    pub fn counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.active)
    }
}

impl IntentChannel for ScreenCastChannel {
    fn poll(&mut self) -> Option<IntentEvent> {
        let active = self.active.load(Ordering::SeqCst);
        let is_active = has_active_session(active);
        match (self.last_emitted, is_active) {
            (true, true) => None,
            (_, true) => {
                self.last_emitted = true;
                Some(IntentEvent::ScreenCastActive)
            }
            (_, false) => {
                self.last_emitted = false;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Zero active sessions ⇒ no event. One or more ⇒ event fires.
    #[test]
    fn predicate_fires_on_nonzero() {
        assert!(!has_active_session(0));
        assert!(has_active_session(1));
        assert!(has_active_session(5));
    }

    /// Counter-driven channel emits on 0→1 transition, dedupes on
    /// sustained 1, and re-arms after 1→0.
    #[test]
    fn channel_dedupes_sustained_active() {
        let active = Arc::new(AtomicUsize::new(0));
        let mut ch = ScreenCastChannel {
            active: Arc::clone(&active),
            last_emitted: false,
        };
        assert_eq!(ch.poll(), None);

        active.store(1, Ordering::SeqCst);
        assert_eq!(ch.poll(), Some(IntentEvent::ScreenCastActive));
        // Sustained → suppress.
        assert_eq!(ch.poll(), None);

        // Drop back to zero, then rise again → emit again.
        active.store(0, Ordering::SeqCst);
        assert_eq!(ch.poll(), None);
        active.store(2, Ordering::SeqCst);
        assert_eq!(ch.poll(), Some(IntentEvent::ScreenCastActive));
    }
}
