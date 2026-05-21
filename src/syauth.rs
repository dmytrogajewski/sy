//! syauth waybar applet.
//!
//! Surfaces the 6-digit LESC numeric-comparison code the privileged
//! desktop `syauth pair --waybar` process is currently waiting on,
//! and lets the operator accept / reject the bond with a click. The
//! two processes rendezvous over `$XDG_RUNTIME_DIR/syauth/`:
//!
//! - `pair-request.json` — written by `syauth pair --waybar` when
//!   BlueZ asks for user confirmation of the LESC numeric comparison.
//!   Schema:
//!
//!   ```json
//!   {
//!     "schema_version": 1,
//!     "kind": "pair_confirm",
//!     "request_id": "<pid>-<nanos>",
//!     "passkey": "692386",
//!     "created_at_secs": 1779039123
//!   }
//!   ```
//!
//! - `pair-response.json` — written by this applet on click. Schema:
//!
//!   ```json
//!   {
//!     "schema_version": 1,
//!     "request_id": "<matching>",
//!     "decision": "accept" | "reject"
//!   }
//!   ```
//!
//! Subcommands:
//!   sy syauth --waybar       → emit waybar JSON for the bar
//!   sy syauth accept         → write `accept` response
//!   sy syauth reject         → write `reject` response

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

const IPC_SUBDIR: &str = "syauth";
const REQUEST_FILE: &str = "pair-request.json";
const RESPONSE_FILE: &str = "pair-response.json";

/// Schema version both sides write + read. Bump together with the
/// matching constant in `crates/syauth-cli/src/pair_backend.rs`.
const SCHEMA_VERSION: u32 = 1;

/// Typed snapshot of `syauth status --json`. Designed to outlive the
/// JSON it was parsed from so step 2's waybar table can query it
/// without re-parsing.
///
/// `last_unlock_outcome` is intentionally a free-form `String` for
/// step 1: today `syauth status --json` does not emit it, and step 4
/// of the roadmap will promote it to a typed enum when it parses the
/// `/var/lib/syauth/last.log` tail. Carrying it as `Option<String>`
/// now keeps the struct shape forward-compatible without dead code.
#[derive(Debug, Clone)]
pub struct StatusSummary {
    pub state: String,
    pub peer_id: Option<String>,
    pub last_connect_ms_ago: Option<u64>,
    pub last_unlock_outcome: Option<String>,
    /// `peers[0].in_flight_challenges`. Drives the `active` pill state
    /// in step 2 of the syauth roadmap. `None` when no peer is bonded;
    /// `Some(0)` when bonded but idle.
    pub in_flight_challenges: Option<u64>,
}

impl StatusSummary {
    pub fn is_daemon_up(&self) -> bool {
        self.state == "up"
    }
    pub fn has_peer(&self) -> bool {
        self.peer_id.is_some()
    }
}

/// Parse `syauth status --json` output. Returns the typed summary or
/// the `serde_json` error verbatim so the caller can decide whether
/// to fall back, exit non-zero, or surface the diagnostic. The parser
/// is pure — it does not touch the filesystem, the network, or any
/// global state, which lets the test fixtures live as `&'static str`.
pub fn parse_status_json(src: &str) -> Result<StatusSummary, serde_json::Error> {
    let v: serde_json::Value = serde_json::from_str(src)?;
    let daemon = v.get("daemon").unwrap_or(&serde_json::Value::Null);
    let state = daemon
        .get("state")
        .and_then(|s| s.as_str())
        .unwrap_or("unknown")
        .to_string();
    let peers = daemon.get("peers").and_then(|p| p.as_array());
    let first = peers.and_then(|arr| arr.first());
    let peer_id = first
        .and_then(|p| p.get("peer_id"))
        .and_then(|s| s.as_str())
        .map(str::to_string);
    let last_connect_ms_ago = first
        .and_then(|p| p.get("last_connect_ms_ago"))
        .and_then(|n| n.as_u64());
    let in_flight_challenges = first
        .and_then(|p| p.get("in_flight_challenges"))
        .and_then(|n| n.as_u64());
    Ok(StatusSummary {
        state,
        peer_id,
        last_connect_ms_ago,
        last_unlock_outcome: None,
        in_flight_challenges,
    })
}

/// Format `last_connect_ms_ago` as a human "Ns ago" / "Nm ago" /
/// "Nh ago" suffix. Pure; keep next to the parser so step 2 reuses
/// the same renderer for the bar tooltip.
fn humanize_ms_ago(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

/// Render the one-line operator-visible summary. Ordering is chosen so
/// the line ends with the bond id (the operator's eye lands on the
/// most stable identifier last); when no peer is bonded we end with
/// `not paired`. When the daemon is down, we suppress the peer block
/// entirely — there is no useful bond truth without a live daemon.
pub fn render_status_line(s: &StatusSummary) -> String {
    if !s.is_daemon_up() {
        return format!("daemon {}", s.state);
    }
    let mut parts: Vec<String> = vec!["daemon up".to_string()];
    if let Some(ms) = s.last_connect_ms_ago {
        let mut chunk = format!("last connect {} ago", humanize_ms_ago(ms));
        if let Some(outcome) = &s.last_unlock_outcome {
            chunk.push(' ');
            chunk.push_str(outcome);
        }
        parts.push(chunk);
    }
    match &s.peer_id {
        Some(pid) => {
            let head: String = pid.chars().take(6).collect();
            parts.push(format!("bonded {head}\u{2026}"));
        }
        None => parts.push("not paired".to_string()),
    }
    parts.join(" \u{00b7} ")
}

/// Typed snapshot of `syauth doctor --json`. The pill only consumes
/// the `bluez_adapter` field today (Step 2 of the syauth roadmap); the
/// struct stays narrow on purpose — Step 5's `sy syauth doctor` will
/// promote the full doctor surface when it lands.
#[derive(Debug, Clone)]
pub struct DoctorSummary {
    pub bluez_adapter: String,
}

/// Parse `syauth doctor --json` output. Pure — fed by fixtures in
/// tests so unit tests never shell out. The probe captured on
/// 2026-05-19 puts `bluez_adapter` at the top level as a string
/// (`"ok"` / `"unknown"`); the parser tolerates a missing field as
/// `"unknown"` so a future syauth that hides the probe behind a
/// feature flag doesn't crash the bar.
pub fn parse_doctor_json(src: &str) -> Result<DoctorSummary, serde_json::Error> {
    let v: serde_json::Value = serde_json::from_str(src)?;
    let bluez_adapter = v
        .get("bluez_adapter")
        .and_then(|s| s.as_str())
        .unwrap_or("unknown")
        .to_string();
    Ok(DoctorSummary { bluez_adapter })
}

/// Rendered waybar slot output. The waybar `custom/` module consumes
/// `{text, tooltip, class}`; we keep the struct narrow so the
/// emitter can decide whether to omit `class` (no class hook needed
/// when no bond is in play and the CSS table only carries the five
/// step-2 states).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaybarPill {
    pub text: String,
    pub class: String,
    pub tooltip: String,
}

/// Pill icon. Nerd Font `fa-key` (U+F084) — a MONOCHROME key glyph
/// that respects CSS `color`. The 🔑 (U+1F511) color emoji ignores
/// CSS color because color emoji fonts paint their own bitmap, so
/// the state-driven color never showed through. Requires
/// `JetBrainsMono Nerd Font` (the bar's default — see
/// `configs/waybar/style.css` font-family).
const PILL_GLYPH_KEY: char = '\u{F084}'; //

/// "Recent" cutoff for last-audit-outcome → pill state. The window
/// must outlive a single sudo + biometric reaction (≈3 s) but be
/// short enough that an old denied row doesn't permanently stick
/// the pill red. 10 minutes mirrors the operator-facing
/// troubleshooting threshold in `docs/syauth-setup.md`.
const PILL_RECENT_OUTCOME_MS: u64 = 10 * 60 * 1000;

/// Render the pill from a parsed status + doctor pair + last audit
/// outcome. Pure; the live caller threads
/// `summary.in_flight_challenges.unwrap_or(0)`, the latest
/// `audit_log_tail` row (if any), and a wall-clock `now_ms` through.
///
/// State precedence:
///   `degraded` > `unpaired` > `active` > `ok`.
/// Daemon-down / recent-denied is the operator's most actionable
/// signal, so it wins. Pair-pending pre-empts everything earlier in
/// the call chain; this fn never sees a pending request — the caller
/// branches on the request file first, mirroring the existing
/// `waybar_out` path.
///
/// `doctor` is consumed only for the tooltip; the bluez_adapter
/// field is an upstream stub today (always `"unknown"`) so feeding
/// the pill class off it would lock the bar into a permanent
/// `degraded` verdict.
pub fn render_pill(
    status: &StatusSummary,
    in_flight: u64,
    doctor: &DoctorSummary,
    last_outcome: Option<&UnlockOutcome>,
    now_ms: u64,
    host: &str,
) -> WaybarPill {
    let icon = PILL_GLYPH_KEY.to_string();
    let recent_denied = last_outcome.and_then(|o| {
        let denied = matches!(o.kind, OutcomeKind::Denied(_));
        let age_ms = now_ms.saturating_sub(o.t_end_ms);
        if denied && age_ms <= PILL_RECENT_OUTCOME_MS {
            Some(o)
        } else {
            None
        }
    });
    let (class, headline) = if !status.is_daemon_up() {
        ("degraded", "daemon down".to_string())
    } else if !status.has_peer() {
        ("unpaired", "no phone bonded".to_string())
    } else if let Some(o) = recent_denied {
        let (class, label) = match &o.kind {
            OutcomeKind::Denied(r) if denied_is_transient(r) => ("reconnecting", "reconnecting"),
            OutcomeKind::Denied(_) => ("degraded", "phone away"),
            OutcomeKind::Ok => ("ok", "phone reachable"),
        };
        let reason = match &o.kind {
            OutcomeKind::Denied(r) => denied_reason_text(r),
            OutcomeKind::Ok => "ok",
        };
        (class, format!("{label} · last {reason}"))
    } else if in_flight > 0 {
        // In-flight challenge is the "phone is being used right now"
        // state — the operator's mental model is "cyan because sudo
        // works". Surface the activity in the headline; keep the
        // color cyan so a healthy sudo doesn't flash yellow every
        // 2-3 s.
        ("ok", format!("authenticating · {in_flight} in flight"))
    } else {
        ("ok", "phone reachable".to_string())
    };
    let tip = build_pill_tooltip(status, doctor, last_outcome, now_ms, host, &headline);
    WaybarPill {
        text: icon,
        class: class.to_string(),
        tooltip: tip,
    }
}

fn denied_reason_text(r: &DeniedReason) -> &'static str {
    match r {
        DeniedReason::PeerRevoked => "peer revoked",
        DeniedReason::NoBond => "no bond",
        DeniedReason::AuthError => "auth error",
        DeniedReason::TransportError => "transport error",
        DeniedReason::Other(_) => "denied",
    }
}

/// Whether a denied outcome looks transient (yellow / reconnecting)
/// or permanent (red / degraded). Transport-error and any unknown
/// reason are transient — the daemon will likely succeed on the
/// next attempt once the phone reconnects. Peer-revoked / no-bond /
/// auth-error are operator-actionable problems.
fn denied_is_transient(r: &DeniedReason) -> bool {
    matches!(r, DeniedReason::TransportError | DeniedReason::Other(_))
}

fn build_pill_tooltip(
    status: &StatusSummary,
    doctor: &DoctorSummary,
    last_outcome: Option<&UnlockOutcome>,
    now_ms: u64,
    host: &str,
    headline: &str,
) -> String {
    let mut lines: Vec<String> = vec![format!("syauth · {host} · {headline}")];
    match &status.peer_id {
        Some(pid) => {
            let head: String = pid.chars().take(8).collect();
            let suffix = match status.last_connect_ms_ago {
                Some(ms) => format!(" · last connect {} ago", humanize_ms_ago(ms)),
                None => String::new(),
            };
            lines.push(format!("peer {head}\u{2026}{suffix}"));
        }
        None => lines.push("no peer".to_string()),
    }
    if let Some(o) = last_outcome {
        let age = humanize_ms_ago(now_ms.saturating_sub(o.t_end_ms));
        let verdict = match &o.kind {
            OutcomeKind::Ok => "ok".to_string(),
            OutcomeKind::Denied(r) => format!("denied · {}", denied_reason_text(r)),
        };
        lines.push(format!("last unlock {age} ago: {verdict}"));
    }
    lines.push(format!(
        "daemon {state} · adapter {adapter}",
        state = status.state,
        adapter = doctor.bluez_adapter,
    ));
    lines.join("\\n")
}

/// Typed reason tag for a denied unlock attempt. Mirrors the
/// `outcome` column the daemon writes to `/var/lib/syauth/last.log`
/// (see `crates/syauth-presenced/src/audit.rs`). The roadmap
/// (Step 4) calls out four enumerated reasons plus an `Other` catch-
/// all so a new daemon-side tag doesn't crash the notifier — DoD #3
/// is about rejecting the ISO shape, not exhaustively typing every
/// CSV reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeniedReason {
    PeerRevoked,
    NoBond,
    AuthError,
    TransportError,
    Other(String),
}

impl DeniedReason {
    fn from_csv(tag: &str) -> Self {
        match tag {
            "peer-revoked" => DeniedReason::PeerRevoked,
            "no-bond" => DeniedReason::NoBond,
            "auth-err" | "auth-error" => DeniedReason::AuthError,
            "transport-error" => DeniedReason::TransportError,
            other => DeniedReason::Other(other.to_string()),
        }
    }

    /// Human-readable tag used in the notification body.
    pub fn as_display(&self) -> &str {
        match self {
            DeniedReason::PeerRevoked => "peer revoked",
            DeniedReason::NoBond => "no bond",
            DeniedReason::AuthError => "auth-err",
            DeniedReason::TransportError => "transport-error",
            DeniedReason::Other(s) => s.as_str(),
        }
    }
}

/// Typed outcome of a single unlock attempt. `Ok` means the daemon
/// signed the challenge; `Denied(_)` carries the reason tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutcomeKind {
    Ok,
    Denied(DeniedReason),
}

/// One parsed row of the daemon-side CSV in
/// `/var/lib/syauth/last.log`. Layout:
/// `peer_id,nonce,t_start_ms,t_end_ms,outcome,reason`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnlockOutcome {
    pub peer_id: String,
    pub nonce: String,
    pub t_start_ms: u64,
    pub t_end_ms: u64,
    pub kind: OutcomeKind,
}

impl UnlockOutcome {
    /// Idempotency key for the `notify_dispatcher`. Two distinct
    /// denied unlocks with the same reason must both notify
    /// (different `nonce` + `t_end_ms`); the same outcome read twice
    /// must not (identical key). Including nonce + t_end_ms makes the
    /// key collision-free in practice and grep-friendly in the cache
    /// file.
    pub fn state_key(&self) -> String {
        let tag = match &self.kind {
            OutcomeKind::Ok => "ok".to_string(),
            OutcomeKind::Denied(r) => format!("denied:{}", r.as_display()),
        };
        format!("{}|{}|{}", tag, self.nonce, self.t_end_ms)
    }
}

/// Parse the tail of `/var/lib/syauth/last.log` into typed outcomes.
/// Filters on `NF == 6 && t_end_ms ~ /^[0-9]+$/`, mirroring the
/// canonical awk filter in `~/sources/syauth/scripts/e2e-unlock.sh`.
/// Without that filter the PAM module's ISO-timestamp lines parse as
/// `elapsed_ms = 0` and mis-classify every unlock as an instant
/// transport-error. Returns the **last** `n` parsed CSV rows (oldest
/// first), so the caller can compare the trailing element against
/// the cached last-notified key.
pub fn audit_log_tail(content: &str, n: usize) -> Vec<UnlockOutcome> {
    let mut out: Vec<UnlockOutcome> = Vec::new();
    for line in content.lines() {
        let cols: Vec<&str> = line.split(',').collect();
        if cols.len() != AUDIT_NUM_COLUMNS {
            continue;
        }
        let Ok(t_start_ms) = cols[AUDIT_COL_T_START].parse::<u64>() else {
            continue;
        };
        let Ok(t_end_ms) = cols[AUDIT_COL_T_END].parse::<u64>() else {
            continue;
        };
        let outcome = cols[AUDIT_COL_OUTCOME];
        let kind = if outcome == AUDIT_OUTCOME_OK {
            OutcomeKind::Ok
        } else {
            OutcomeKind::Denied(DeniedReason::from_csv(outcome))
        };
        out.push(UnlockOutcome {
            peer_id: cols[AUDIT_COL_PEER_ID].to_string(),
            nonce: cols[AUDIT_COL_NONCE].to_string(),
            t_start_ms,
            t_end_ms,
            kind,
        });
    }
    if out.len() > n {
        let drop = out.len() - n;
        out.drain(0..drop);
    }
    out
}

/// Pair-flow side of the notifier state machine. The waybar applet
/// already classifies the live state into one of these three; the
/// notifier consumes them to fire pair-request / pair-completion
/// notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairState {
    /// No pair request in flight; no bond just landed.
    Idle,
    /// A pair-confirm request is currently presented to the operator.
    Pending { passkey: String },
    /// A bond is live (transition target after a previous `Pending`).
    /// `host` is the local machine name used in the notification body.
    Ok { host: String },
}

/// Persisted state the notifier compares against on each poll. The
/// `last_pair_key` and `last_outcome_key` fields are the idempotency
/// anchors: a transition is "the cached key changed".
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NotifierState {
    pub last_pair_key: Option<String>,
    pub last_outcome_key: Option<String>,
}

impl PairState {
    /// State key for the pair side. None for `Idle` so we never
    /// notify on idle (the bar pill is the visual signal).
    fn state_key(&self) -> Option<String> {
        match self {
            PairState::Idle => None,
            PairState::Pending { passkey } => Some(format!("pending:{passkey}")),
            PairState::Ok { host } => Some(format!("ok:{host}")),
        }
    }

    fn notification_body(&self) -> Option<String> {
        match self {
            PairState::Idle => None,
            PairState::Pending { passkey } => Some(format!(
                "syauth: pair request — code {passkey}, click bar to accept"
            )),
            PairState::Ok { host } => Some(format!("syauth: paired with {host}")),
        }
    }
}

/// Pure state machine. Compares the most recent outcome + pair state
/// against the cached keys and invokes `notify` exactly once per
/// transition; every transition that didn't notify (duplicate poll,
/// idle → idle) appends one line to `fallback_log` so the operator
/// can grep `~/.local/state/sy/syauth.log` for "why didn't I see
/// a notification" — DoD #2.
///
/// Inputs:
/// - `state`: mutated in place; carries the last-notified keys.
/// - `pair_state`: current pair-flow snapshot from the bar.
/// - `outcomes`: parsed audit-log tail; only the trailing element is
///   inspected (the bar polls at 1 Hz so transitions ≤ 1 row apart
///   are the only ones that matter; older rows have already been
///   notified or silently skipped).
/// - `notify`: callback that fires a desktop notification.
/// - `fallback_log`: callback that appends one line to the fallback
///   audit log for transitions that did not notify.
pub fn notify_dispatcher(
    state: &mut NotifierState,
    pair_state: &PairState,
    outcomes: &[UnlockOutcome],
    notify: &mut dyn FnMut(&str),
    fallback_log: &mut dyn FnMut(&str),
) {
    // -- pair side -----------------------------------------------------
    let pair_key = pair_state.state_key();
    if pair_key != state.last_pair_key {
        if let Some(body) = pair_state.notification_body() {
            notify(&body);
            fallback_log(&format!("pair-transition fired key={:?}", pair_key));
        } else {
            fallback_log(&format!(
                "pair-transition skipped key=idle prev={:?}",
                state.last_pair_key
            ));
        }
        state.last_pair_key = pair_key;
    } else if pair_state != &PairState::Idle {
        fallback_log(&format!("pair-poll skipped key={:?}", pair_key));
    }

    // -- unlock side ---------------------------------------------------
    let Some(last) = outcomes.last() else {
        return;
    };
    let key = last.state_key();
    if Some(&key) == state.last_outcome_key.as_ref() {
        fallback_log(&format!("unlock-poll skipped key={key}"));
        return;
    }
    match &last.kind {
        OutcomeKind::Ok => {
            // Roadmap: unlock-ok is intentionally silent (the bar
            // pill's `active` class is the visual signal). Still
            // record the transition so DoD #2 ("no silent failures")
            // surfaces it for an operator chasing a missing notif.
            fallback_log(&format!("unlock-ok silent key={key}"));
        }
        OutcomeKind::Denied(reason) => {
            notify(&format!("syauth: unlock denied ({})", reason.as_display()));
            fallback_log(&format!("unlock-denied fired key={key}"));
        }
    }
    state.last_outcome_key = Some(key);
}

/// `$XDG_STATE_HOME/sy/` (falling back to `~/.local/state/sy/`).
/// Created mode 0700 on first touch — the cache + fallback log carry
/// peer ids + nonces and shouldn't be world-readable. Returns `None`
/// when neither env var nor `$HOME` is set (no place to write).
fn xdg_state_dir() -> Option<PathBuf> {
    let base = if let Some(x) = std::env::var_os(SY_XDG_STATE_HOME).filter(|v| !v.is_empty()) {
        PathBuf::from(x)
    } else {
        let home = std::env::var_os(SY_HOME_ENV).filter(|v| !v.is_empty())?;
        PathBuf::from(home).join(".local/state")
    };
    let dir = base.join("sy");
    if let Err(e) = ensure_state_dir(&dir) {
        tracing::debug!(target: "sy::syauth", error = %e, dir = %dir.display(),
            "could not create state dir");
        return None;
    }
    Some(dir)
}

const SY_XDG_STATE_HOME: &str = "XDG_STATE_HOME";
const SY_HOME_ENV: &str = "HOME";
const NOTIFIER_CACHE_FILE: &str = "syauth.last-outcome";
const NOTIFIER_LOG_FILE: &str = "syauth.log";

/// Ensure `dir` exists with mode 0700. Idempotent.
fn ensure_state_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(dir)?.permissions();
        if perms.mode() & 0o777 != 0o700 {
            perms.set_mode(0o700);
            fs::set_permissions(dir, perms)?;
        }
    }
    Ok(())
}

/// Persist the notifier state. Two lines, key-prefixed for grep:
/// `pair=<key>\noutcome=<key>\n`. Empty values when `None`.
pub fn save_notifier_state(state: &NotifierState) -> Result<()> {
    let Some(dir) = xdg_state_dir() else {
        return Err(anyhow!(
            "no XDG_STATE_HOME / $HOME to persist notifier state"
        ));
    };
    let path = dir.join(NOTIFIER_CACHE_FILE);
    let body = format!(
        "pair={}\noutcome={}\n",
        state.last_pair_key.as_deref().unwrap_or(""),
        state.last_outcome_key.as_deref().unwrap_or(""),
    );
    write_atomic(&path, body.as_bytes()).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Load the cached notifier state. Missing file / parse error → an
/// empty (default) state; the caller fires a notification on the
/// very next transition, which is the safe degradation. We never
/// surface the error so a corrupt cache can't break the bar poll.
pub fn load_notifier_state() -> NotifierState {
    let Some(dir) = xdg_state_dir() else {
        return NotifierState::default();
    };
    let path = dir.join(NOTIFIER_CACHE_FILE);
    let Ok(body) = fs::read_to_string(&path) else {
        return NotifierState::default();
    };
    let mut state = NotifierState::default();
    for line in body.lines() {
        if let Some(v) = line.strip_prefix("pair=") {
            state.last_pair_key = if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            };
        } else if let Some(v) = line.strip_prefix("outcome=") {
            state.last_outcome_key = if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            };
        }
    }
    state
}

/// Append one line to the fallback log so the operator can grep
/// `~/.local/state/sy/syauth.log` for transitions that didn't fire
/// a notification (DoD #2). Format:
/// `<rfc3339> <transition> <reason>\n` so it stays grep-able.
fn append_fallback_log(line: &str) {
    let Some(dir) = xdg_state_dir() else { return };
    let path = dir.join(NOTIFIER_LOG_FILE);
    let ts = chrono::Utc::now().to_rfc3339();
    let entry = format!("{ts} {line}\n");
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| {
            use std::io::Write as _;
            f.write_all(entry.as_bytes())
        });
}

// Audit-CSV layout. Mirrors `AUDIT_COL_*` in
// `~/sources/syauth/scripts/e2e-unlock.sh` (0-based here, 1-based in
// awk). Bumping these in lock-step is intentional — the schema is a
// cross-repo contract.
const AUDIT_NUM_COLUMNS: usize = 6;
const AUDIT_COL_PEER_ID: usize = 0;
const AUDIT_COL_NONCE: usize = 1;
const AUDIT_COL_T_START: usize = 2;
const AUDIT_COL_T_END: usize = 3;
const AUDIT_COL_OUTCOME: usize = 4;
const AUDIT_OUTCOME_OK: &str = "ok";

/// Decoded pair-confirm request from the desktop side.
#[derive(Debug, Deserialize)]
struct PairRequest {
    schema_version: u32,
    kind: String,
    request_id: String,
    passkey: String,
    created_at_secs: u64,
}

/// Decision the applet writes back on click.
#[derive(Debug, Serialize)]
struct PairResponse<'a> {
    schema_version: u32,
    request_id: &'a str,
    decision: &'a str,
}

/// Default PAM control flag the sy wrapper passes through to
/// `syauth install-pam`. The wrapper opts the operator into the
/// "fall through when phone unavailable" semantic by default
/// (roadmap step 3): a required-failing syauth would block the
/// auth stack even when the phone is out of range and the user
/// wants FIDO / password instead. The upstream CLI keeps its own
/// default (`sufficient` today) for compatibility; this wrapper
/// forwards the flag explicitly so the operator-correct default
/// is invariant across upstream knob changes.
pub const SY_DEFAULT_PAM_CONTROL: &str = "sufficient";

/// Default PAM module-args string the wrapper forwards to
/// `syauth install-pam`. `timeout=8000` (8 s) matches the daemon's
/// `DEFAULT_AUTH_TIMEOUT` and gives a real BiometricPrompt enough
/// headroom; `timeout=1200` is the historical upstream value that
/// re-introduced the bug that fell every unlock through to FIDO,
/// so we never want to silently inherit it.
pub const SY_DEFAULT_PAM_MODULE_ARGS: &str = "timeout=8000";

/// Build the argv vector the sy wrapper hands to `syauth install-pam`.
/// Pure: no spawn, no fs. Tests assert the exact shape so the wrapper
/// stays in lock-step with the upstream CLI surface.
///
/// `yes` controls whether `--yes` is appended; CLIG idempotency rule:
/// when the operator did not pass `--yes` to `sy syauth install-pam`,
/// the wrapper leaves it off so the upstream CLI can ask before it
/// touches `/etc/pam.d`.
pub fn install_pam_args_builder(
    service: &str,
    control: &str,
    module_args: &str,
    yes: bool,
) -> Vec<String> {
    let mut argv = vec![
        "install-pam".to_string(),
        "--service".to_string(),
        service.to_string(),
        "--control".to_string(),
        control.to_string(),
        "--module-args".to_string(),
        module_args.to_string(),
    ];
    if yes {
        argv.push("--yes".to_string());
    }
    argv
}

/// Build the argv vector the sy wrapper hands to `syauth
/// uninstall-pam`. Pure; mirror of [`install_pam_args_builder`] for
/// the inverse operation. The upstream CLI restores the `.bak`
/// snapshot the install step wrote, so the argv carries only the
/// service name + `--yes` gate.
pub fn uninstall_pam_args_builder(service: &str, yes: bool) -> Vec<String> {
    let mut argv = vec![
        "uninstall-pam".to_string(),
        "--service".to_string(),
        service.to_string(),
    ];
    if yes {
        argv.push("--yes".to_string());
    }
    argv
}

/// Options struct for the `sy syauth` dispatcher. Carries the
/// action-specific flags (`--service`, `--control`, `--yes`) so the
/// dispatcher does not balloon into an N-arg signature as new actions
/// land. `service` / `control` are `None` for the action set that
/// doesn't need them (status / accept / reject); each consuming arm
/// validates its own requirements.
#[derive(Debug, Default, Clone, Copy)]
pub struct RunOpts<'a> {
    pub action: Option<&'a str>,
    pub waybar: bool,
    pub service: Option<&'a str>,
    pub control: Option<&'a str>,
    pub yes: bool,
}

/// Thin shim the `Cmd::Syauth` clap arm calls into. Builds a
/// [`RunOpts`] from the unpacked clap fields so `main.rs` stays a
/// one-liner per subcommand (LOC ceiling in `scripts/check_main_rs_loc.sh`).
pub fn run_cli(
    action: Option<&str>,
    waybar: bool,
    service: Option<&str>,
    control: Option<&str>,
    yes: bool,
) -> Result<()> {
    run(RunOpts {
        action,
        waybar,
        service,
        control,
        yes,
    })
}

pub fn run(opts: RunOpts<'_>) -> Result<()> {
    if opts.waybar {
        return waybar_out();
    }
    match opts.action.unwrap_or("status") {
        "accept" => respond("accept"),
        "reject" => respond("reject"),
        "status" => print_status(),
        "install-pam" => install_pam_dispatch(&opts),
        "uninstall-pam" => uninstall_pam_dispatch(&opts),
        "doctor" => doctor_dispatch(),
        other => Err(anyhow!(
            "unknown syauth action: {other} (accept|reject|status|install-pam|uninstall-pam|doctor; --waybar for bar JSON)"
        )),
    }
}

/// Shell out to `syauth doctor --json`, fold in the two sy-only fs
/// probes, render the OK/WARN/FAIL surface to stdout, and exit with
/// the aggregate code (0 ok, 1 any FAIL, 2 WARN-only). Performance:
/// the upstream `syauth doctor` already runs in well under 1 s; the
/// two sy probes are a stat + a small file read, so the wall-clock
/// budget (≤ 2 s, roadmap DoD #1) is comfortable.
fn doctor_dispatch() -> Result<()> {
    let out = Command::new(syauth_bin())
        .args(["doctor", "--json"])
        .output()
        .with_context(|| format!("spawn {SYAUTH_BIN_DEFAULT} doctor --json"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "syauth doctor --json failed: exit {:?}, stderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim(),
        ));
    }
    let body = std::str::from_utf8(&out.stdout).context("syauth doctor --json emitted non-utf8")?;
    let pam_present = Path::new(SY_PAM_SO_PATH).exists();
    let pam_text = fs::read_to_string(SY_PAM_SUDO_PATH).ok();
    let lines = build_doctor_lines(body, pam_present, pam_text.as_deref())
        .with_context(|| format!("parse syauth doctor --json: {body}"))?;
    print!("{}", render_doctor_lines(&lines));
    let code = doctor_exit_code(&lines);
    if code == 0 {
        Ok(())
    } else {
        std::process::exit(code)
    }
}

/// Shell out to `syauth install-pam` with the operator-correct
/// defaults. Prints the upstream CLI's stdout (the wrote-backup /
/// already-installed banner) to sy's stdout, forwards stderr, and
/// surfaces the upstream exit code as a typed error so agent callers
/// can branch on the result.
fn install_pam_dispatch(opts: &RunOpts<'_>) -> Result<()> {
    let service = opts
        .service
        .ok_or_else(|| anyhow!("sy syauth install-pam requires --service <name>"))?;
    let control = opts.control.unwrap_or(SY_DEFAULT_PAM_CONTROL);
    let argv = install_pam_args_builder(service, control, SY_DEFAULT_PAM_MODULE_ARGS, opts.yes);
    let out = Command::new(syauth_bin())
        .args(&argv)
        .output()
        .with_context(|| format!("spawn {SYAUTH_BIN_DEFAULT} install-pam"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    print!("{stdout}");
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }
    if !out.status.success() {
        return Err(anyhow!(
            "syauth install-pam failed: exit {:?}",
            out.status.code()
        ));
    }
    // Operator reminder (roadmap §step-3): pamtester is the only way
    // to validate the inserted line without a real `sudo` cycle.
    eprintln!("verify with: pamtester {service} $USER authenticate");
    Ok(())
}

/// Shell out to `syauth uninstall-pam`, which restores the `.bak`
/// snapshot the install step wrote.
fn uninstall_pam_dispatch(opts: &RunOpts<'_>) -> Result<()> {
    let service = opts
        .service
        .ok_or_else(|| anyhow!("sy syauth uninstall-pam requires --service <name>"))?;
    let argv = uninstall_pam_args_builder(service, opts.yes);
    let out = Command::new(syauth_bin())
        .args(&argv)
        .output()
        .with_context(|| format!("spawn {SYAUTH_BIN_DEFAULT} uninstall-pam"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    print!("{stdout}");
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }
    if !out.status.success() {
        return Err(anyhow!(
            "syauth uninstall-pam failed: exit {:?}",
            out.status.code()
        ));
    }
    Ok(())
}

// -- paths -----------------------------------------------------------------

fn ipc_dir() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_RUNTIME_DIR")?;
    if xdg.is_empty() {
        return None;
    }
    Some(PathBuf::from(xdg).join(IPC_SUBDIR))
}

fn request_path() -> Option<PathBuf> {
    ipc_dir().map(|d| d.join(REQUEST_FILE))
}

fn response_path() -> Option<PathBuf> {
    ipc_dir().map(|d| d.join(RESPONSE_FILE))
}

// -- request load ---------------------------------------------------------

/// Try to read + parse the current request file. Returns `None` if
/// the file is absent or unparseable. Schema-version mismatches are
/// dropped silently — the bar slot just stays empty so an out-of-
/// date applet doesn't pretend to confirm something it can't.
fn read_request() -> Option<PairRequest> {
    let p = request_path()?;
    let bytes = fs::read(&p).ok()?;
    let req: PairRequest = serde_json::from_slice(&bytes).ok()?;
    if req.schema_version != SCHEMA_VERSION || req.kind != "pair_confirm" {
        return None;
    }
    tracing::debug!(
        target: "sy::syauth",
        request_id = %req.request_id,
        created_at_secs = req.created_at_secs,
        "read pair request"
    );
    Some(req)
}

// -- waybar --------------------------------------------------------------

fn waybar_out() -> Result<()> {
    let req_opt = read_request();
    let host = hostname_for_pill();
    let status_res = live_status();
    let doctor_res = live_doctor();
    // Notifier side-effect: classify the current snapshot into a
    // `PairState` + audit tail and dispatch. Errors are intentionally
    // swallowed — a bar poll that can't fire `notify-send` must still
    // emit the pill JSON.
    dispatch_notifications(&req_opt, &status_res, &host);

    if let Some(req) = req_opt {
        let text = format!("{PILL_GLYPH_KEY} {}", req.passkey);
        let tip = format!(
            "syauth · pair request\\n6-digit code {}\\nclick: accept · right-click: reject",
            req.passkey
        );
        println!("{}", emit_pill_json(&text, "pending", &tip));
        return Ok(());
    }
    // No pending pair request: render the live four-class pill
    // (red / aqua / yellow + pending-green). Failures of either
    // upstream probe fall through to `degraded` (red) so the bar
    // never goes blank.
    let outcomes = live_audit_outcomes();
    let last_outcome = outcomes.last();
    let now_ms = now_ms_since_epoch();
    let pill = match (status_res, doctor_res) {
        (Ok(status), Ok(doctor)) => {
            let in_flight = status.in_flight_challenges.unwrap_or(0);
            render_pill(&status, in_flight, &doctor, last_outcome, now_ms, &host)
        }
        (status_res, doctor_res) => degraded_fallback(status_res, doctor_res, &host),
    };
    println!("{}", emit_pill_json(&pill.text, &pill.class, &pill.tooltip));
    Ok(())
}

/// Serialise a pill into the JSON shape waybar's `custom/` module
/// consumes. Hand-rolled (not `serde_json::to_string`) so the field
/// ordering matches the existing pre-step-2 output and a future grep
/// for `"class":"pending"` keeps working.
fn emit_pill_json(text: &str, class: &str, tooltip: &str) -> String {
    fn esc(s: &str) -> String {
        s.replace('\\', "\\\\").replace('"', "\\\"")
    }
    format!(
        r#"{{"text":"{t}","class":"{c}","tooltip":"{tip}"}}"#,
        t = esc(text),
        c = esc(class),
        tip = esc(tooltip),
    )
}

/// Best-effort host string for the pill text. Order: `/proc/sys/kernel/
/// hostname`, then `$HOSTNAME`, then the literal `"host"`. The branch
/// chosen is logged at debug level so an operator chasing a blank pill
/// has a paper trail.
fn hostname_for_pill() -> String {
    if let Ok(raw) = fs::read_to_string("/proc/sys/kernel/hostname") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            tracing::debug!(target: "sy::syauth", source = "proc", host = %trimmed,
                "hostname for pill");
            return trimmed.to_string();
        }
    }
    if let Ok(env) = std::env::var("HOSTNAME") {
        if !env.is_empty() {
            tracing::debug!(target: "sy::syauth", source = "env", host = %env,
                "hostname for pill");
            return env;
        }
    }
    tracing::debug!(target: "sy::syauth", source = "default",
        "hostname for pill (no /proc, no $HOSTNAME)");
    "host".to_string()
}

fn degraded_fallback(
    status_res: Result<StatusSummary>,
    doctor_res: Result<DoctorSummary>,
    host: &str,
) -> WaybarPill {
    let why = match (&status_res, &doctor_res) {
        (Err(e), _) => format!("status probe failed: {e}"),
        (_, Err(e)) => format!("doctor probe failed: {e}"),
        _ => "unknown".to_string(),
    };
    WaybarPill {
        text: PILL_GLYPH_KEY.to_string(),
        class: "degraded".to_string(),
        tooltip: format!("syauth · {host} · probe failure\\n{why}"),
    }
}

/// Wall-clock since unix epoch in milliseconds. Used only for
/// "how old is the last unlock outcome" math; on a clock-skew host
/// the worst that happens is the pill stays cyan a little longer
/// than it should.
fn now_ms_since_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// -- accept / reject ------------------------------------------------------

fn respond(decision: &str) -> Result<()> {
    let Some(req) = read_request() else {
        notify("syauth: no pending pair request");
        return Ok(());
    };
    let Some(out_path) = response_path() else {
        return Err(anyhow!("XDG_RUNTIME_DIR unset; cannot write response"));
    };
    let dir = out_path
        .parent()
        .ok_or_else(|| anyhow!("response path has no parent"))?;
    fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let body = serde_json::to_vec(&PairResponse {
        schema_version: SCHEMA_VERSION,
        request_id: &req.request_id,
        decision,
    })?;
    write_atomic(&out_path, &body).with_context(|| format!("write {}", out_path.display()))?;
    notify(&format!("syauth: {decision} (passkey {})", req.passkey));
    Ok(())
}

/// Atomically write `body` to `path` (write-then-rename pattern). The
/// desktop polls the response file and reads the moment it appears;
/// an interrupted partial write would surface as an empty/truncated
/// JSON which the desktop's hand-rolled parser would interpret as
/// "no decision yet" and keep polling. write_atomic eliminates that
/// race entirely.
fn write_atomic(path: &Path, body: &[u8]) -> Result<()> {
    let dir = path.parent().ok_or_else(|| anyhow!("path has no parent"))?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("pair-response")
    ));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(body)?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn notify(body: &str) {
    let _ = Command::new("notify-send")
        .args(["-a", "sy", "-t", "1500", "syauth", body])
        .status();
}

/// Audit-log path. Test-overridable via `SY_SYAUTH_AUDIT_LOG` so unit
/// tests can hand in a tempfile instead of `/var/lib/syauth/last.log`.
const SY_SYAUTH_AUDIT_LOG_ENV: &str = "SY_SYAUTH_AUDIT_LOG";
const SY_SYAUTH_AUDIT_LOG_DEFAULT: &str = "/var/lib/syauth/last.log";

/// Number of audit-log lines to consult per poll. The bar polls at
/// 1 Hz, so 64 covers any reasonable backlog while keeping the read
/// bounded.
const SY_SYAUTH_AUDIT_TAIL_LINES: usize = 64;

fn audit_log_path() -> PathBuf {
    std::env::var_os(SY_SYAUTH_AUDIT_LOG_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(SY_SYAUTH_AUDIT_LOG_DEFAULT))
}

/// Best-effort read of the audit log tail. Returns an empty Vec on
/// any I/O error — the notifier degrades silently rather than
/// blocking the bar poll on a permission glitch.
fn live_audit_outcomes() -> Vec<UnlockOutcome> {
    let path = audit_log_path();
    let Ok(body) = fs::read_to_string(&path) else {
        tracing::debug!(target: "sy::syauth", path = %path.display(),
            "audit log unreadable; notifier sees empty tail");
        return Vec::new();
    };
    audit_log_tail(&body, SY_SYAUTH_AUDIT_TAIL_LINES)
}

/// Classify the current pair-flow snapshot for the notifier. Pending
/// wins over Ok (the request file is the strongest signal). `Idle`
/// means no request file AND no bonded peer — there's nothing to
/// notify about. The Ok branch fires once on the
/// pending → ok transition.
fn classify_pair_state(
    req: &Option<PairRequest>,
    status_res: &Result<StatusSummary>,
    host: &str,
) -> PairState {
    if let Some(r) = req {
        return PairState::Pending {
            passkey: r.passkey.clone(),
        };
    }
    match status_res {
        Ok(s) if s.is_daemon_up() && s.has_peer() => PairState::Ok {
            host: host.to_string(),
        },
        _ => PairState::Idle,
    }
}

/// Glue between `waybar_out` and the pure `notify_dispatcher`:
/// loads cached state, classifies the snapshot, fires
/// `notify-send` + appends `~/.local/state/sy/syauth.log`, then
/// saves the updated state. Errors in any step are swallowed —
/// `waybar_out` MUST still emit the pill JSON.
fn dispatch_notifications(
    req: &Option<PairRequest>,
    status_res: &Result<StatusSummary>,
    host: &str,
) {
    let mut state = load_notifier_state();
    let pair_state = classify_pair_state(req, status_res, host);
    let outcomes = live_audit_outcomes();
    let mut notify_cb = |body: &str| notify(body);
    let mut log_cb = |line: &str| append_fallback_log(line);
    notify_dispatcher(
        &mut state,
        &pair_state,
        &outcomes,
        &mut notify_cb,
        &mut log_cb,
    );
    if let Err(e) = save_notifier_state(&state) {
        tracing::debug!(target: "sy::syauth", error = %e,
            "could not persist notifier state; next poll may re-notify");
    }
}

// -- status (live bond + daemon truth) ------------------------------------

/// Path to the upstream `syauth` CLI. Override with `SY_SYAUTH_BIN`
/// in tests / dev hosts where the binary isn't on `$PATH`.
const SYAUTH_BIN_ENV: &str = "SY_SYAUTH_BIN";
const SYAUTH_BIN_DEFAULT: &str = "syauth";

fn syauth_bin() -> std::ffi::OsString {
    std::env::var_os(SYAUTH_BIN_ENV).unwrap_or_else(|| std::ffi::OsString::from(SYAUTH_BIN_DEFAULT))
}

/// Shell out to `syauth status --json` and return the typed summary.
/// Failures bubble as `anyhow::Error` so the caller can decide how
/// to degrade (the pill renders a degraded fallback; `sy syauth
/// status` exits 1 with the error on stderr).
fn live_status() -> Result<StatusSummary> {
    let out = Command::new(syauth_bin())
        .args(["status", "--json"])
        .output()
        .with_context(|| format!("spawn {SYAUTH_BIN_DEFAULT} status --json"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "syauth status --json failed: exit {:?}, stderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim(),
        ));
    }
    let body = std::str::from_utf8(&out.stdout).context("syauth status --json emitted non-utf8")?;
    parse_status_json(body).with_context(|| format!("parse syauth status --json: {body}"))
}

/// Shell out to `syauth doctor --json` and return the narrow doctor
/// summary the pill consumes. Same failure semantics as `live_status`.
fn live_doctor() -> Result<DoctorSummary> {
    let out = Command::new(syauth_bin())
        .args(["doctor", "--json"])
        .output()
        .with_context(|| format!("spawn {SYAUTH_BIN_DEFAULT} doctor --json"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "syauth doctor --json failed: exit {:?}, stderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim(),
        ));
    }
    let body = std::str::from_utf8(&out.stdout).context("syauth doctor --json emitted non-utf8")?;
    parse_doctor_json(body).with_context(|| format!("parse syauth doctor --json: {body}"))
}

/// Implements `sy syauth status`. Prints the live one-liner and
/// exits 0 only when the daemon is up AND a peer is bonded; exits 1
/// otherwise so agents / scripts can branch on the return code.
fn print_status() -> Result<()> {
    let summary = match live_status() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sy syauth status: {e}");
            std::process::exit(1);
        }
    };
    println!("{}", render_status_line(&summary));
    if summary.is_daemon_up() && summary.has_peer() {
        Ok(())
    } else {
        std::process::exit(1)
    }
}

// -- Step 5: `sy syauth doctor` ---------------------------------------------

/// Tri-state outcome for a single doctor probe. The aggregate exit
/// code is derived from the worst status across all probes:
/// any `Fail` → 1; otherwise any `Warn` → 2; otherwise 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeStatus {
    Ok,
    Warn,
    Fail,
}

impl ProbeStatus {
    fn as_str(&self) -> &'static str {
        match self {
            ProbeStatus::Ok => "ok",
            ProbeStatus::Warn => "warn",
            ProbeStatus::Fail => "fail",
        }
    }
}

/// One rendered probe line. `hint` is the operator-actionable next
/// step (`systemctl --user status syauth-presenced`, etc.). Emitted
/// as `key=<key> status=<status> hint="<hint>"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeLine {
    pub key: String,
    pub status: ProbeStatus,
    pub hint: String,
}

/// Default file paths for the two sy-only probes. Test-overridable
/// via dedicated args on `build_doctor_lines` so unit tests never
/// touch `/usr` or `/etc`.
pub const SY_PAM_SO_PATH: &str = "/usr/lib64/security/pam_syauth.so";
pub const SY_PAM_SUDO_PATH: &str = "/etc/pam.d/sudo";

/// Pure helper: does `pam_text` carry an uncommented `pam_syauth.so`
/// auth line? Fed by `std::fs::read_to_string(SY_PAM_SUDO_PATH)` in
/// production; fed by string fixtures in tests. Commented lines
/// (`# auth …`) do not count — the operator could have left a stale
/// comment after an uninstall and that is not a live wire.
pub fn contains_pam_syauth_line(pam_text: &str) -> bool {
    pam_text.lines().any(|raw| {
        let trimmed = raw.trim_start();
        !trimmed.starts_with('#') && trimmed.contains("pam_syauth.so")
    })
}

/// Probe-builder order is the operator-visible diff order. Keep it
/// stable so two consecutive runs produce identical output.
const DOCTOR_PROBE_ORDER: &[&str] = &[
    "daemon",
    "bonds_file",
    "keys",
    "bluez_adapter",
    "systemctl",
    "last_log_tail",
    "pam_so_present",
    "pam_so_wired",
];

/// Aggregate exit code per the roadmap: 1 if any FAIL, 2 if WARN-only,
/// 0 otherwise.
pub fn doctor_exit_code(lines: &[ProbeLine]) -> i32 {
    if lines.iter().any(|l| l.status == ProbeStatus::Fail) {
        1
    } else if lines.iter().any(|l| l.status == ProbeStatus::Warn) {
        2
    } else {
        0
    }
}

/// Render the probe list as one `key=… status=… hint="…"` line per
/// probe. Single-line hints (quoted) so the surface is greppable.
pub fn render_doctor_lines(lines: &[ProbeLine]) -> String {
    let mut out = String::new();
    for l in lines {
        let hint = l.hint.replace('"', "\\\"");
        out.push_str(&format!(
            "key={} status={} hint=\"{}\"\n",
            l.key,
            l.status.as_str(),
            hint
        ));
    }
    out
}

/// Build the typed probe-line vector from a `syauth doctor --json`
/// blob + the two sy-only inputs (`pam_so_present` bool and
/// `pam_sudo_contents` string — both injected by the caller so this
/// function stays pure and unit-testable).
pub fn build_doctor_lines(
    doctor_json: &str,
    pam_so_present: bool,
    pam_sudo_contents: Option<&str>,
) -> Result<Vec<ProbeLine>, serde_json::Error> {
    let v: serde_json::Value = serde_json::from_str(doctor_json)?;
    let mut probes: Vec<ProbeLine> = Vec::with_capacity(DOCTOR_PROBE_ORDER.len());
    probes.push(probe_daemon(&v));
    probes.push(probe_bonds(&v));
    probes.push(probe_keys(&v));
    probes.push(probe_bluez(&v));
    probes.push(probe_systemctl(&v));
    probes.push(probe_log_tail(&v));
    probes.push(probe_pam_present(pam_so_present));
    probes.push(probe_pam_wired(pam_sudo_contents));
    Ok(probes)
}

fn probe_daemon(v: &serde_json::Value) -> ProbeLine {
    let up = v
        .get("daemon")
        .and_then(|d| d.get("state"))
        .and_then(|s| s.as_str())
        == Some("up");
    if up {
        ProbeLine {
            key: "daemon".into(),
            status: ProbeStatus::Ok,
            hint: "daemon up".into(),
        }
    } else {
        ProbeLine {
            key: "daemon".into(),
            status: ProbeStatus::Fail,
            hint: "systemctl --user status syauth-presenced".into(),
        }
    }
}

fn probe_bonds(v: &serde_json::Value) -> ProbeLine {
    let b = v.get("bonds_file");
    let exists = b.and_then(|x| x.get("exists")).and_then(|x| x.as_bool()) == Some(true);
    let parseable = b.and_then(|x| x.get("parseable")).and_then(|x| x.as_bool()) == Some(true);
    let count = b
        .and_then(|x| x.get("count"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    if exists && parseable && count > 0 {
        ProbeLine {
            key: "bonds_file".into(),
            status: ProbeStatus::Ok,
            hint: format!("{count} bond(s)"),
        }
    } else {
        ProbeLine {
            key: "bonds_file".into(),
            status: ProbeStatus::Warn,
            hint: "syauth pair --waybar".into(),
        }
    }
}

fn probe_keys(v: &serde_json::Value) -> ProbeLine {
    let files = v
        .get("keys")
        .and_then(|k| k.get("files"))
        .and_then(|f| f.as_array());
    match files {
        Some(arr) if !arr.is_empty() => {
            let all_ok = arr
                .iter()
                .all(|f| f.get("ok").and_then(|x| x.as_bool()) == Some(true));
            if all_ok {
                ProbeLine {
                    key: "keys".into(),
                    status: ProbeStatus::Ok,
                    hint: format!("{} file(s) mode 0600", arr.len()),
                }
            } else {
                ProbeLine {
                    key: "keys".into(),
                    status: ProbeStatus::Fail,
                    hint: "chmod 0600 /var/lib/syauth/keys/*.bin".into(),
                }
            }
        }
        _ => ProbeLine {
            key: "keys".into(),
            status: ProbeStatus::Warn,
            hint: "no keys yet — pair a phone first".into(),
        },
    }
}

fn probe_bluez(v: &serde_json::Value) -> ProbeLine {
    let raw = v
        .get("bluez_adapter")
        .and_then(|x| x.as_str())
        .unwrap_or("missing");
    match raw {
        "ok" => ProbeLine {
            key: "bluez_adapter".into(),
            status: ProbeStatus::Ok,
            hint: "adapter up".into(),
        },
        "unknown" => ProbeLine {
            key: "bluez_adapter".into(),
            status: ProbeStatus::Warn,
            hint: "bluetoothctl show".into(),
        },
        other => ProbeLine {
            key: "bluez_adapter".into(),
            status: ProbeStatus::Fail,
            hint: format!("adapter state: {other}"),
        },
    }
}

fn probe_systemctl(v: &serde_json::Value) -> ProbeLine {
    let raw = v
        .get("systemctl")
        .and_then(|x| x.as_str())
        .unwrap_or("missing");
    if raw == "active" {
        ProbeLine {
            key: "systemctl".into(),
            status: ProbeStatus::Ok,
            hint: "unit active".into(),
        }
    } else {
        ProbeLine {
            key: "systemctl".into(),
            status: ProbeStatus::Fail,
            hint: "systemctl --user enable --now syauth-presenced".into(),
        }
    }
}

fn probe_log_tail(v: &serde_json::Value) -> ProbeLine {
    let empty = v
        .get("last_log_tail")
        .and_then(|x| x.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(true);
    if empty {
        ProbeLine {
            key: "last_log_tail".into(),
            status: ProbeStatus::Warn,
            hint: "no unlock attempts yet — try sudo true".into(),
        }
    } else {
        ProbeLine {
            key: "last_log_tail".into(),
            status: ProbeStatus::Ok,
            hint: "audit log non-empty".into(),
        }
    }
}

fn probe_pam_present(present: bool) -> ProbeLine {
    if present {
        ProbeLine {
            key: "pam_so_present".into(),
            status: ProbeStatus::Ok,
            hint: SY_PAM_SO_PATH.into(),
        }
    } else {
        ProbeLine {
            key: "pam_so_present".into(),
            status: ProbeStatus::Fail,
            hint: "build + install pam_syauth.so".into(),
        }
    }
}

fn probe_pam_wired(pam_text: Option<&str>) -> ProbeLine {
    let wired = pam_text.map(contains_pam_syauth_line).unwrap_or(false);
    if wired {
        ProbeLine {
            key: "pam_so_wired".into(),
            status: ProbeStatus::Ok,
            hint: SY_PAM_SUDO_PATH.into(),
        }
    } else {
        ProbeLine {
            key: "pam_so_wired".into(),
            status: ProbeStatus::Warn,
            hint: "sy syauth install-pam --service sudo".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `XDG_RUNTIME_DIR` is a process-global; cargo test runs tests
    /// in parallel; without this gate the three env-mutating tests
    /// race and one of them sees the other's tempdir before it can
    /// restore the var. Use the crate-wide canonical lock so we also
    /// serialise against `aiplane::ipc::tests` (which dial sockets
    /// resolved from the same env var).
    use crate::aiplane::TEST_ENV_LOCK as XDG_LOCK;

    #[test]
    fn waybar_emits_empty_slot_when_no_request_file() {
        let _guard = XDG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // We can't easily intercept stdout here, so just exercise
        // the read_request path: with no XDG_RUNTIME_DIR or no file,
        // read_request returns None.
        let saved = std::env::var_os("XDG_RUNTIME_DIR");
        // Point XDG_RUNTIME_DIR at a tempdir with no request file.
        let td = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", td.path());
        }
        assert!(read_request().is_none());
        // Restore env.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
    }

    #[test]
    fn waybar_reads_valid_request_file() {
        let _guard = XDG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let saved = std::env::var_os("XDG_RUNTIME_DIR");
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join(IPC_SUBDIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(REQUEST_FILE),
            br#"{"schema_version":1,"kind":"pair_confirm","request_id":"abc","passkey":"123456","created_at_secs":1700000000}"#,
        )
        .unwrap();
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", td.path());
        }
        let req = read_request().expect("must parse");
        assert_eq!(req.passkey, "123456");
        assert_eq!(req.request_id, "abc");
        unsafe {
            match saved {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
    }

    // -- Step 1: parse_status_json + render_status_line --------------------

    /// Fixture captured 2026-05-19 from
    /// `~/sources/syauth/target/debug/syauth status --json` on a host
    /// with a single bonded Galaxy S25 Ultra and the daemon up.
    const FIXTURE_BONDED: &str = r#"{
  "daemon_socket": "/run/user/1000/syauth/auth.sock",
  "daemon": {
    "state": "up",
    "started_at": 1779192091,
    "peers": [
      {
        "peer_id": "665d53ea76d2b59f1931ba526947b26d",
        "last_challenge_ms_ago": 4321,
        "last_connect_ms_ago": 4321,
        "current_session_uuid": "386540a3-4334-dbdc-6ccf-2ce3bdc7c67a",
        "in_flight_challenges": 0
      }
    ]
  }
}"#;

    const FIXTURE_NO_PEERS: &str = r#"{
  "daemon_socket": "/run/user/1000/syauth/auth.sock",
  "daemon": { "state": "up", "started_at": 1779192091, "peers": [] }
}"#;

    const FIXTURE_DAEMON_DOWN: &str = r#"{
  "daemon_socket": "/run/user/1000/syauth/auth.sock",
  "daemon": { "state": "down", "started_at": 0, "peers": [] }
}"#;

    #[test]
    fn status_renders_bonded_line() {
        let summary = parse_status_json(FIXTURE_BONDED).expect("fixture parses");
        let line = render_status_line(&summary);
        // Roadmap contract: the one-liner ends with `bonded <peer8>…`
        // so the operator's eye lands on the bond id last.
        assert!(line.ends_with("bonded 665d53…"), "got: {line:?}");
        assert!(summary.is_daemon_up());
        assert!(summary.has_peer());
    }

    #[test]
    fn status_handles_empty_peers_array() {
        let summary = parse_status_json(FIXTURE_NO_PEERS).expect("fixture parses");
        let line = render_status_line(&summary);
        assert!(line.contains("not paired"), "got: {line:?}");
        assert!(summary.is_daemon_up());
        assert!(!summary.has_peer());
    }

    #[test]
    fn status_line_includes_unlock_outcome_when_set() {
        // The parser leaves `last_unlock_outcome` as `None` today;
        // step 4 fills it from `/var/lib/syauth/last.log`. Exercise
        // the renderer's outcome branch now so step 4 only has to
        // touch the parser, not the renderer.
        let mut s = parse_status_json(FIXTURE_BONDED).expect("fixture parses");
        s.last_unlock_outcome = Some("ok".to_string());
        assert!(render_status_line(&s).contains("ago ok"));
        s.last_unlock_outcome = Some("denied".to_string());
        assert!(render_status_line(&s).contains("ago denied"));
    }

    #[test]
    fn status_handles_daemon_down() {
        let summary = parse_status_json(FIXTURE_DAEMON_DOWN).expect("fixture parses");
        let line = render_status_line(&summary);
        // No peer block when daemon is down; the operator's first
        // problem is the daemon, not the bond.
        assert!(line.contains("daemon down"), "got: {line:?}");
        assert!(!line.contains("bonded"), "got: {line:?}");
        assert!(!summary.is_daemon_up());
    }

    // -- Step 2: four-class pill renderer ---------------------------------

    fn doctor_unknown() -> DoctorSummary {
        DoctorSummary {
            bluez_adapter: "unknown".to_string(),
        }
    }

    #[test]
    fn waybar_renders_ok_class_when_bonded_idle() {
        let s = parse_status_json(FIXTURE_BONDED).expect("fixture parses");
        let d = doctor_unknown();
        let pill = render_pill(&s, 0, &d, None, 0, "fedora");
        assert_eq!(pill.class, "ok", "got: {pill:?}");
        assert_eq!(
            pill.text, "\u{F084}",
            "pill text should be the key emoji alone"
        );
        assert!(
            pill.tooltip.contains("fedora"),
            "tooltip carries host: {pill:?}"
        );
        assert!(pill.tooltip.contains("phone reachable"), "got: {pill:?}");
    }

    #[test]
    fn waybar_renders_unpaired_when_no_bond() {
        let s = parse_status_json(FIXTURE_NO_PEERS).expect("fixture parses");
        let pill = render_pill(&s, 0, &doctor_unknown(), None, 0, "fedora");
        assert_eq!(pill.class, "unpaired", "got: {pill:?}");
        assert_eq!(pill.text, "\u{F084}");
        assert!(pill.tooltip.contains("no phone bonded"), "got: {pill:?}");
    }

    #[test]
    fn waybar_renders_degraded_when_daemon_down() {
        let s = parse_status_json(FIXTURE_DAEMON_DOWN).expect("fixture parses");
        let pill = render_pill(&s, 0, &doctor_unknown(), None, 0, "fedora");
        assert_eq!(pill.class, "degraded", "got: {pill:?}");
        assert_eq!(pill.text, "\u{F084}");
        assert!(pill.tooltip.contains("daemon down"), "got: {pill:?}");
    }

    #[test]
    fn waybar_stays_ok_when_adapter_is_unknown_upstream_stub() {
        // Regression: the upstream `syauth doctor --json` always
        // emits `bluez_adapter: "unknown"` today; the bar must NOT
        // treat that as degraded — otherwise the pill is locked red
        // for everyone on green hosts.
        let s = parse_status_json(FIXTURE_BONDED).expect("fixture parses");
        let pill = render_pill(&s, 0, &doctor_unknown(), None, 0, "fedora");
        assert_eq!(pill.class, "ok", "got: {pill:?}");
    }

    #[test]
    fn waybar_renders_pending_when_request_present() {
        // Pair-request pre-empts every other state. The emitter is
        // shared between the pending path and the four-class path,
        // so assert against the JSON shape directly — that's what
        // waybar's `custom/` module parses.
        let json = emit_pill_json("\u{F084} 692386", "pending", "tip");
        assert!(json.contains(r#""class":"pending""#), "got: {json}");
        assert!(json.contains("692386"), "got: {json}");
        assert!(json.contains("\u{F084}"), "got: {json}");
    }

    #[test]
    fn emit_pill_json_escapes_quotes_and_backslashes() {
        // Tooltips can contain quotes (the pair tooltip already does)
        // and a future operator-set hostname could carry a backslash.
        // The emitter MUST escape both so waybar's JSON parser
        // accepts the slot.
        let json = emit_pill_json(r#"a"b\c"#, "ok", "tip");
        assert!(json.contains(r#"a\"b\\c"#), "got: {json}");
    }

    #[test]
    fn waybar_stays_ok_during_in_flight_challenge() {
        // In-flight is the "phone is being used right now" healthy
        // state — operator's mental model is cyan, not yellow.
        // Yellow is reserved for actual reconnection trouble.
        let s = parse_status_json(FIXTURE_BONDED).expect("fixture parses");
        let pill = render_pill(&s, 1, &doctor_unknown(), None, 0, "fedora");
        assert_eq!(pill.class, "ok", "got: {pill:?}");
        assert_eq!(pill.text, "\u{F084}");
        assert!(pill.tooltip.contains("authenticating"), "got: {pill:?}");
        assert!(pill.tooltip.contains("1 in flight"), "got: {pill:?}");
    }

    #[test]
    fn waybar_renders_reconnecting_on_recent_transient_denied() {
        // response-timeout (parsed as DeniedReason::Other) and
        // transport-error are both transient → yellow, not red.
        let s = parse_status_json(FIXTURE_BONDED).expect("fixture parses");
        let now: u64 = 1_779_200_000_000;
        let timeout = UnlockOutcome {
            peer_id: "665d53".to_string(),
            nonce: "n".to_string(),
            t_start_ms: now - 7_000,
            t_end_ms: now - 5_000,
            kind: OutcomeKind::Denied(DeniedReason::Other("response-timeout".to_string())),
        };
        let pill = render_pill(&s, 0, &doctor_unknown(), Some(&timeout), now, "fedora");
        assert_eq!(pill.class, "reconnecting", "got: {pill:?}");
        assert!(pill.tooltip.contains("reconnecting"), "got: {pill:?}");
    }

    #[test]
    fn waybar_renders_degraded_on_recent_permanent_denied() {
        // peer-revoked / no-bond / auth-error are operator-actionable
        // (rebond, re-key) → red.
        let s = parse_status_json(FIXTURE_BONDED).expect("fixture parses");
        let now: u64 = 1_779_200_000_000;
        let revoked = UnlockOutcome {
            peer_id: "665d53".to_string(),
            nonce: "n".to_string(),
            t_start_ms: now - 7_000,
            t_end_ms: now - 5_000,
            kind: OutcomeKind::Denied(DeniedReason::PeerRevoked),
        };
        let pill = render_pill(&s, 0, &doctor_unknown(), Some(&revoked), now, "fedora");
        assert_eq!(pill.class, "degraded", "got: {pill:?}");
        assert!(pill.tooltip.contains("phone away"), "got: {pill:?}");
        assert!(pill.tooltip.contains("peer revoked"), "got: {pill:?}");
    }

    #[test]
    fn waybar_renders_reconnecting_when_recent_unlock_was_transport_error() {
        // The operator-facing "trying to reach phone" signal is a
        // recent transport-error in the audit log. Inside the 10-min
        // window the pill goes yellow (reconnecting); outside, it
        // stays cyan.
        let s = parse_status_json(FIXTURE_BONDED).expect("fixture parses");
        let now: u64 = 1_779_200_000_000;
        let recent = UnlockOutcome {
            peer_id: "665d53".to_string(),
            nonce: "n".to_string(),
            t_start_ms: now - 62_000,
            t_end_ms: now - 60_000,
            kind: OutcomeKind::Denied(DeniedReason::TransportError),
        };
        let pill = render_pill(&s, 0, &doctor_unknown(), Some(&recent), now, "fedora");
        assert_eq!(pill.class, "reconnecting", "got: {pill:?}");
        assert!(pill.tooltip.contains("transport error"), "got: {pill:?}");
    }

    #[test]
    fn waybar_renders_ok_when_denied_outcome_is_outside_recent_window() {
        let s = parse_status_json(FIXTURE_BONDED).expect("fixture parses");
        let now: u64 = 1_779_200_000_000;
        let old = UnlockOutcome {
            peer_id: "665d53".to_string(),
            nonce: "n".to_string(),
            t_start_ms: now - 30 * 60 * 1000 - 2_000,
            t_end_ms: now - 30 * 60 * 1000, // 30 min ago
            kind: OutcomeKind::Denied(DeniedReason::TransportError),
        };
        let pill = render_pill(&s, 0, &doctor_unknown(), Some(&old), now, "fedora");
        assert_eq!(pill.class, "ok", "got: {pill:?}");
    }

    #[test]
    fn waybar_tooltip_carries_peer_and_last_unlock_when_ok() {
        let s = parse_status_json(FIXTURE_BONDED).expect("fixture parses");
        let now: u64 = 1_779_200_000_000;
        let ok = UnlockOutcome {
            peer_id: "665d53ea76d2b59f1931ba526947b26d".to_string(),
            nonce: "n".to_string(),
            t_start_ms: now - 7_000,
            t_end_ms: now - 5_000,
            kind: OutcomeKind::Ok,
        };
        let pill = render_pill(&s, 0, &doctor_unknown(), Some(&ok), now, "fedora");
        assert_eq!(pill.class, "ok", "got: {pill:?}");
        // Multi-line tooltip — uses literal `\\n` so waybar JSON parses it.
        assert!(pill.tooltip.contains("\\n"), "got: {pill:?}");
        assert!(pill.tooltip.contains("peer 665d53ea"), "got: {pill:?}");
        assert!(pill.tooltip.contains("last unlock"), "got: {pill:?}");
        assert!(pill.tooltip.contains(": ok"), "got: {pill:?}");
    }

    // -- Step 2: parse_doctor_json -----------------------------------------

    /// Fixture captured 2026-05-19 from
    /// `~/sources/syauth/target/debug/syauth doctor --json` on the
    /// same host as `FIXTURE_BONDED`. `bluez_adapter` is "unknown"
    /// because the upstream probe is currently a stub — the field is
    /// nonetheless top-level (not under `bluez.adapter` or
    /// `bluez_adapter.state`).
    const FIXTURE_DOCTOR_ADAPTER_UNKNOWN: &str = r#"{
  "daemon_socket": "/run/user/1000/syauth/auth.sock",
  "daemon": { "state": "up" },
  "bluez_adapter": "unknown",
  "systemctl": "active",
  "summary": "ok"
}"#;

    const FIXTURE_DOCTOR_ADAPTER_OK: &str = r#"{
  "daemon": { "state": "up" },
  "bluez_adapter": "ok",
  "summary": "ok"
}"#;

    #[test]
    fn doctor_parser_extracts_bluez_adapter_state() {
        let d = parse_doctor_json(FIXTURE_DOCTOR_ADAPTER_OK).expect("fixture parses");
        assert_eq!(d.bluez_adapter, "ok");
        let d2 = parse_doctor_json(FIXTURE_DOCTOR_ADAPTER_UNKNOWN).expect("fixture parses");
        assert_eq!(d2.bluez_adapter, "unknown");
    }

    // -- Step 3: install-pam argv builder ----------------------------------

    #[test]
    fn install_pam_args_builder_passes_sufficient_by_default() {
        let argv = install_pam_args_builder(
            "sudo",
            SY_DEFAULT_PAM_CONTROL,
            SY_DEFAULT_PAM_MODULE_ARGS,
            true,
        );
        // Argv shape (roadmap §step-3): subcommand first, then named
        // flags. Asserting on the pair sequence prevents accidental
        // flag-name drift (`--ctrl` vs `--control`).
        let pair_idx = argv
            .iter()
            .position(|s| s == "--control")
            .expect("flag present");
        assert_eq!(argv[pair_idx + 1], "sufficient");
        let svc_idx = argv
            .iter()
            .position(|s| s == "--service")
            .expect("flag present");
        assert_eq!(argv[svc_idx + 1], "sudo");
        assert!(
            argv.contains(&"--yes".to_string()),
            "--yes must be forwarded"
        );
        assert!(
            !argv.iter().any(|s| s == "required"),
            "argv must not silently fall back to required"
        );
    }

    #[test]
    fn install_pam_args_builder_includes_timeout_8000() {
        // Roadmap reality-corrected default: 8000 ms, never 1200.
        let argv = install_pam_args_builder(
            "sudo",
            SY_DEFAULT_PAM_CONTROL,
            SY_DEFAULT_PAM_MODULE_ARGS,
            true,
        );
        let args_idx = argv
            .iter()
            .position(|s| s == "--module-args")
            .expect("flag present");
        assert_eq!(argv[args_idx + 1], "timeout=8000");
        assert!(
            !argv.iter().any(|s| s.contains("timeout=1200")),
            "argv must not carry the historical 1200 ms timeout"
        );
    }

    #[test]
    fn install_pam_args_builder_omits_yes_when_not_requested() {
        // CLIG idempotency rule: without an explicit --yes the
        // wrapper leaves the gate to the upstream CLI.
        let argv = install_pam_args_builder(
            "sudo",
            SY_DEFAULT_PAM_CONTROL,
            SY_DEFAULT_PAM_MODULE_ARGS,
            false,
        );
        assert!(!argv.contains(&"--yes".to_string()));
    }

    #[test]
    fn uninstall_pam_args_builder_carries_service_and_yes() {
        let argv = uninstall_pam_args_builder("sudo", true);
        assert_eq!(argv[0], "uninstall-pam");
        let svc_idx = argv
            .iter()
            .position(|s| s == "--service")
            .expect("flag present");
        assert_eq!(argv[svc_idx + 1], "sudo");
        assert!(argv.contains(&"--yes".to_string()));
    }

    // -- Step 4: audit_log_tail (CSV vs ISO filter) ------------------------

    /// Mixed-format fixture mirroring real `/var/lib/syauth/last.log`:
    /// the PAM module appends ISO-timestamp lines (`<rfc3339>
    /// success|denied <peer_id>`); the daemon appends CSV
    /// (`peer_id,nonce,t_start_ms,t_end_ms,outcome,reason`). The
    /// canonical filter (`NF == 6 && $4 ~ /^[0-9]+$/`) is the only way
    /// to keep the notifier from mis-classifying every ISO line as
    /// `elapsed_ms = 0 → instant transport-error`.
    const FIXTURE_AUDIT_MIXED: &str = concat!(
        "2026-05-17T21:39:18.255966214Z failure fbd6cd666d0af720a5db0efd72b47cb5\n",
        "665d53ea76d2b59f1931ba526947b26d,a92eef385f5501ac83ba7e2872aad371,1779136968522,1779136969724,ok,ok\n",
        "2026-05-17T21:40:52.787853771Z failure fbd6cd666d0af720a5db0efd72b47cb5\n",
        "665d53ea76d2b59f1931ba526947b26d,3ab3f5e98772c0c4e10ff7b772f4ca13,1779138669839,1779138669839,bad-signature,bad-signature\n",
    );

    #[test]
    fn audit_log_tail_skips_iso_lines() {
        let outcomes = audit_log_tail(FIXTURE_AUDIT_MIXED, 10);
        // Two CSV rows survive; two ISO rows are dropped.
        assert_eq!(outcomes.len(), 2, "got: {outcomes:?}");
        assert!(
            matches!(outcomes[0].kind, OutcomeKind::Ok),
            "got: {:?}",
            outcomes[0]
        );
        assert!(
            matches!(
                outcomes[1].kind,
                OutcomeKind::Denied(DeniedReason::Other(ref r)) if r == "bad-signature",
            ),
            "got: {:?}",
            outcomes[1]
        );
        // ISO lines must not sneak through as zero-elapsed transport-error.
        assert!(
            !outcomes.iter().any(|o| matches!(
                o.kind,
                OutcomeKind::Denied(DeniedReason::TransportError)
            ) && o.t_start_ms == o.t_end_ms
                && o.peer_id.starts_with("2026-")),
            "ISO line leaked as transport-error: {outcomes:?}"
        );
    }

    #[test]
    fn notify_is_idempotent_per_state() {
        // Feed the dispatcher two identical outcome snapshots; the
        // second call must not produce a notification (the state key
        // matches the cached one). Pair state is unchanged.
        let outcomes = audit_log_tail(FIXTURE_AUDIT_MIXED, 10);
        let mut state = NotifierState::default();
        let mut sent: Vec<String> = Vec::new();
        let mut log: Vec<String> = Vec::new();

        let dispatch_once =
            |state: &mut NotifierState, sent: &mut Vec<String>, log: &mut Vec<String>| {
                notify_dispatcher(
                    state,
                    &PairState::Idle,
                    &outcomes,
                    &mut |body: &str| sent.push(body.to_string()),
                    &mut |line: &str| log.push(line.to_string()),
                );
            };

        dispatch_once(&mut state, &mut sent, &mut log);
        let after_first = sent.len();
        dispatch_once(&mut state, &mut sent, &mut log);
        assert_eq!(
            sent.len(),
            after_first,
            "second identical poll must not re-notify; sent={sent:?}"
        );
        assert_eq!(after_first, 1, "expected one notification; got {sent:?}");
    }

    #[test]
    fn notify_fires_on_pair_completion() {
        // pending → ok transition: one notification, body mentions
        // "paired".
        let outcomes: Vec<UnlockOutcome> = Vec::new();
        let mut state = NotifierState::default();
        let mut sent: Vec<String> = Vec::new();
        let mut log: Vec<String> = Vec::new();
        notify_dispatcher(
            &mut state,
            &PairState::Pending {
                passkey: "000000".into(),
            },
            &outcomes,
            &mut |body: &str| sent.push(body.to_string()),
            &mut |line: &str| log.push(line.to_string()),
        );
        assert_eq!(
            sent.len(),
            1,
            "pending must fire one notification: {sent:?}"
        );
        assert!(
            sent[0].contains("000000"),
            "pending body missing passkey: {sent:?}"
        );
        sent.clear();
        notify_dispatcher(
            &mut state,
            &PairState::Ok {
                host: "fedora".into(),
            },
            &outcomes,
            &mut |body: &str| sent.push(body.to_string()),
            &mut |line: &str| log.push(line.to_string()),
        );
        assert_eq!(
            sent.len(),
            1,
            "pending→ok must fire one notification: {sent:?}"
        );
        assert!(
            sent[0].contains("paired"),
            "ok body missing 'paired': {sent:?}"
        );
        assert!(sent[0].contains("fedora"), "ok body missing host: {sent:?}");
    }

    #[test]
    fn notify_fires_on_unlock_denied() {
        // Only the bad-signature CSV row matters; the notifier must
        // emit one notification carrying the reason verbatim.
        let outcomes = audit_log_tail(FIXTURE_AUDIT_MIXED, 10);
        // Prime the cache to the first (ok) row so only the trailing
        // bad-signature row triggers a notification.
        let mut state = NotifierState {
            last_outcome_key: Some(outcomes[0].state_key()),
            ..Default::default()
        };
        let mut sent: Vec<String> = Vec::new();
        let mut log: Vec<String> = Vec::new();
        notify_dispatcher(
            &mut state,
            &PairState::Idle,
            &outcomes,
            &mut |body: &str| sent.push(body.to_string()),
            &mut |line: &str| log.push(line.to_string()),
        );
        assert_eq!(sent.len(), 1, "denied must fire one notification: {sent:?}");
        assert!(
            sent[0].contains("bad-signature"),
            "denied body must carry reason verbatim: {sent:?}"
        );
        assert!(
            sent[0].contains("denied"),
            "denied body must say 'denied': {sent:?}"
        );
    }

    #[test]
    fn notifier_state_round_trips_through_xdg_cache() {
        // Cache path is `$XDG_STATE_HOME/sy/syauth.last-outcome` with
        // 0700 on the parent dir; save+load round-trips the keys.
        let td = tempfile::tempdir().unwrap();
        let saved = std::env::var_os("XDG_STATE_HOME");
        unsafe {
            std::env::set_var("XDG_STATE_HOME", td.path());
        }
        let state = NotifierState {
            last_pair_key: Some("pending:000000".to_string()),
            last_outcome_key: Some("denied:bad-signature|nonce|123".to_string()),
        };
        save_notifier_state(&state).expect("save");
        let loaded = load_notifier_state();
        unsafe {
            match saved {
                Some(v) => std::env::set_var("XDG_STATE_HOME", v),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
        }
        assert_eq!(loaded, state, "round-trip mismatch");
        // Parent dir exists with mode 0700 (POSIX-only).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = td.path().join("sy");
            let meta = fs::metadata(&dir).unwrap();
            let mode = meta.permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "parent dir mode: {mode:o}");
        }
    }

    #[test]
    fn notify_fallback_logs_when_no_notification_fires() {
        // DoD #2: every transition that didn't notify writes a line
        // to the fallback log. Two identical polls: second poll skips
        // notify-send; the fallback log records the skip.
        let outcomes = audit_log_tail(FIXTURE_AUDIT_MIXED, 10);
        let mut state = NotifierState::default();
        let mut sent: Vec<String> = Vec::new();
        let mut log: Vec<String> = Vec::new();
        notify_dispatcher(
            &mut state,
            &PairState::Idle,
            &outcomes,
            &mut |body: &str| sent.push(body.to_string()),
            &mut |line: &str| log.push(line.to_string()),
        );
        let log_after_first = log.len();
        notify_dispatcher(
            &mut state,
            &PairState::Idle,
            &outcomes,
            &mut |body: &str| sent.push(body.to_string()),
            &mut |line: &str| log.push(line.to_string()),
        );
        assert!(
            log.len() > log_after_first,
            "duplicate poll must append a fallback-log line: {log:?}"
        );
        assert!(
            log.last().unwrap().contains("skipped"),
            "fallback-log entry should mention skip: {log:?}"
        );
    }

    // -- Step 5: doctor probe surface --------------------------------------

    /// Real `syauth doctor --json` output captured 2026-05-19 on the
    /// green host (peer ids anonymised in comments only — the JSON
    /// here is verbatim so the parser is tested against the actual
    /// upstream shape). All probes are OK except `bluez_adapter`
    /// (the upstream probe is stubbed as `"unknown"`).
    const FIXTURE_DOCTOR_FULL_OK: &str = r#"{
  "daemon_socket": "/run/user/1000/syauth/auth.sock",
  "daemon": { "state": "up" },
  "bonds_file": {
    "path": "/var/lib/syauth/bonds.toml",
    "exists": true,
    "count": 1,
    "parseable": true
  },
  "keys": {
    "dir": "/var/lib/syauth/keys",
    "files": [
      { "peer_id": "665d53ea76d2b59f1931ba526947b26d", "mode": "0600", "ok": true }
    ]
  },
  "bluez_adapter": "unknown",
  "systemctl": "active",
  "last_log_tail": [
    "665d53ea76d2b59f1931ba526947b26d,1f5d32f2a4b390fd8333c59ecf6d21a4,1779194389806,1779194392056,ok,ok"
  ],
  "xdg_runtime_dir": { "set": true, "value": "/run/user/1000" },
  "summary": "ok"
}"#;

    const FIXTURE_DOCTOR_FULL_FAIL: &str = r#"{
  "daemon": { "state": "down" },
  "bonds_file": {
    "path": "/var/lib/syauth/bonds.toml",
    "exists": false,
    "count": 0,
    "parseable": false
  },
  "keys": {
    "dir": "/var/lib/syauth/keys",
    "files": [
      { "peer_id": "abc", "mode": "0644", "ok": false }
    ]
  },
  "bluez_adapter": "unknown",
  "systemctl": "inactive",
  "last_log_tail": []
}"#;

    /// Sample PAM stack with pam_syauth.so wired in at the top (sudo
    /// canonical shape — see roadmap step 3).
    const FIXTURE_PAM_SUDO_WIRED: &str = "\
auth        sufficient    pam_syauth.so timeout=8000
auth        required      pam_unix.so try_first_pass
account     required      pam_unix.so
";

    /// Sample PAM stack with no pam_syauth.so line.
    const FIXTURE_PAM_SUDO_UNWIRED: &str = "\
auth        required      pam_unix.so try_first_pass
account     required      pam_unix.so
";

    #[test]
    fn contains_pam_syauth_line_detects_wired_stack() {
        assert!(contains_pam_syauth_line(FIXTURE_PAM_SUDO_WIRED));
        assert!(!contains_pam_syauth_line(FIXTURE_PAM_SUDO_UNWIRED));
        // Commented-out lines must not count as wired — the operator
        // could have left a stale `# auth sufficient pam_syauth.so`
        // after an uninstall; that's not a live wire.
        assert!(!contains_pam_syauth_line(
            "# auth sufficient pam_syauth.so\n"
        ));
    }

    #[test]
    fn doctor_aggregates_check_results() {
        // All-OK doctor JSON + present-and-wired PAM probes → exit 0.
        let lines = build_doctor_lines(
            FIXTURE_DOCTOR_FULL_OK,
            /*pam_so_present=*/ true,
            /*pam_sudo_contents=*/ Some(FIXTURE_PAM_SUDO_WIRED),
        )
        .expect("fixture parses");
        // bluez_adapter == "unknown" is WARN, not FAIL.
        assert_eq!(doctor_exit_code(&lines), 2, "lines={lines:?}");

        // Failure fixture: daemon down, bonds missing, keys not 0600,
        // systemctl inactive, last_log_tail empty, PAM missing+unwired.
        let lines = build_doctor_lines(
            FIXTURE_DOCTOR_FULL_FAIL,
            false,
            Some(FIXTURE_PAM_SUDO_UNWIRED),
        )
        .expect("fixture parses");
        assert_eq!(doctor_exit_code(&lines), 1, "lines={lines:?}");
    }

    #[test]
    fn doctor_emits_one_line_per_check() {
        let lines = build_doctor_lines(FIXTURE_DOCTOR_FULL_OK, true, Some(FIXTURE_PAM_SUDO_WIRED))
            .expect("fixture parses");
        // One line per probe (8 total: daemon, bonds_file, keys,
        // bluez_adapter, systemctl, last_log_tail, pam_so_present,
        // pam_so_wired).
        assert_eq!(lines.len(), 8, "probes: {lines:?}");
        // Order is deterministic — the same fixture twice must yield
        // identical output.
        let again = build_doctor_lines(FIXTURE_DOCTOR_FULL_OK, true, Some(FIXTURE_PAM_SUDO_WIRED))
            .expect("fixture parses");
        assert_eq!(
            render_doctor_lines(&lines),
            render_doctor_lines(&again),
            "render must be deterministic"
        );
        // Format: `key=<name> status=<ok|warn|fail> hint="..."` —
        // every rendered line carries all three fields.
        for line in render_doctor_lines(&lines).lines() {
            assert!(line.starts_with("key="), "line={line:?}");
            assert!(line.contains(" status="), "line={line:?}");
            assert!(line.contains(" hint=\""), "line={line:?}");
        }
    }

    #[test]
    fn doctor_keys_probe_fails_when_any_file_not_ok() {
        // Single bad key file flips the keys probe to FAIL even when
        // every other file is mode 0600.
        let src = r#"{
  "daemon": { "state": "up" },
  "bonds_file": { "path": "x", "exists": true, "count": 1, "parseable": true },
  "keys": {
    "dir": "/var/lib/syauth/keys",
    "files": [
      { "peer_id": "good", "mode": "0600", "ok": true },
      { "peer_id": "bad", "mode": "0644", "ok": false }
    ]
  },
  "bluez_adapter": "ok",
  "systemctl": "active",
  "last_log_tail": ["x,y,1,2,ok,ok"]
}"#;
        let lines =
            build_doctor_lines(src, true, Some(FIXTURE_PAM_SUDO_WIRED)).expect("fixture parses");
        let keys = lines.iter().find(|l| l.key == "keys").expect("keys probe");
        assert_eq!(keys.status, ProbeStatus::Fail, "got: {keys:?}");
    }

    #[test]
    fn waybar_rejects_unknown_schema_version() {
        let _guard = XDG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let saved = std::env::var_os("XDG_RUNTIME_DIR");
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join(IPC_SUBDIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(REQUEST_FILE),
            br#"{"schema_version":99,"kind":"pair_confirm","request_id":"x","passkey":"000000","created_at_secs":0}"#,
        )
        .unwrap();
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", td.path());
        }
        assert!(read_request().is_none());
        unsafe {
            match saved {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
    }
}
