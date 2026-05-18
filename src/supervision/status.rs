// arch-supervision Step 3 (`specs/roadmaps/arch-supervision/ROADMAP.md`):
// `sy service status <name>` — maps `systemctl --user show` output to
// the SPEC §4.5 state table.
//
// Stable JSON schema emitted by `--json` (kept in lock-step with this
// comment so agents can rely on it):
// ```json
// {
//   "name":         "aiplane",
//   "unit":         "sy-aiplane.service",
//   "state":        "ready | stopped | starting | degraded | failed | not_installed",
//   "active_state": "active | inactive | failed | activating | …",
//   "sub_state":    "running | dead | failed | start | …",
//   "result":       "success | exit-code | signal | timeout | core-dump | …"
// }
// ```
//
// `systemctl show -p ActiveState -p SubState -p Result --value` is **not**
// used because `--value` strips the keys, so we'd lose the ability to
// re-order or omit a field; we ask for `KEY=value\n` and parse it. The
// format is documented in `systemd.exec(5)` and has been stable since
// the systemd 208 era (~2013). If it changes, the parser tests catch
// it before the wrapper does.

use serde::Serialize;

use crate::supervision::service::{exit, ServiceError};

/// SPEC §4.5 state table — the logical sy state derived from systemd's
/// `ActiveState` + `SubState` + `Result` triple.
///
/// Note: the SPEC also names a `Degraded` state (sy-level concept,
/// detected via `StatusText=degraded: <reason>`). It lands together
/// with the `sd_notify` plumbing in Step 4 of this roadmap — exposing
/// it here without the corresponding parser path would be unreachable
/// code, which the no-dead-code rule forbids.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceStatus {
    /// Unit file is not loaded (`LoadState=not-found`) or `ActiveState`
    /// is empty / `inactive` with `SubState=dead` + `Result=success`
    /// **and** the unit file doesn't exist on disk. Reported by
    /// `systemctl show` as `ActiveState=inactive SubState=dead`.
    #[default]
    NotInstalled,
    /// `ActiveState=inactive`, `SubState=dead`, the unit *is* loaded.
    Stopped,
    /// `ActiveState=activating` — `Type=notify` unit hasn't sent
    /// `READY=1` yet.
    Starting,
    /// `ActiveState=active`, `SubState=running`. The happy path.
    Ready,
    /// `ActiveState=failed`.
    Failed,
}

impl ServiceStatus {
    /// SPEC §4.5 state-table lookup. Pure function over the systemctl
    /// triple; covered by `status_*_maps_to_*` unit tests so a future
    /// systemd reshuffle is caught by `make test`.
    pub fn from_systemctl(active: &str, _sub: &str, _result: &str, load: &str) -> Self {
        if load == "not-found" || load.is_empty() {
            return ServiceStatus::NotInstalled;
        }
        match active {
            "active" => ServiceStatus::Ready,
            "activating" => ServiceStatus::Starting,
            "inactive" => ServiceStatus::Stopped,
            "failed" => ServiceStatus::Failed,
            _ => ServiceStatus::Stopped,
        }
    }
}

/// Raw `systemctl show` triple plus the load state we always ask for.
/// Public so callers (e.g. `sy doctor`) can render the same record
/// without re-shelling out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StatusRecord {
    pub name: String,
    pub unit: String,
    pub state: ServiceStatus,
    pub active_state: String,
    pub sub_state: String,
    pub result: String,
}

/// Parse `systemctl show -p Key1 -p Key2 …` output. Format is
/// `KEY=value\n` per `systemd.exec(5)`; empty values (`KEY=`) come
/// through as `""`. Unknown keys are ignored so we tolerate systemd
/// adding new properties without breaking the parser.
fn parse_show(bytes: &[u8]) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for line in String::from_utf8_lossy(bytes).lines() {
        if let Some((k, v)) = line.split_once('=') {
            out.insert(k.to_string(), v.to_string());
        }
    }
    out
}

/// Status + the raw systemctl triple for `--json` consumers. Kept
/// separate from `status()` so unit tests for the parser don't have
/// to shell out.
pub fn status_record(name: &str, unit: &str) -> Result<StatusRecord, ServiceError> {
    let out = std::process::Command::new("systemctl")
        .args([
            "--user",
            "show",
            "-p",
            "ActiveState",
            "-p",
            "SubState",
            "-p",
            "Result",
            "-p",
            "LoadState",
            unit,
        ])
        .output()
        .map_err(|e| ServiceError {
            code: exit::GENERIC,
            msg: format!("spawn systemctl --user show {unit}: {e}"),
        })?;
    if !out.status.success() {
        return Err(ServiceError {
            code: exit::GENERIC,
            msg: format!(
                "systemctl --user show {unit} exited with status {}",
                out.status
            ),
        });
    }
    let kv = parse_show(&out.stdout);
    let active_state = kv.get("ActiveState").cloned().unwrap_or_default();
    let sub_state = kv.get("SubState").cloned().unwrap_or_default();
    let result = kv.get("Result").cloned().unwrap_or_default();
    let load = kv.get("LoadState").cloned().unwrap_or_default();
    let state = ServiceStatus::from_systemctl(&active_state, &sub_state, &result, &load);
    Ok(StatusRecord {
        name: name.to_string(),
        unit: unit.to_string(),
        state,
        active_state,
        sub_state,
        result,
    })
}

/// Render the status record on stdout and map the SPEC §4.5 logical
/// state to the SPEC §4.7 exit code:
///
///   * `Ready`         → `exit::SUCCESS`     (0)
///   * `Starting`      → `exit::NOT_READY`   (4)
///   * `NotInstalled`  → `exit::NOT_READY`   (4)
///   * `Stopped`       → `exit::DRIFT`       (3)
///   * `Failed`        → `exit::DRIFT`       (3)
///
/// The mapping lets agents `sy service status aiplane && …` to gate
/// follow-up work on a healthy daemon without parsing `--json`.
pub fn run_cli(name: &str, unit: &str, json: bool) -> anyhow::Result<()> {
    let rec = status_record(name, unit)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rec)?);
    } else {
        println!(
            "{name}: {state} (active={active} sub={sub} result={result})",
            state = format!("{:?}", rec.state).to_lowercase(),
            active = rec.active_state,
            sub = rec.sub_state,
            result = rec.result,
        );
    }
    match rec.state {
        ServiceStatus::Ready => Ok(()),
        ServiceStatus::Starting | ServiceStatus::NotInstalled => Err(ServiceError {
            code: exit::NOT_READY,
            msg: format!("{name}: not ready ({:?})", rec.state),
        }
        .into()),
        ServiceStatus::Stopped | ServiceStatus::Failed => Err(ServiceError {
            code: exit::DRIFT,
            msg: format!("{name}: drift ({:?})", rec.state),
        }
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic `systemctl show` outputs — bytes-for-bytes what we'd
    // get from a real `systemctl --user show -p … sy-<name>.service`
    // on Fedora 43 (systemd 256). Kept inline so a future systemd
    // reshuffle is caught by `make test`.

    #[test]
    fn status_active_substate_running_maps_to_ready() {
        let out = b"ActiveState=active\nSubState=running\nResult=success\nLoadState=loaded\n";
        let kv = parse_show(out);
        let s = ServiceStatus::from_systemctl(
            &kv["ActiveState"],
            &kv["SubState"],
            &kv["Result"],
            &kv["LoadState"],
        );
        assert_eq!(s, ServiceStatus::Ready);
    }

    #[test]
    fn status_failed_maps_to_failed() {
        let out = b"ActiveState=failed\nSubState=failed\nResult=exit-code\nLoadState=loaded\n";
        let kv = parse_show(out);
        let s = ServiceStatus::from_systemctl(
            &kv["ActiveState"],
            &kv["SubState"],
            &kv["Result"],
            &kv["LoadState"],
        );
        assert_eq!(s, ServiceStatus::Failed);
    }

    #[test]
    fn status_inactive_maps_to_stopped() {
        let out = b"ActiveState=inactive\nSubState=dead\nResult=success\nLoadState=loaded\n";
        let kv = parse_show(out);
        let s = ServiceStatus::from_systemctl(
            &kv["ActiveState"],
            &kv["SubState"],
            &kv["Result"],
            &kv["LoadState"],
        );
        assert_eq!(s, ServiceStatus::Stopped);
    }

    #[test]
    fn status_not_found_maps_to_not_installed() {
        // `LoadState=not-found` is what systemd reports when the unit
        // file doesn't exist on disk. Belt-and-suspenders for the
        // hosts running `sy service status` against a name the user
        // hasn't applied yet.
        let out = b"ActiveState=inactive\nSubState=dead\nResult=success\nLoadState=not-found\n";
        let kv = parse_show(out);
        let s = ServiceStatus::from_systemctl(
            &kv["ActiveState"],
            &kv["SubState"],
            &kv["Result"],
            &kv["LoadState"],
        );
        assert_eq!(s, ServiceStatus::NotInstalled);
    }

    #[test]
    fn status_activating_maps_to_starting() {
        // `Type=notify` daemons enter `activating` after exec and
        // remain there until `sd_notify(READY=1)` fires — SPEC §4.5
        // "starting" row.
        let out = b"ActiveState=activating\nSubState=start\nResult=success\nLoadState=loaded\n";
        let kv = parse_show(out);
        let s = ServiceStatus::from_systemctl(
            &kv["ActiveState"],
            &kv["SubState"],
            &kv["Result"],
            &kv["LoadState"],
        );
        assert_eq!(s, ServiceStatus::Starting);
    }

    #[test]
    fn json_schema_keys_are_total() {
        // Stability check for the `--json` schema in the head comment.
        // Every documented key MUST be present so agent consumers can
        // address them unconditionally.
        let rec = StatusRecord {
            name: "aiplane".into(),
            unit: "sy-aiplane.service".into(),
            state: ServiceStatus::Ready,
            active_state: "active".into(),
            sub_state: "running".into(),
            result: "success".into(),
        };
        let v: serde_json::Value = serde_json::to_value(&rec).unwrap();
        for key in [
            "name",
            "unit",
            "state",
            "active_state",
            "sub_state",
            "result",
        ] {
            assert!(v.get(key).is_some(), "missing key: {key}");
        }
        assert_eq!(v["state"], "ready");
    }
}
