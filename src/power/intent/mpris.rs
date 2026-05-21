//! MPRIS media-playback watcher (SPEC §2 "12-signal panel": media
//! channel). Discovers every well-known name on the session bus
//! matching `org.mpris.MediaPlayer2.*`, reads each player's
//! `PlaybackStatus` property, and emits
//! [`IntentEvent::MediaPlaying`] whenever **any** player reports
//! `"Playing"`.
//!
//! No player identity, no track title, no metadata of any kind
//! crosses the snapshot boundary — only the coarse "media is
//! playing" bool. Matches SPEC §4 "Privacy".

use std::sync::{Arc, Mutex};

use super::{IntentChannel, IntentEvent};

/// MPRIS spec well-known-name prefix; per-player names follow the
/// shape `org.mpris.MediaPlayer2.<id>` (e.g. `…spotify`,
/// `…firefox.instance123`).
const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
/// MPRIS player object path — every conforming player exposes the
/// `PlaybackStatus` property at this exact path.
const MPRIS_OBJECT: &str = "/org/mpris/MediaPlayer2";
/// `org.mpris.MediaPlayer2.Player` is where `PlaybackStatus` lives.
const MPRIS_PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
/// SPEC-defined value indicating the player is actively producing audio.
const PLAYBACK_PLAYING: &str = "Playing";

/// Errors `MprisChannel::new` returns. Same shape as the rest of
/// the intent channels.
#[derive(Debug)]
pub enum MprisError {
    BusUnreachable,
}

impl std::fmt::Display for MprisError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MprisError::BusUnreachable => write!(f, "session bus unreachable"),
        }
    }
}

impl std::error::Error for MprisError {}

/// Pure-fn classifier: given `(bus_name, status)` pairs, return
/// `true` iff any matching MPRIS player reports `"Playing"`. Caller
/// is responsible for fetching the property values from the bus.
pub fn any_player_playing(players: &[(String, &str)]) -> bool {
    players
        .iter()
        .any(|(name, status)| name.starts_with(MPRIS_PREFIX) && *status == PLAYBACK_PLAYING)
}

/// Stateful MPRIS channel. Holds the session-bus connection plus a
/// dedup slot — a sustained `"Playing"` only fires once. Step 10
/// will wire `PropertiesChanged` for sub-second latency; today the
/// channel polls on every tick (matches `LogindChannel`'s pattern).
pub struct MprisChannel {
    conn: zbus::blocking::Connection,
    /// `Some(true)` after we emitted `MediaPlaying`; cleared back to
    /// `Some(false)` once every player goes idle.
    last_emitted: Arc<Mutex<Option<bool>>>,
}

impl MprisChannel {
    /// Connect to the session bus. Returns `Err(BusUnreachable)` on
    /// CI / hermetic environments — the daemon keeps running.
    pub fn new() -> Result<Self, MprisError> {
        let conn = zbus::blocking::Connection::session().map_err(|_| MprisError::BusUnreachable)?;
        Ok(Self {
            conn,
            last_emitted: Arc::new(Mutex::new(None)),
        })
    }

    /// Walk the session bus for MPRIS players + collect their
    /// `PlaybackStatus`. Errors degrade to "no players", matching
    /// the no-snowflake rule.
    fn collect_statuses(&self) -> Vec<(String, String)> {
        let dbus = match zbus::blocking::fdo::DBusProxy::new(&self.conn) {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        };
        let names = match dbus.list_names() {
            Ok(n) => n,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for owned in names {
            let name: &str = owned.as_str();
            if !name.starts_with(MPRIS_PREFIX) {
                continue;
            }
            let proxy = match zbus::blocking::fdo::PropertiesProxy::builder(&self.conn)
                .destination(name.to_string())
                .and_then(|b| b.path(MPRIS_OBJECT))
                .and_then(|b| b.build())
            {
                Ok(p) => p,
                Err(_) => continue,
            };
            let iface_name = match MPRIS_PLAYER_IFACE.try_into() {
                Ok(n) => n,
                Err(_) => continue,
            };
            let value = match proxy.get(iface_name, "PlaybackStatus") {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Ok(s) = String::try_from(value) {
                out.push((name.to_string(), s));
            }
        }
        out
    }
}

impl IntentChannel for MprisChannel {
    fn poll(&mut self) -> Option<IntentEvent> {
        let raw = self.collect_statuses();
        let view: Vec<(String, &str)> = raw.iter().map(|(n, s)| (n.clone(), s.as_str())).collect();
        let playing = any_player_playing(&view);
        let mut slot = self.last_emitted.lock().ok()?;
        match (*slot, playing) {
            (Some(true), true) => None, // sustained → suppress
            (_, true) => {
                *slot = Some(true);
                Some(IntentEvent::MediaPlaying)
            }
            (_, false) => {
                *slot = Some(false);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC §2: any MPRIS player reporting `"Playing"` ⇒ fire
    /// `MediaPlaying`. A bus name that is not under the MPRIS
    /// prefix must NOT match even if its status happens to be
    /// `"Playing"`.
    #[test]
    fn playback_status_playing() {
        let players = vec![
            ("org.mpris.MediaPlayer2.spotify".to_string(), "Playing"),
            ("org.mpris.MediaPlayer2.firefox".to_string(), "Paused"),
        ];
        assert!(any_player_playing(&players));

        let none_playing = vec![
            ("org.mpris.MediaPlayer2.spotify".to_string(), "Paused"),
            ("org.mpris.MediaPlayer2.vlc".to_string(), "Stopped"),
        ];
        assert!(!any_player_playing(&none_playing));

        // Non-MPRIS name with status="Playing" must NOT match — the
        // classifier keys on the bus-name prefix.
        let off_prefix = vec![("com.example.Other".to_string(), "Playing")];
        assert!(!any_player_playing(&off_prefix));
    }

    /// Empty player list ⇒ trivially "no media" — channel must not
    /// emit.
    #[test]
    fn empty_player_list_returns_false() {
        assert!(!any_player_playing(&[]));
    }
}
