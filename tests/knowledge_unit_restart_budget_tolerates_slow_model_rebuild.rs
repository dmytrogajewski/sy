//! BUG-20260927-0119: `sy-knowledge.service` must not latch itself `failed`
//! faster than a human (or a VAIP recompile) can fix the cause.
//!
//! The unit used to retry every 5 s with a 5-attempt budget inside a 60 s
//! window — and the two start-limit directives were spelled in `[Service]`,
//! the pre-systemd-230 location. Result: a missing embed model burned the
//! burst in ~25 s, systemd answered "Start request repeated too quickly",
//! and the plane stayed dead for four days because nothing would start the
//! unit again without a manual `systemctl --user reset-failed`.
//!
//! These assertions keep the retry window wider than a model rebuild and
//! the directives in the section modern systemd actually reads.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

const UNIT: &str = "configs/systemd/user/sy-knowledge.service";
/// A cold `prep_npu_workload.py --workload embed` measures ~10–20 min on
/// Strix, so retries must still be pending well past that.
const MIN_RETRY_WINDOW_SECS: u64 = 600;
const MIN_RESTART_DELAY_SECS: u64 = 15;

/// Parse `key=value` pairs per `[section]`, ignoring comments.
fn sections(raw: &str) -> HashMap<&str, Vec<(&str, &str)>> {
    let mut out: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();
    let mut section = "";
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name;
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            out.entry(section)
                .or_default()
                .push((key.trim(), value.trim()));
        }
    }
    out
}

/// First value for `key` in `section`.
fn value<'a>(
    parsed: &'a HashMap<&'a str, Vec<(&'a str, &'a str)>>,
    section: &str,
    key: &str,
) -> Option<&'a str> {
    parsed
        .get(section)?
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| *v)
}

/// systemd duration suffixes we care about (`s` explicit or bare seconds).
fn secs(raw: &str) -> u64 {
    let digits: String = raw.chars().take_while(|c| c.is_ascii_digit()).collect();
    let unit = raw[digits.len()..].trim();
    let base: u64 = digits
        .parse()
        .unwrap_or_else(|_| panic!("bad duration {raw:?}"));
    match unit {
        "" | "s" => base,
        "min" | "m" => base * 60,
        "h" => base * 3600,
        other => panic!("unsupported duration unit {other:?} in {raw:?}"),
    }
}

fn unit_path() -> PathBuf {
    let path = Path::new(UNIT);
    assert!(path.is_file(), "{UNIT} missing");
    path.to_path_buf()
}

#[test]
fn start_limits_live_in_the_unit_section_where_systemd_reads_them() {
    let raw = std::fs::read_to_string(unit_path()).expect("read unit");
    let parsed = sections(&raw);
    for key in ["StartLimitIntervalSec", "StartLimitBurst"] {
        assert!(
            value(&parsed, "Unit", key).is_some(),
            "{key} must be set in [Unit]; [Service] is the legacy location systemd 257 ignores"
        );
        assert!(
            value(&parsed, "Service", key).is_none()
                && value(&parsed, "Service", "StartLimitInterval").is_none(),
            "{key} must not also be spelled in [Service] — two sources of truth"
        );
    }
}

#[test]
fn retry_window_outlasts_a_model_rebuild() {
    let raw = std::fs::read_to_string(unit_path()).expect("read unit");
    let parsed = sections(&raw);
    let interval = secs(
        value(&parsed, "Unit", "StartLimitIntervalSec").expect("StartLimitIntervalSec in [Unit]"),
    );
    let restart_sec =
        secs(value(&parsed, "Service", "RestartSec").expect("RestartSec in [Service]"));
    let burst: u64 = value(&parsed, "Unit", "StartLimitBurst")
        .expect("StartLimitBurst in [Unit]")
        .parse()
        .expect("burst is a count");

    assert!(
        restart_sec >= MIN_RESTART_DELAY_SECS,
        "RestartSec={restart_sec}s hammers the NPU/qdrant on every attempt"
    );
    assert!(
        interval >= MIN_RETRY_WINDOW_SECS,
        "retry window {interval}s is shorter than a cold prep_npu_workload run; \
         the unit would latch failed before the artifact is rebuilt"
    );
    // The window must actually contain the burst at this restart delay.
    assert!(
        burst * restart_sec <= interval,
        "burst {burst} x RestartSec {restart_sec}s exceeds the {interval}s window — \
         attempts get cut off before the budget is used"
    );
}

#[test]
fn restart_policy_still_recovers_from_a_crash() {
    let raw = std::fs::read_to_string(unit_path()).expect("read unit");
    let parsed = sections(&raw);
    assert_eq!(
        value(&parsed, "Service", "Restart"),
        Some("on-failure"),
        "degraded start must not remove crash-restart"
    );
}
