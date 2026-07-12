//! `sy power status` renderer — Step 11.
//!
//! Pure-function transformer from the daemon's wire-shape
//! [`StatusResponse`] (Step 10) to the SPEC §4 `sy.power.status/v1`
//! schema. The CLI handler (Step 11) calls [`format_status`] after
//! dialing the IPC socket; tests exercise the same function without
//! a socket so the schema contract stays pinned to a pure call.
//!
//! Values for fields R1 does not yet populate (bandit, shield, drift,
//! applied policy) are conservative stubs documented inline. Later
//! steps fill them in-place: Step 13 (applied policy), Step 17
//! (shield), Step 19 (bandit), Step 26 (drift).

use serde_json::{json, Value};

use super::config::PowerConfig;
use super::ipc::{StatusResponse, STATUS_SCHEMA};
use super::log::AuditEntry;
use super::onboarding::OnboardingStatus;
use super::shield::ShieldState;

/// Resolve the onboarding view the status document should render.
///
/// BUG-20260712-1530: the onboarding gate is the daemon's, so its
/// self-reported view (`resp.onboarding`) is authoritative whenever
/// present — it reflects the `SY_POWER_ONBOARDING_DAYS` the *daemon*
/// loaded, which can differ from the CLI process's (a systemd drop-in
/// scoping the env to `sy-powerd` only made `sy power status` lie about
/// the live gate). When the field is absent — a pre-fix daemon — fall
/// back to the CLI's local computation (`local` + `cfg.onboarding.days`)
/// so old ↔ new wire frames stay compatible in both directions.
///
/// Returns the resolved `(OnboardingStatus, target_days)` pair; every
/// key of the printed `onboarding` block derives from it, so the JSON
/// schema shape is unchanged regardless of which branch fires.
fn effective_onboarding(
    resp: &StatusResponse,
    cfg: &PowerConfig,
    local: &OnboardingStatus,
) -> (OnboardingStatus, u32) {
    match resp.onboarding.as_ref() {
        Some(w) => (
            OnboardingStatus {
                active: w.active,
                days_collected: w.days_collected,
                ready_at: w.ready_at,
            },
            w.target_days,
        ),
        None => (local.clone(), cfg.onboarding.days),
    }
}

/// Versioned schema id stamped on every `sy power explain --json`
/// document. Distinct from `STATUS_SCHEMA` so consumers can branch on
/// the historical-replay shape without sniffing field presence.
pub const EXPLAIN_SCHEMA: &str = "sy.power.explain/v1";

/// Empty-list sentinel printed by the human form when no audit entries
/// match. The SPEC §4 anti-goal #4 ("no black-box decisions") explicitly
/// requires a textual answer even when the daemon hasn't ticked yet.
const EXPLAIN_NO_ENTRIES_HUMAN: &str = "no audit entries yet\n";

/// Baseline "pre-training" model marker. Mirrors the constant in
/// [`super::cli`] — Step 25's offline trainer replaces this with the
/// SHA of the first user-personal ONNX.
const RULES_BASELINE_VERSION: &str = "rules-baseline";

/// R1 stub for the activity classifier label. Step 28 wires the
/// linfa-ftrl classifier and overwrites this slot.
const STUB_ACTIVITY_LABEL: &str = "idle";

/// Build the SPEC §4 `sy.power.status/v1` document from a wire
/// response. Sensors + `ts` come from the live snapshot; bandit
/// `conservative_alpha` and onboarding `target_days` from the config;
/// `shield_state` from the live Step 17 DFA result the caller supplies;
/// `applied_policy` from `resp.last_audit` (Step 19) when the daemon
/// has ticked at least once; the rest are conservative stubs
/// documented at the constants above.
pub fn build_status_value(
    resp: &StatusResponse,
    cfg: &PowerConfig,
    shield_state: ShieldState,
    onboarding: &OnboardingStatus,
) -> Value {
    let snap = &resp.snapshot;
    let raw = snap.get("raw").cloned().unwrap_or(Value::Null);
    let ts = snap
        .get("ts")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    let tctl_c = raw.get("tctl_c").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let pkg_w = raw
        .get("package_power_w")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let igpu_busy_pct = raw
        .get("igpu_busy_pct")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let npu_workloads = raw
        .get("npu_workloads")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let battery_pct = raw
        .get("battery_soc_pct")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let ac = raw
        .get("ac_online")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    // BUG-20260712-1530: prefer the daemon's self-reported onboarding
    // gate over the CLI-side re-computation when the daemon supplies it.
    let (onboarding, target_days) = effective_onboarding(resp, cfg, onboarding);
    let onboarding = &onboarding;
    let applied_policy = build_applied_policy(resp, cfg);
    let bandit_block = build_bandit_block(resp, cfg, shield_state);
    let model_version = if onboarding.active {
        RULES_BASELINE_VERSION.to_string()
    } else {
        resp.last_audit
            .as_ref()
            .map(|_| RULES_BASELINE_VERSION.to_string())
            .unwrap_or_else(|| RULES_BASELINE_VERSION.to_string())
    };
    // Step T3 (BUG-20260525-2352): `model.missing_classes` is the
    // operator's affordance for noticing the trainer refused to ship
    // a degenerate model. `null` means the last retrain succeeded or
    // hasn't fired; a populated list names the classes whose row
    // counts fell below the trainer's per-class floor.
    let missing_classes = resp
        .model
        .as_ref()
        .map(|m| json!(m.missing_classes))
        .unwrap_or(Value::Null);
    json!({
        "schema": STATUS_SCHEMA,
        "ts": ts,
        "onboarding": {
            "active": onboarding.active,
            "days_collected": onboarding.days_collected,
            "ready_at": onboarding.ready_at.to_rfc3339(),
            "target_days": target_days,
        },
        "model": {
            "version_sha": model_version,
            "loaded_at": null,
            "params": 0,
            "missing_classes": missing_classes,
        },
        "shield_state": shield_state.as_str(),
        "activity_label": STUB_ACTIVITY_LABEL,
        "forecast": {
            "horizon_s": 60,
            "next_activity": {
                "build": 0.0,
                "code": 0.0,
                "idle": 1.0,
                "call": 0.0,
            },
        },
        "bandit": bandit_block,
        "applied_policy": applied_policy,
        "sensors": {
            "package_power_w_5tap": pkg_w,
            "tctl_c": tctl_c,
            "igpu_busy_pct": igpu_busy_pct,
            "npu_workloads": npu_workloads,
            "battery_pct": battery_pct,
            "ac": ac,
        },
        "drift": build_drift_block(&resp.drift),
        "snapshot_hash": resp.snapshot_hash,
    })
}

/// Build the SPEC §4 `sy.power.status/v1` `bandit` block. Step 11
/// shipped a stub; Step 22 populates every key from the most-recent
/// audit entry's `ranked_actions` + `conservative_alpha` + the rules
/// baseline that anchored the decision. Keys are mandatory — every
/// snapshot must carry every key so the daemon-in-CI smoke test can
/// pin the schema.
fn build_bandit_block(
    resp: &StatusResponse,
    cfg: &PowerConfig,
    shield_state: ShieldState,
) -> Value {
    let baseline_arm =
        crate::power::policy::rules_baseline(shield_state, &snapshot_anchor(), &cfg.rules_baseline)
            .to_string();
    let Some(entry) = resp.last_audit.as_ref() else {
        return json!({
            "chosen_arm": RULES_BASELINE_VERSION,
            "ucb_score": 0.0,
            "top3": [],
            "conservative_alpha": cfg.bandit.alpha,
            "baseline_arm": baseline_arm,
        });
    };
    let top3: Vec<Value> = entry
        .ranked_actions
        .iter()
        .take(3)
        .map(|(n, s)| json!([n, s]))
        .collect();
    let chosen_arm = entry
        .applied_arm
        .clone()
        .unwrap_or_else(|| RULES_BASELINE_VERSION.to_string());
    let ucb_score = entry
        .ranked_actions
        .first()
        .map(|(_, s)| *s as f64)
        .unwrap_or(0.0);
    let alpha = if entry.conservative_alpha > 0.0 {
        entry.conservative_alpha as f64
    } else {
        cfg.bandit.alpha
    };
    json!({
        "chosen_arm": chosen_arm,
        "ucb_score": ucb_score,
        "top3": top3,
        "conservative_alpha": alpha,
        "baseline_arm": baseline_arm,
    })
}

/// Build the SPEC §4 `drift` block. Step 31 hydrates this from the
/// daemon's [`crate::power::drift::DriftStatus`] (carried over IPC
/// in [`StatusResponse::drift`]); the `last_alarm_at` slot is
/// `null` until the first alarm fires.
fn build_drift_block(d: &crate::power::drift::DriftStatus) -> Value {
    json!({
        "adwin_alarm": d.adwin_alarm,
        "ddm_warning": d.ddm_warning,
        "last_alarm_at": d.last_alarm_at.map(|t| t.to_rfc3339()),
    })
}

/// Zero-valued snapshot fed to `rules_baseline` when the daemon
/// hasn't ticked yet. The rules-baseline table is currently
/// state-only (snapshot fields go unused per the Step 18 module
/// docs), but the signature still requires a value.
fn snapshot_anchor() -> crate::power::snapshot::Snapshot {
    use crate::power::snapshot::{Snapshot, SnapshotRaw, FEATURE_LEN, SCHEMA_ID};
    Snapshot {
        schema: SCHEMA_ID,
        ts: chrono::Utc::now(),
        features: [0.0; FEATURE_LEN],
        raw: SnapshotRaw::default(),
        snapshot_hash: String::new(),
    }
}

/// Build the `applied_policy` block from the wire response's
/// `last_audit` slot. When the daemon hasn't ticked yet (the slot is
/// `None`), fall back to the vendor-default tuple — the same values
/// the SPEC §4 NFR Reliability exit handler would have left behind.
fn build_applied_policy(resp: &StatusResponse, cfg: &PowerConfig) -> Value {
    let Some(entry) = resp.last_audit.as_ref() else {
        return json!({
            "platform_profile": "balanced",
            "epp": "balance_performance",
            "igpu_mode": "POWER_SAVING",
            "npu_pmode": "powersaver",
            "cgroup": { "cpu_uclamp_min": 0 },
            "arm": null,
            "reason_chain": [],
        });
    };
    let arm = entry
        .applied_arm
        .as_deref()
        .and_then(|name| cfg.arms.iter().find(|a| a.name == name));
    let pp = arm
        .map(|a| serde_json::to_value(&a.platform_profile).unwrap_or(Value::Null))
        .unwrap_or(Value::Null);
    let epp = arm
        .map(|a| serde_json::to_value(a.epp).unwrap_or(Value::Null))
        .unwrap_or(Value::Null);
    let igpu = arm
        .map(|a| serde_json::to_value(&a.igpu_mode).unwrap_or(Value::Null))
        .unwrap_or(Value::Null);
    let npu = arm
        .map(|a| serde_json::to_value(a.npu_pmode).unwrap_or(Value::Null))
        .unwrap_or(Value::Null);
    let cgroup = arm
        .map(|a| serde_json::to_value(&a.cgroup).unwrap_or(Value::Null))
        .unwrap_or(Value::Null);
    json!({
        "platform_profile": pp,
        "epp": epp,
        "igpu_mode": igpu,
        "npu_pmode": npu,
        "cgroup": cgroup,
        "arm": entry.applied_arm,
        "reason_chain": entry.reason_chain,
    })
}

/// Render the SPEC §4 status: pretty JSON when `json_out`, else a
/// single-line human summary (`bandit:<arm> · shield:<state> · …`).
pub fn format_status(
    resp: &StatusResponse,
    cfg: &PowerConfig,
    shield_state: ShieldState,
    onboarding: &OnboardingStatus,
    json_out: bool,
) -> String {
    let v = build_status_value(resp, cfg, shield_state, onboarding);
    if json_out {
        return serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string());
    }
    let pkg = v["sensors"]["package_power_w_5tap"].as_f64().unwrap_or(0.0);
    let tctl = v["sensors"]["tctl_c"].as_f64().unwrap_or(0.0);
    let batt = v["sensors"]["battery_pct"].as_u64().unwrap_or(0);
    let ac = if v["sensors"]["ac"].as_bool().unwrap_or(true) {
        "AC"
    } else {
        "DC"
    };
    let arm = v["bandit"]["chosen_arm"].as_str().unwrap_or("?");
    let shield = v["shield_state"].as_str().unwrap_or("?");
    format!("bandit:{arm} · shield:{shield} · {pkg:.1} W · {tctl:.0} °C · {batt} % {ac}")
}

/// Render the SPEC §5 waybar pill: a one-line JSON envelope
/// `{"text": …, "tooltip": …, "class": …}` that waybar's
/// `custom/sy-power` slot consumes (Step 32). The class follows the
/// documented precedence (drift > meeting > onboarding > rules >
/// bandit); the text is class-specific; the tooltip is a four-line
/// arm/shield/ucb/baseline summary. Pure-fn — the CLI handler
/// (`cli::status`) wraps the daemon-down case before calling here.
pub fn format_waybar(
    resp: &StatusResponse,
    cfg: &PowerConfig,
    shield: ShieldState,
    onboarding: &OnboardingStatus,
) -> String {
    // BUG-20260712-1530: the waybar `onboarding` class + "learning Xd
    // Yh" countdown must track the daemon's live gate, not the bar
    // process's own re-computation.
    let (onboarding, target_days) = effective_onboarding(resp, cfg, onboarding);
    let onboarding = &onboarding;
    let arm = applied_arm_or_baseline(resp, cfg, shield);
    let baseline =
        crate::power::policy::rules_baseline(shield, &snapshot_anchor(), &cfg.rules_baseline)
            .to_string();
    let ucb = resp
        .last_audit
        .as_ref()
        .and_then(|e| e.ranked_actions.first())
        .map(|(_, s)| *s as f64)
        .unwrap_or(0.0);
    let class = waybar_class(&resp.drift, shield, onboarding, &arm, &baseline);
    let text = waybar_text(class, &arm, ucb, onboarding, target_days);
    let tooltip = format!(
        "arm: {arm}\nshield: {shield_s}\nucb: {ucb:.2}\nbaseline: {baseline}",
        shield_s = shield.as_str(),
    );
    emit_waybar_json(&text, &tooltip, class)
}

/// Stable class id for the five visual states surfaced by the
/// `custom/sy-power` waybar slot. Mapped 1:1 to the `.custom-sy-power.*`
/// hooks in `configs/waybar/style.css`. The string form is the same
/// token both serialised in the waybar JSON and matched on by the
/// stylesheet — keep them in lockstep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaybarClass {
    Drift,
    Meeting,
    Onboarding,
    Rules,
    Bandit,
    Error,
}

impl WaybarClass {
    fn as_str(self) -> &'static str {
        match self {
            WaybarClass::Drift => "drift",
            WaybarClass::Meeting => "meeting",
            WaybarClass::Onboarding => "onboarding",
            WaybarClass::Rules => "rules",
            WaybarClass::Bandit => "bandit",
            WaybarClass::Error => "error",
        }
    }
}

/// Decide the waybar class per the Step 32 precedence table:
/// `drift > meeting > onboarding > rules > bandit`. Pure-fn so the
/// `waybar_class_*` tests can pin every transition without touching
/// the JSON serialiser.
fn waybar_class(
    drift: &crate::power::drift::DriftStatus,
    shield: ShieldState,
    onboarding: &OnboardingStatus,
    arm: &str,
    baseline: &str,
) -> WaybarClass {
    if drift.adwin_alarm {
        return WaybarClass::Drift;
    }
    if shield == ShieldState::Meeting {
        return WaybarClass::Meeting;
    }
    if onboarding.active {
        return WaybarClass::Onboarding;
    }
    if arm == baseline {
        WaybarClass::Rules
    } else {
        WaybarClass::Bandit
    }
}

/// Render the per-class text shape documented in the Step 32
/// implementation guidance. The onboarding form surfaces the
/// days-remaining countdown (target_days − days_collected) plus the
/// residual hours so the operator can answer "when does the ML kick
/// in" without opening `--json`.
fn waybar_text(
    class: WaybarClass,
    arm: &str,
    ucb: f64,
    onboarding: &OnboardingStatus,
    target_days: u32,
) -> String {
    match class {
        WaybarClass::Drift => "sy: retraining".to_string(),
        WaybarClass::Meeting => "sy: call".to_string(),
        WaybarClass::Onboarding => {
            let target = target_days as i64;
            let collected = onboarding.days_collected as i64;
            let days_left = (target - collected).max(0);
            let hours_left = onboarding_hours_remainder(onboarding);
            format!("sy: learning {days_left}d {hours_left}h")
        }
        WaybarClass::Rules => format!("sy: {arm}"),
        WaybarClass::Bandit => format!("sy: {arm} ({ucb:.2})"),
        WaybarClass::Error => "sy: -".to_string(),
    }
}

/// Hour-of-day residual between `now` and `onboarding.ready_at`.
/// Caps at 0 so a clock-skew event that lands `ready_at` in the past
/// doesn't render a negative `Xh` in the bar pill.
fn onboarding_hours_remainder(onboarding: &OnboardingStatus) -> i64 {
    let delta = onboarding.ready_at - chrono::Utc::now();
    let total_hours = delta.num_hours().max(0);
    total_hours % 24
}

/// "learning Xd Yh" hint for callers outside the `custom/sy-power`
/// pill — currently the `custom/pwr` (perf) tooltip in `src/pwr.rs`.
/// Returns `None` once onboarding is complete so the tooltip drops
/// the line cleanly without per-caller arithmetic. Pure-fn so a unit
/// test can pin both branches without filesystem state.
pub fn onboarding_hint(onboarding: &OnboardingStatus, cfg: &PowerConfig) -> Option<String> {
    if !onboarding.active {
        return None;
    }
    let target = cfg.onboarding.days as i64;
    let collected = onboarding.days_collected as i64;
    let days_left = (target - collected).max(0);
    let hours_left = onboarding_hours_remainder(onboarding);
    Some(format!("learning {days_left}d {hours_left}h"))
}

/// Convenience wrapper: read the config + compute onboarding from
/// the live filesystem state, then defer to [`onboarding_hint`].
/// Returns `None` on any IO / parse failure so a caller in `pwr` can
/// fall back to a tooltip without the hint instead of breaking the
/// bar. Filesystem-only: no IPC to `sy-powerd`, so the cost is one
/// directory read + one TOML parse.
pub fn live_onboarding_hint() -> Option<String> {
    let cfg_path = crate::power::power_config_path();
    let cfg = PowerConfig::load(&cfg_path).ok()?;
    let state_dir = crate::power::power_state_dir_for_daemon();
    let anchor = crate::power::checkpoint::read_anchor(&state_dir.join("checkpoint.json"));
    let onboarding = crate::power::onboarding::compute_onboarding_status(
        &state_dir,
        &crate::power::clock::SystemClock,
        cfg.onboarding.days,
        anchor,
    );
    onboarding_hint(&onboarding, &cfg)
}

/// Daemon-down soft-error envelope. The Step 32 implementation
/// guidance pins this shape — waybar keeps polling at the 1 s
/// interval, so the CLI must emit a parseable JSON object instead
/// of breaking the bar with a missing/empty stdout. The CLI handler
/// (`cli::status` under `--waybar`) calls this when the IPC dial
/// fails.
pub fn format_waybar_daemon_down() -> String {
    emit_waybar_json("sy: -", "daemon down", WaybarClass::Error)
}

/// Serialise the waybar pill into the documented
/// `{"text": …, "tooltip": …, "class": …}` envelope on one line.
/// Hand-rolled (not `serde_json::to_string`) so newlines inside the
/// tooltip are emitted as literal `\n` escape sequences without a
/// separate `serde_json::Value` round-trip — waybar's parser
/// accepts the same escape shape used by `src/syauth.rs::emit_pill_json`.
fn emit_waybar_json(text: &str, tooltip: &str, class: WaybarClass) -> String {
    fn esc(s: &str) -> String {
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    }
    format!(
        r#"{{"text":"{t}","class":"{c}","tooltip":"{tip}"}}"#,
        t = esc(text),
        c = class.as_str(),
        tip = esc(tooltip),
    )
}

/// Resolve the bandit's currently-applied arm. Falls back to the
/// rules baseline when the daemon hasn't ticked yet (no `last_audit`)
/// or the audit entry is missing an `applied_arm` slot. Mirrors the
/// fallback contract in `build_bandit_block`.
fn applied_arm_or_baseline(
    resp: &StatusResponse,
    cfg: &PowerConfig,
    shield: ShieldState,
) -> String {
    if let Some(entry) = resp.last_audit.as_ref() {
        if let Some(name) = entry.applied_arm.as_deref() {
            return name.to_string();
        }
    }
    crate::power::policy::rules_baseline(shield, &snapshot_anchor(), &cfg.rules_baseline)
        .to_string()
}

/// Render the SPEC §4 `sy power explain` output — historical replay of
/// the last N audit-log decisions. JSON form emits the
/// [`EXPLAIN_SCHEMA`] envelope with one object per entry; human form
/// renders a single paragraph per entry summarising shield state,
/// rules baseline, bandit pick, top-3 candidates, applied arm, and the
/// stored reason chain. SPEC §3 anti-goal #4 ("no black-box
/// decisions") lands here.
pub fn format_explain(entries: &[AuditEntry], cfg: &PowerConfig, json_out: bool) -> String {
    if json_out {
        let docs: Vec<Value> = entries.iter().map(|e| explain_entry_json(e, cfg)).collect();
        let envelope = json!({ "schema": EXPLAIN_SCHEMA, "entries": docs });
        return serde_json::to_string_pretty(&envelope).unwrap_or_else(|_| "{}".to_string());
    }
    if entries.is_empty() {
        return EXPLAIN_NO_ENTRIES_HUMAN.to_string();
    }
    let mut out = String::new();
    for entry in entries {
        out.push_str(&explain_entry_human(entry, cfg));
        out.push('\n');
    }
    out
}

/// JSON shape per entry — mirrors the `sy.power.status/v1` bandit +
/// applied-policy blocks plus an explicit historical-context header
/// (`ts`, `snapshot_hash`).
fn explain_entry_json(entry: &AuditEntry, cfg: &PowerConfig) -> Value {
    let shield_str = entry.shield_state.clone().unwrap_or_default();
    let baseline_arm = baseline_for_entry(entry, cfg);
    let top3: Vec<Value> = entry
        .ranked_actions
        .iter()
        .take(3)
        .map(|(n, s)| json!([n, s]))
        .collect();
    let ucb_score = entry
        .ranked_actions
        .first()
        .map(|(_, s)| *s as f64)
        .unwrap_or(0.0);
    json!({
        "ts": entry.snapshot.ts.to_rfc3339(),
        "snapshot_hash": entry.snapshot.snapshot_hash,
        "shield_state": shield_str,
        "applied_arm": entry.applied_arm,
        "ucb_score": ucb_score,
        "ranked_actions": top3,
        "baseline_arm": baseline_arm,
        "conservative_alpha": entry.conservative_alpha as f64,
        "reason_chain": entry.reason_chain,
    })
}

/// Compute the rules-baseline arm that *would* have fired for this
/// historical entry, using the recorded `shield_state` string. Falls
/// back to `ShieldState::CoolAc` when the recorded tag is missing or
/// unknown (forward-compat with older NDJSON).
fn baseline_for_entry<'a>(entry: &AuditEntry, cfg: &'a PowerConfig) -> &'a str {
    let state = entry
        .shield_state
        .as_deref()
        .and_then(ShieldState::parse)
        .unwrap_or(ShieldState::CoolAc);
    crate::power::policy::rules_baseline(state, &entry.snapshot, &cfg.rules_baseline)
}

/// One-paragraph human render per entry. Mentions the rules baseline
/// alongside the bandit's pick whenever they disagree; the explicit
/// "baseline says X, bandit picked Y" phrasing is what SPEC §3 anti-
/// goal #4 promises the user.
fn explain_entry_human(entry: &AuditEntry, cfg: &PowerConfig) -> String {
    let ts = entry.snapshot.ts.to_rfc3339();
    let shield = entry.shield_state.as_deref().unwrap_or("-");
    let baseline = baseline_for_entry(entry, cfg);
    let chosen = entry.applied_arm.as_deref().unwrap_or("-");
    let ucb_score = entry
        .ranked_actions
        .first()
        .map(|(_, s)| *s as f64)
        .unwrap_or(0.0);
    let top3: String = entry
        .ranked_actions
        .iter()
        .take(3)
        .map(|(n, s)| format!("{n}:{s:.2}"))
        .collect::<Vec<_>>()
        .join(", ");
    let applied = applied_arm_human(entry, cfg);
    let reason = if entry.reason_chain.is_empty() {
        "-".to_string()
    } else {
        entry.reason_chain.join(" → ")
    };
    let baseline_clause = if baseline != chosen {
        format!("baseline says {baseline}, bandit picked {chosen}")
    } else {
        format!("baseline={baseline}")
    };
    format!(
        "{ts} · shield={shield} · {baseline_clause} (ucb={ucb_score:.2}) \
         — top3 [{top3}] — applied {applied} — reason: {reason}",
    )
}

/// Render the applied arm's actuator tuple in the canonical human
/// shape `platform_profile=… epp=… igpu=… npu=…`. Used by the human
/// explain output so the operator can grep for the actual values the
/// daemon wrote that tick. Missing-arm rows render as `-` so an audit
/// entry that pre-dates Step 19 still produces a single readable line.
fn applied_arm_human(entry: &AuditEntry, cfg: &PowerConfig) -> String {
    let Some(name) = entry.applied_arm.as_deref() else {
        return "-".to_string();
    };
    let Some(arm) = cfg.arms.iter().find(|a| a.name == name) else {
        return "-".to_string();
    };
    let pp = serde_json::to_value(&arm.platform_profile)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "?".to_string());
    let epp = serde_json::to_value(arm.epp)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "?".to_string());
    let igpu = serde_json::to_value(&arm.igpu_mode)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "?".to_string());
    let npu = serde_json::to_value(arm.npu_pmode)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "?".to_string());
    format!("platform_profile={pp} epp={epp} igpu={igpu} npu={npu}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::ipc::StatusResponse;

    /// SPEC §4 `sy.power.status/v1` top-level required keys. Step 11's
    /// schema contract — every later step that extends the shape must
    /// keep these.
    const REQUIRED_KEYS: &[&str] = &[
        "schema",
        "ts",
        "onboarding",
        "model",
        "shield_state",
        "activity_label",
        "forecast",
        "bandit",
        "applied_policy",
        "sensors",
        "drift",
    ];

    fn empty_response() -> StatusResponse {
        StatusResponse {
            schema: STATUS_SCHEMA.to_string(),
            snapshot_hash: "0".repeat(64),
            snapshot: serde_json::json!({}),
            last_audit: None,
            drift: crate::power::drift::DriftStatus::default(),
            model: None,
            onboarding: None,
        }
    }

    /// Pre-Step-26 tests assumed the onboarding window was active
    /// with zero days collected. Centralise that fixture here so the
    /// Step 26 wire-up doesn't require restamping every test.
    fn default_onboarding() -> OnboardingStatus {
        OnboardingStatus {
            active: true,
            days_collected: 0,
            ready_at: chrono::Utc::now() + chrono::Duration::days(14),
        }
    }

    #[test]
    fn renders_schema_v1_required_keys() {
        let cfg = PowerConfig::default();
        let resp = empty_response();
        let out = format_status(
            &resp,
            &cfg,
            ShieldState::CoolAc,
            &default_onboarding(),
            true,
        );
        let v: Value = serde_json::from_str(&out).expect("json round-trip");
        for key in REQUIRED_KEYS {
            assert!(v.get(key).is_some(), "missing top-level key {key}");
        }
        assert_eq!(v["schema"], STATUS_SCHEMA);
        assert_eq!(
            v["bandit"]["conservative_alpha"].as_f64().unwrap(),
            crate::power::config::DEFAULT_BANDIT_ALPHA,
        );
        assert_eq!(
            v["onboarding"]["target_days"].as_u64().unwrap(),
            crate::power::config::DEFAULT_ONBOARDING_DAYS as u64,
        );
    }

    #[test]
    fn human_format_includes_shield_state() {
        let cfg = PowerConfig::default();
        let resp = empty_response();
        let out = format_status(&resp, &cfg, ShieldState::Hot, &default_onboarding(), false);
        assert!(
            out.contains(ShieldState::Hot.as_str()),
            "human format must include shield_state {:?}, got: {out}",
            ShieldState::Hot.as_str()
        );
    }

    /// Sensor values from the live snapshot flow through unchanged.
    /// Catches any regression where `raw.<field>` lookups silently fall
    /// back to the zero default.
    #[test]
    fn sensors_flow_through_from_snapshot_raw() {
        let cfg = PowerConfig::default();
        let resp = StatusResponse {
            schema: STATUS_SCHEMA.to_string(),
            snapshot_hash: "deadbeef".to_string(),
            last_audit: None,
            snapshot: serde_json::json!({
                "ts": "2026-05-19T12:00:00Z",
                "raw": {
                    "tctl_c": 71.5,
                    "package_power_w": 27.4,
                    "igpu_busy_pct": 4,
                    "npu_workloads": 0,
                    "battery_soc_pct": 100,
                    "ac_online": true,
                },
            }),
            drift: crate::power::drift::DriftStatus::default(),
            model: None,
            onboarding: None,
        };
        let v = build_status_value(&resp, &cfg, ShieldState::CoolAc, &default_onboarding());
        assert!((v["sensors"]["package_power_w_5tap"].as_f64().unwrap() - 27.4).abs() < 1e-3);
        assert!((v["sensors"]["tctl_c"].as_f64().unwrap() - 71.5).abs() < 1e-3);
        assert_eq!(v["sensors"]["igpu_busy_pct"].as_u64().unwrap(), 4);
        assert_eq!(v["sensors"]["battery_pct"].as_u64().unwrap(), 100);
        assert!(v["sensors"]["ac"].as_bool().unwrap());
    }

    /// Step 19 wire-up: when the daemon's `last_audit` slot carries
    /// an entry, `applied_policy.arm` mirrors it and the bandit's
    /// `chosen_arm` slot is updated to the same name. The `top-level
    /// reason_chain` from the audit entry flows through unchanged.
    #[test]
    fn applied_policy_reflects_last_audit_entry() {
        use crate::power::log::{AuditEntry, SCHEMA_ID as AUDIT_SCHEMA};
        use crate::power::snapshot::{
            Snapshot, SnapshotRaw, FEATURE_LEN, SCHEMA_ID as SNAP_SCHEMA,
        };
        use chrono::TimeZone;
        let cfg_path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/power.toml");
        let cfg = PowerConfig::load(&cfg_path).expect("shipped power.toml parses");
        let entry = AuditEntry {
            schema: AUDIT_SCHEMA,
            snapshot: Snapshot {
                schema: SNAP_SCHEMA,
                ts: chrono::Utc
                    .with_ymd_and_hms(2026, 5, 19, 12, 0, 0)
                    .single()
                    .expect("pinned UTC"),
                features: [0.0_f32; FEATURE_LEN],
                raw: SnapshotRaw::default(),
                snapshot_hash: "0".repeat(64),
            },
            applied_arm: Some("build".to_string()),
            shield_state: Some("COOL_AC".to_string()),
            reason_chain: vec!["baseline:build".to_string(), "shield:COOL_AC".to_string()],
            ranked_actions: Vec::new(),
            conservative_alpha: 0.0,
        };
        let resp = StatusResponse {
            schema: STATUS_SCHEMA.to_string(),
            snapshot_hash: "0".repeat(64),
            snapshot: serde_json::json!({}),
            last_audit: Some(entry),
            drift: crate::power::drift::DriftStatus::default(),
            model: None,
            onboarding: None,
        };
        let v = build_status_value(&resp, &cfg, ShieldState::CoolAc, &default_onboarding());
        assert_eq!(v["applied_policy"]["arm"].as_str(), Some("build"));
        assert_eq!(v["bandit"]["chosen_arm"].as_str(), Some("build"));
        // The `build` arm in the shipped config has platform=performance.
        assert_eq!(v["applied_policy"]["platform_profile"], "performance");
        let chain = v["applied_policy"]["reason_chain"]
            .as_array()
            .expect("chain");
        assert!(chain.iter().any(|s| s == "baseline:build"));
    }

    /// Build a Step 23 fixture audit entry with the given ranked
    /// actions + applied arm + shield state. Keeps the three explain
    /// tests below from each re-stamping the snapshot struct literal.
    fn explain_fixture(
        applied_arm: &str,
        shield_state: &str,
        ranked: &[(&str, f32)],
    ) -> crate::power::log::AuditEntry {
        use crate::power::log::{AuditEntry, SCHEMA_ID as AUDIT_SCHEMA};
        use crate::power::snapshot::{
            Snapshot, SnapshotRaw, FEATURE_LEN, SCHEMA_ID as SNAP_SCHEMA,
        };
        use chrono::TimeZone;
        AuditEntry {
            schema: AUDIT_SCHEMA,
            snapshot: Snapshot {
                schema: SNAP_SCHEMA,
                ts: chrono::Utc
                    .with_ymd_and_hms(2026, 5, 20, 8, 35, 0)
                    .single()
                    .expect("pinned UTC"),
                features: [0.0_f32; FEATURE_LEN],
                raw: SnapshotRaw::default(),
                snapshot_hash: "0".repeat(64),
            },
            applied_arm: Some(applied_arm.to_string()),
            shield_state: Some(shield_state.to_string()),
            reason_chain: vec![format!(
                "bandit picks {applied_arm} (ucb={:.2})",
                ranked[0].1
            )],
            ranked_actions: ranked.iter().map(|(n, s)| (n.to_string(), *s)).collect(),
            conservative_alpha: 0.05,
        }
    }

    /// Step 23: the human render must surface every entry in the top-3
    /// ranked-actions slot. SPEC §3 anti-goal #4 — "no black-box
    /// decisions" — depends on this being visible without `--json`.
    #[test]
    fn explain_includes_top3_arms() {
        let cfg_path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/power.toml");
        let cfg = PowerConfig::load(&cfg_path).expect("shipped power.toml parses");
        let entry = explain_fixture(
            "build",
            "WARM_AC",
            &[("build", 1.34), ("code", 1.21), ("browse", 0.88)],
        );
        let out = format_explain(&[entry], &cfg, false);
        for name in ["build", "code", "browse"] {
            assert!(out.contains(name), "human render missing arm {name}: {out}");
        }
    }

    /// Step 23: when the bandit's chosen arm differs from the
    /// rules-baseline arm-of-the-day, the human render explicitly says
    /// "baseline says X, bandit picked Y" so the operator can spot
    /// exploration steps at a glance. The shipped config maps
    /// COOL_AC → `code`; pinning the bandit on `build` reveals both.
    #[test]
    fn explain_renders_baseline_arm() {
        let cfg_path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/power.toml");
        let cfg = PowerConfig::load(&cfg_path).expect("shipped power.toml parses");
        let baseline = cfg.rules_baseline.cool_ac.clone();
        assert_ne!(baseline, "build", "fixture relies on baseline ≠ build");
        let entry = explain_fixture(
            "build",
            "COOL_AC",
            &[("build", 1.34), (&baseline, 1.21), ("browse", 0.88)],
        );
        let out = format_explain(&[entry], &cfg, false);
        assert!(
            out.contains(&format!("baseline says {baseline}, bandit picked build")),
            "human render must surface both baseline and bandit pick: {out}",
        );
    }

    /// Step 23 golden snapshot: a single fixture entry renders into
    /// the canonical phrase shape documented in the implementation
    /// guidance — shield + baseline + bandit pick + UCB + top3 +
    /// applied actuator tuple + reason chain. Pinning a substring
    /// (not the full line) leaves room for downstream formatting
    /// tweaks without churn here, but every load-bearing token is
    /// asserted.
    #[test]
    fn explain_human_format_readable() {
        let cfg_path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/power.toml");
        let cfg = PowerConfig::load(&cfg_path).expect("shipped power.toml parses");
        let entry = explain_fixture(
            "build",
            "WARM_AC",
            &[("build", 1.34), ("code", 1.21), ("browse", 0.88)],
        );
        let out = format_explain(&[entry], &cfg, false);
        for needle in [
            "shield=WARM_AC",
            "ucb=1.34",
            "top3 [build:1.34, code:1.21, browse:0.88]",
            "platform_profile=performance",
            "epp=balance_performance",
            "reason: bandit picks build (ucb=1.34)",
        ] {
            assert!(
                out.contains(needle),
                "human format missing canonical token {needle:?}: {out}",
            );
        }
    }

    /// Step 23 edge case: the JSON form on an empty audit log returns
    /// the documented envelope with an empty `entries` array. The
    /// human form returns the documented `no audit entries yet` line.
    #[test]
    fn explain_empty_log_renders_sentinel() {
        let cfg = PowerConfig::default();
        let human = format_explain(&[], &cfg, false);
        assert_eq!(human, "no audit entries yet\n");
        let json = format_explain(&[], &cfg, true);
        let v: Value = serde_json::from_str(&json).expect("envelope is valid JSON");
        assert_eq!(v["schema"], EXPLAIN_SCHEMA);
        assert_eq!(v["entries"].as_array().expect("entries").len(), 0);
    }

    /// Step 17 wire-up: the shield_state slot reflects the DFA result
    /// the caller passes, not a constant. This is the test that proves
    /// the stub was retired.
    #[test]
    fn shield_state_reflects_dfa_result() {
        let cfg = PowerConfig::default();
        let resp = empty_response();
        for s in [
            ShieldState::CoolAc,
            ShieldState::WarmAc,
            ShieldState::Hot,
            ShieldState::BatteryLow,
            ShieldState::Meeting,
        ] {
            let v = build_status_value(&resp, &cfg, s, &default_onboarding());
            assert_eq!(v["shield_state"].as_str().unwrap(), s.as_str());
        }
    }

    /// Step 26 DoD: the `onboarding` block on
    /// `sy power status --json` reflects the supplied
    /// [`OnboardingStatus`] — `active`, `days_collected`, `ready_at`
    /// all flow through. The model `version_sha` is pinned at
    /// `"rules-baseline"` while `active`.
    #[test]
    fn onboarding_block_reflects_status() {
        const DAYS_COLLECTED: u32 = 7;
        let cfg = PowerConfig::default();
        let resp = empty_response();
        let ready_at = chrono::Utc::now() + chrono::Duration::days(7);
        let onb = OnboardingStatus {
            active: true,
            days_collected: DAYS_COLLECTED,
            ready_at,
        };
        let v = build_status_value(&resp, &cfg, ShieldState::CoolAc, &onb);
        assert_eq!(v["onboarding"]["active"].as_bool(), Some(true));
        assert_eq!(
            v["onboarding"]["days_collected"].as_u64(),
            Some(DAYS_COLLECTED as u64),
        );
        assert_eq!(
            v["onboarding"]["ready_at"].as_str(),
            Some(ready_at.to_rfc3339().as_str()),
        );
        assert_eq!(v["model"]["version_sha"], RULES_BASELINE_VERSION);
    }

    /// BUG-20260712-1530: when the daemon supplies its own onboarding
    /// view (`resp.onboarding`), the status document renders *that* gate
    /// — active, days_collected, ready_at, and its effective
    /// `target_days` — in preference to the CLI's local computation.
    /// This is the repro fix: the CLI-side `OnboardingStatus` claims the
    /// window is still active with a 14-day target, but the daemon
    /// (drop-in `SY_POWER_ONBOARDING_DAYS=0`) reports the gate open with
    /// `target_days = 0`; the printed block must match the daemon, not
    /// the CLI.
    #[test]
    fn onboarding_block_prefers_daemon_reported_view() {
        use crate::power::ipc::OnboardingWire;
        let cfg = PowerConfig::default();
        let mut resp = empty_response();
        let daemon_ready_at = chrono::Utc::now() - chrono::Duration::days(1);
        resp.onboarding = Some(OnboardingWire {
            active: false,
            days_collected: 5,
            ready_at: daemon_ready_at,
            target_days: 0,
        });
        // The CLI-side computation disagrees on every field.
        let cli_local = OnboardingStatus {
            active: true,
            days_collected: 5,
            ready_at: chrono::Utc::now() + chrono::Duration::days(9),
        };
        let v = build_status_value(&resp, &cfg, ShieldState::CoolAc, &cli_local);
        assert_eq!(
            v["onboarding"]["active"].as_bool(),
            Some(false),
            "daemon reports the gate open — CLI must not override it to active",
        );
        assert_eq!(v["onboarding"]["target_days"].as_u64(), Some(0));
        assert_eq!(v["onboarding"]["days_collected"].as_u64(), Some(5));
        assert_eq!(
            v["onboarding"]["ready_at"].as_str(),
            Some(daemon_ready_at.to_rfc3339().as_str()),
        );
        // The daemon's "gate open" view also flows into the derived
        // model version_sha (no longer pinned at the rules baseline).
        assert_ne!(v["onboarding"]["active"].as_bool(), Some(true));
    }

    /// BUG-20260712-1530 backward-compat: a response from a pre-fix
    /// daemon carries no `onboarding` field (`None`). The status
    /// document must then fall back to the CLI-side computation
    /// (`local` + `cfg.onboarding.days`) — preserving the old behaviour
    /// so old ↔ new wire frames stay compatible.
    #[test]
    fn onboarding_block_falls_back_when_daemon_field_absent() {
        const DAYS_COLLECTED: u32 = 7;
        let cfg = PowerConfig::default();
        let resp = empty_response();
        assert!(resp.onboarding.is_none(), "fixture models a pre-fix daemon");
        let ready_at = chrono::Utc::now() + chrono::Duration::days(7);
        let cli_local = OnboardingStatus {
            active: true,
            days_collected: DAYS_COLLECTED,
            ready_at,
        };
        let v = build_status_value(&resp, &cfg, ShieldState::CoolAc, &cli_local);
        assert_eq!(v["onboarding"]["active"].as_bool(), Some(true));
        assert_eq!(
            v["onboarding"]["days_collected"].as_u64(),
            Some(DAYS_COLLECTED as u64),
        );
        assert_eq!(
            v["onboarding"]["ready_at"].as_str(),
            Some(ready_at.to_rfc3339().as_str()),
        );
        assert_eq!(
            v["onboarding"]["target_days"].as_u64(),
            Some(cfg.onboarding.days as u64),
            "fallback target_days must come from the CLI-loaded config",
        );
    }

    /// Step 32 DoD: the waybar JSON pill on `sy power status --waybar`
    /// carries `class="onboarding"` while the onboarding window is
    /// active, and the `text` slot includes the days-remaining hint
    /// (`target_days - days_collected`). With `days_collected=3` and
    /// the default 14-day window, the hint must include `"11d"`.
    #[test]
    fn waybar_class_onboarding_during_first_14d() {
        const DAYS_COLLECTED: u32 = 3;
        const DAYS_REMAINING_HINT: &str = "11d";
        let cfg = PowerConfig::default();
        let resp = empty_response();
        let onb = OnboardingStatus {
            active: true,
            days_collected: DAYS_COLLECTED,
            ready_at: chrono::Utc::now() + chrono::Duration::days(11),
        };
        let out = format_waybar(&resp, &cfg, ShieldState::CoolAc, &onb);
        let v: Value = serde_json::from_str(&out).expect("waybar JSON parses");
        assert_eq!(v["class"], "onboarding");
        let text = v["text"].as_str().expect("text slot");
        assert!(
            text.contains(DAYS_REMAINING_HINT),
            "onboarding text must include days remaining {DAYS_REMAINING_HINT:?}: {text}",
        );
    }

    /// `onboarding_hint` returns Some("learning Xd Yh") only while
    /// `OnboardingStatus.active` is true. Pins the contract the perf
    /// (`custom/pwr`) tooltip relies on so the line drops cleanly
    /// once onboarding is over.
    #[test]
    fn onboarding_hint_renders_only_while_active() {
        let cfg = PowerConfig::default();
        let onb_active = OnboardingStatus {
            active: true,
            days_collected: 3,
            ready_at: chrono::Utc::now() + chrono::Duration::days(11),
        };
        let hint = onboarding_hint(&onb_active, &cfg).expect("active hint");
        assert!(
            hint.starts_with("learning ") && hint.contains("11d"),
            "active hint should mention the 11d remainder: {hint}"
        );

        let onb_done = OnboardingStatus {
            active: false,
            days_collected: cfg.onboarding.days,
            ready_at: chrono::Utc::now(),
        };
        assert_eq!(onboarding_hint(&onb_done, &cfg), None);
    }

    /// Step 32: precedence test — `shield_state == Meeting` must
    /// classify as `meeting`, never `bandit`, even when the bandit
    /// would otherwise have a verdict. Pins the class-precedence
    /// order documented in the Step 32 implementation guidance.
    #[test]
    fn waybar_class_meeting_overrides_bandit() {
        let cfg = PowerConfig::default();
        let resp = empty_response();
        let onb = OnboardingStatus {
            active: false,
            days_collected: 30,
            ready_at: chrono::Utc::now() - chrono::Duration::days(16),
        };
        let out = format_waybar(&resp, &cfg, ShieldState::Meeting, &onb);
        let v: Value = serde_json::from_str(&out).expect("waybar JSON parses");
        assert_eq!(v["class"], "meeting");
    }

    /// Step 32: when `drift.adwin_alarm = true`, the waybar pill must
    /// classify as `drift` — highest priority in the precedence
    /// table, overriding even MEETING / onboarding. Mirrors the SPEC
    /// §5 phase-7 "drift detected" notification surface.
    #[test]
    fn waybar_class_drift_when_alarm_active() {
        use crate::power::drift::DriftStatus;
        let cfg = PowerConfig::default();
        let mut resp = empty_response();
        resp.drift = DriftStatus {
            adwin_alarm: true,
            ddm_warning: true,
            last_alarm_at: Some(chrono::Utc::now()),
        };
        let onb = OnboardingStatus {
            active: false,
            days_collected: 30,
            ready_at: chrono::Utc::now() - chrono::Duration::days(16),
        };
        let out = format_waybar(&resp, &cfg, ShieldState::CoolAc, &onb);
        let v: Value = serde_json::from_str(&out).expect("waybar JSON parses");
        assert_eq!(v["class"], "drift");
    }

    /// Step 32: the tooltip surfaces the active arm so a hover over
    /// the pill answers "which arm is the bandit on right now". With
    /// `applied_arm = "build"`, the tooltip text must contain
    /// `"build"`.
    #[test]
    fn waybar_tooltip_includes_top_arm() {
        use crate::power::log::{AuditEntry, SCHEMA_ID as AUDIT_SCHEMA};
        use crate::power::snapshot::{
            Snapshot, SnapshotRaw, FEATURE_LEN, SCHEMA_ID as SNAP_SCHEMA,
        };
        use chrono::TimeZone;
        let cfg_path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/power.toml");
        let cfg = PowerConfig::load(&cfg_path).expect("shipped power.toml parses");
        let entry = AuditEntry {
            schema: AUDIT_SCHEMA,
            snapshot: Snapshot {
                schema: SNAP_SCHEMA,
                ts: chrono::Utc
                    .with_ymd_and_hms(2026, 5, 20, 12, 0, 0)
                    .single()
                    .expect("pinned UTC"),
                features: [0.0_f32; FEATURE_LEN],
                raw: SnapshotRaw::default(),
                snapshot_hash: "0".repeat(64),
            },
            applied_arm: Some("build".to_string()),
            shield_state: Some("COOL_AC".to_string()),
            reason_chain: vec!["bandit picks build".to_string()],
            ranked_actions: vec![("build".to_string(), 1.34_f32)],
            conservative_alpha: 0.05,
        };
        let resp = StatusResponse {
            schema: STATUS_SCHEMA.to_string(),
            snapshot_hash: "0".repeat(64),
            snapshot: serde_json::json!({}),
            last_audit: Some(entry),
            drift: crate::power::drift::DriftStatus::default(),
            model: None,
            onboarding: None,
        };
        let onb = OnboardingStatus {
            active: false,
            days_collected: 30,
            ready_at: chrono::Utc::now() - chrono::Duration::days(16),
        };
        let out = format_waybar(&resp, &cfg, ShieldState::CoolAc, &onb);
        let v: Value = serde_json::from_str(&out).expect("waybar JSON parses");
        let tip = v["tooltip"].as_str().expect("tooltip slot");
        assert!(
            tip.contains("build"),
            "tooltip must mention applied arm 'build': {tip}",
        );
    }

    /// Step T3 DoD: when the daemon's last retrain attempt errored
    /// with `InsufficientClassCoverage`, `sy power status --json` must
    /// surface the missing-classes list on `model.missing_classes` so
    /// the operator can attribute the skipped train without digging
    /// through `journalctl`.
    #[test]
    fn status_model_block_surfaces_missing_classes() {
        let cfg = PowerConfig::default();
        let mut resp = empty_response();
        resp.model = Some(crate::power::ipc::ModelStatus {
            missing_classes: vec!["call".to_string(), "build".to_string()],
        });
        let v = build_status_value(&resp, &cfg, ShieldState::CoolAc, &default_onboarding());
        let missing = v["model"]["missing_classes"]
            .as_array()
            .expect("missing_classes array");
        assert_eq!(missing.len(), 2);
        assert_eq!(missing[0], "call");
        assert_eq!(missing[1], "build");
    }

    /// Step T3 DoD: when no retrain has reported a coverage gap, the
    /// `model.missing_classes` slot is `null`, not an empty array —
    /// the documented "hasn't fired or last train succeeded" shape.
    #[test]
    fn status_model_missing_classes_null_when_no_gap() {
        let cfg = PowerConfig::default();
        let resp = empty_response();
        let v = build_status_value(&resp, &cfg, ShieldState::CoolAc, &default_onboarding());
        assert!(
            v["model"]["missing_classes"].is_null(),
            "expected null missing_classes, got {}",
            v["model"]["missing_classes"],
        );
    }

    /// Step 31 DoD: the `drift` block on `sy power status --json`
    /// reflects the wire-borne [`crate::power::drift::DriftStatus`]
    /// (carried by `resp.drift`). `last_alarm_at` is serialised as a
    /// human-readable RFC3339 string when populated and `null` when
    /// absent.
    #[test]
    fn drift_block_reflects_status_response() {
        use crate::power::drift::DriftStatus;
        let cfg = PowerConfig::default();
        let mut resp = empty_response();
        let alarm_at = chrono::Utc::now();
        resp.drift = DriftStatus {
            adwin_alarm: true,
            ddm_warning: true,
            last_alarm_at: Some(alarm_at),
        };
        let v = build_status_value(&resp, &cfg, ShieldState::CoolAc, &default_onboarding());
        assert_eq!(v["drift"]["adwin_alarm"].as_bool(), Some(true));
        assert_eq!(v["drift"]["ddm_warning"].as_bool(), Some(true));
        assert_eq!(
            v["drift"]["last_alarm_at"].as_str(),
            Some(alarm_at.to_rfc3339().as_str()),
        );
    }
}
