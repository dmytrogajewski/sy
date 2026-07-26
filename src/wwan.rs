//! Mobile-broadband (WWAN / 4G-LTE) plane for USB modems.
//!
//! Declares carrier APN profiles in `~/.config/sy/wwan.toml` (rendered
//! from `configs/sy/wwan.toml` by `sy apply`) and reconciles them into
//! NetworkManager `gsm` connections named `sy-wwan-<name>`. sy owns
//! exactly the connections it creates; user-made profiles are untouched.
//!
//! Subcommands:
//!   sy wwan enable   — create/update the managed gsm connection(s), idempotent
//!   sy wwan disable  — delete the managed connection(s); modem hardware untouched
//!   sy wwan up       — bring the managed connection(s) online now
//!   sy wwan down     — take the managed connection(s) offline now
//!   sy wwan status   — modem state, operator, signal + managed-connection status
//!
//! ModemManager does the radio work; this module only owns the NM profile
//! so the working config is reproducible instead of a one-off `nmcli add`.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// Prefix that marks a NetworkManager connection as sy-owned. Reconcile
/// only ever creates, modifies, or deletes connections under this prefix.
const CONN_PREFIX: &str = "sy-wwan-";

/// The AT mode-switch helper, embedded so the single binary carries it and
/// there is no repo-path resolution at runtime (`python3 -` reads it on stdin).
const MODESWITCH_PY: &str = include_str!("../scripts/wwan_modeswitch.py");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Short id; the NM connection is named `sy-wwan-<name>`.
    pub name: String,
    /// Carrier APN (the one setting the SIM's operator requires).
    pub apn: String,
    /// Bring the connection up automatically when the modem appears.
    #[serde(default = "default_true")]
    pub autoconnect: bool,
    /// Refuse to attach while roaming (avoids surprise roaming charges).
    #[serde(default)]
    pub home_only: bool,
    /// Optional carrier username (most SIMs need none).
    #[serde(default)]
    pub username: Option<String>,
    /// Optional carrier password (most SIMs need none).
    #[serde(default)]
    pub password: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, rename = "profile")]
    pub profiles: Vec<Profile>,
}

/// Parse a `wwan.toml` document into profiles.
pub fn parse_config(text: &str) -> Result<Config> {
    toml::from_str(text).context("parse wwan config")
}

impl Profile {
    /// Deterministic NM connection name for this profile.
    fn conn_name(&self) -> String {
        format!("{CONN_PREFIX}{}", self.name)
    }

    /// `nmcli` argv to create the gsm connection from scratch.
    fn add_args(&self) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "connection".into(),
            "add".into(),
            "type".into(),
            "gsm".into(),
            "con-name".into(),
            self.conn_name(),
            // ifname '*' binds the profile to whichever modem is present
            // rather than a kernel-assigned wwpXsY name that changes.
            "ifname".into(),
            "*".into(),
            "gsm.apn".into(),
            self.apn.clone(),
        ];
        a.extend(self.setting_pairs());
        a
    }

    /// `nmcli` argv to bring an existing gsm connection into line.
    fn modify_args(&self) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "connection".into(),
            "modify".into(),
            self.conn_name(),
            "gsm.apn".into(),
            self.apn.clone(),
        ];
        a.extend(self.setting_pairs());
        a
    }

    /// The settings shared by add and modify (everything after the APN).
    fn setting_pairs(&self) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "connection.autoconnect".into(),
            yesno(self.autoconnect),
            "gsm.home-only".into(),
            yesno(self.home_only),
        ];
        // Empty string clears a previously-set credential, keeping the
        // NM profile a faithful mirror of the declared config.
        a.push("gsm.username".into());
        a.push(self.username.clone().unwrap_or_default());
        a.push("gsm.password".into());
        a.push(self.password.clone().unwrap_or_default());
        a
    }
}

fn yesno(b: bool) -> String {
    if b { "yes" } else { "no" }.to_string()
}

fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/sy/wwan.toml")
}

pub fn run(action: Option<&str>, json: bool, yes: bool) -> Result<()> {
    match action.unwrap_or("status") {
        "enable" => enable(),
        "disable" => disable(),
        "up" => set_state(true),
        "down" => set_state(false),
        "status" => status(json),
        "modeswitch" => modeswitch(yes),
        other => Err(anyhow!(
            "unknown wwan action: {other} (enable|disable|up|down|status|modeswitch)"
        )),
    }
}

fn load_config() -> Result<Config> {
    let p = config_path();
    let text = fs::read_to_string(&p).with_context(|| {
        format!(
            "read {} — run `sy apply` to materialise it from configs/sy/wwan.toml",
            p.display()
        )
    })?;
    parse_config(&text)
}

fn enable() -> Result<()> {
    ensure_modemmanager()?;
    ensure_wwan_radio()?;
    let cfg = load_config()?;
    if cfg.profiles.is_empty() {
        println!("no wwan profiles declared in {}", config_path().display());
        return Ok(());
    }
    for p in &cfg.profiles {
        let name = p.conn_name();
        if connection_exists(&name)? {
            run_nmcli(&p.modify_args())?;
            println!("wwan: updated {name} (apn={})", p.apn);
        } else {
            run_nmcli(&p.add_args())?;
            println!("wwan: created {name} (apn={})", p.apn);
        }
    }
    println!("run `sy wwan status` to check registration and signal");
    Ok(())
}

fn disable() -> Result<()> {
    let cfg = load_config()?;
    let mut removed = 0;
    for p in &cfg.profiles {
        let name = p.conn_name();
        if connection_exists(&name)? {
            run_nmcli(&["connection".into(), "delete".into(), name.clone()])?;
            println!("wwan: deleted {name}");
            removed += 1;
        }
    }
    if removed == 0 {
        println!("wwan: no managed connections present");
    }
    Ok(())
}

fn set_state(up: bool) -> Result<()> {
    let cfg = load_config()?;
    let verb = if up { "up" } else { "down" };
    for p in &cfg.profiles {
        let name = p.conn_name();
        if !connection_exists(&name)? {
            println!("wwan: {name} not present — run `sy wwan enable` first");
            continue;
        }
        let ok = run_nmcli(&["connection".into(), verb.into(), "id".into(), name.clone()]).is_ok();
        println!("wwan: {name} {verb}{}", if ok { "" } else { " (failed)" });
    }
    Ok(())
}

fn status(json: bool) -> Result<()> {
    let modem = probe_modem();
    let managed = managed_connections()?;
    if json {
        let doc = serde_json::json!({
            "modem": modem,
            "managed_connections": managed,
        });
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(());
    }
    match &modem {
        Some(m) => {
            println!("modem:    {} {}", m.manufacturer, m.model);
            println!("state:    {}", m.state);
            println!("operator: {}", m.operator);
            println!("signal:   {}", m.signal);
        }
        None => println!("modem:    (none detected — is it plugged in? is ModemManager running?)"),
    }
    if managed.is_empty() {
        println!("managed:  (none — run `sy wwan enable`)");
    } else {
        for c in &managed {
            println!(
                "managed:  {} [{}]",
                c.name,
                if c.active { "active" } else { "inactive" }
            );
        }
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct ModemInfo {
    manufacturer: String,
    model: String,
    state: String,
    operator: String,
    signal: String,
}

#[derive(Debug, Serialize)]
struct ManagedConn {
    name: String,
    active: bool,
}

fn probe_modem() -> Option<ModemInfo> {
    let list = Command::new("mmcli").arg("-L").output().ok()?;
    let list = String::from_utf8_lossy(&list.stdout);
    // A line like: `/org/freedesktop/ModemManager1/Modem/0 [Fibocom] …`
    let idx: String = list
        .lines()
        .find_map(|l| l.rsplit('/').next().and_then(|t| t.split_whitespace().next()))
        .filter(|t| t.chars().all(|c| c.is_ascii_digit()))
        .map(|t| t.to_string())?;
    let out = Command::new("mmcli").args(["-m", &idx]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    Some(ModemInfo {
        manufacturer: field(&text, "manufacturer").unwrap_or_else(|| "?".into()),
        model: field(&text, "model").unwrap_or_else(|| "?".into()),
        state: field(&text, "state").unwrap_or_else(|| "?".into()),
        operator: field(&text, "operator name").unwrap_or_else(|| "?".into()),
        signal: field(&text, "signal quality").unwrap_or_else(|| "?".into()),
    })
}

/// Pull `value` out of an `mmcli` `Section | key: value` block line. The
/// real key sits after the last column separator (`|`) and before the colon.
fn field(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let (lhs, rhs) = l.split_once(':')?;
        let k = lhs.rsplit('|').next().unwrap_or(lhs).trim();
        k.eq_ignore_ascii_case(key)
            .then(|| rhs.trim().to_string())
            .filter(|s| !s.is_empty())
    })
}

fn managed_connections() -> Result<Vec<ManagedConn>> {
    let out = Command::new("nmcli")
        .args(["-t", "-f", "NAME,ACTIVE", "connection", "show"])
        .output()
        .context("nmcli connection show")?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (name, active) = l.rsplit_once(':')?;
            name.starts_with(CONN_PREFIX).then(|| ManagedConn {
                name: name.to_string(),
                active: active == "yes",
            })
        })
        .collect())
}

fn connection_exists(name: &str) -> Result<bool> {
    let out = Command::new("nmcli")
        .args(["-t", "-f", "NAME", "connection", "show"])
        .output()
        .context("nmcli connection show")?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|l| l == name))
}

fn run_nmcli(args: &[String]) -> Result<()> {
    let status = Command::new("nmcli")
        .args(args)
        .status()
        .context("spawn nmcli")?;
    if !status.success() {
        return Err(anyhow!("`nmcli {}` failed", args.join(" ")));
    }
    Ok(())
}

/// NetworkManager keeps a soft radio switch per technology; a WWAN modem
/// device stays `unavailable` (and no gsm profile can activate) while
/// `nmcli radio wwan` is off. Reconcile it on rather than leaving the
/// working state to a hand-typed `nmcli radio wwan on`.
fn ensure_wwan_radio() -> Result<()> {
    let on = Command::new("nmcli")
        .args(["radio", "wwan"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "enabled")
        .unwrap_or(false);
    if on {
        return Ok(());
    }
    run_nmcli(&["radio".into(), "wwan".into(), "on".into()])?;
    println!("wwan: enabled NetworkManager wwan radio");
    Ok(())
}

fn ensure_modemmanager() -> Result<()> {
    let active = Command::new("systemctl")
        .args(["is-active", "--quiet", "ModemManager"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if active {
        return Ok(());
    }
    let ok = Command::new("sudo")
        .args(["systemctl", "enable", "--now", "ModemManager"])
        .stdout(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err(anyhow!(
            "ModemManager is not running and could not be started.\n  install:  sudo dnf install -y ModemManager\n  enable:   sudo systemctl enable --now ModemManager"
        ))
    }
}

/// GTUSBMODE value that puts the L850-GL into an MBIM composition.
const MBIM_MODE: u8 = 7;
/// Seconds to wait for the modem to re-enumerate after the reset.
const REENUM_WAIT_SECS: u64 = 16;

/// Switch a Fibocom L850-GL / Intel XMM7360 from its stock NCM+PPP
/// composition to MBIM, so ModemManager drives it with the `fibocom`
/// plugin over `cdc-wdm0` instead of legacy AT+PPP dialup (which churns
/// the modem). The switch is a one-time, power-cycle-persistent change,
/// so it is gated behind `--yes`. Without `--yes` it only previews the
/// current mode and firmware (non-destructive).
fn modeswitch(yes: bool) -> Result<()> {
    // ModemManager holds the AT port; it must be stopped for either the
    // read-only probe or the switch to reach `/dev/ttyACM*`.
    stop_modemmanager()?;
    let restart = || {
        let _ = Command::new("sudo")
            .args(["systemctl", "start", "ModemManager"])
            .status();
    };

    if !yes {
        println!("current modem composition (read-only preview):");
        let out = run_modeswitch(&["--check"]);
        restart();
        let out = out?;
        print!("{out}");
        println!(
            "\nThis modem needs MBIM mode for a stable link. To switch it:\n  \
             sy wwan modeswitch --yes\n\n\
             WARNING: AT+GTUSBMODE={MBIM_MODE} + reset is permanent (persists across\n\
             power cycles) and, on some XMM7360 firmware, has been reported to cause\n\
             a reboot loop. Check the firmware line above against the journey doc\n\
             before committing."
        );
        return Ok(());
    }

    println!("switching modem to MBIM (GTUSBMODE={MBIM_MODE}) and resetting …");
    let out = run_modeswitch(&["--mode", "7"]);
    // Always bring ModemManager back, even if the switch errored.
    let out = out.inspect_err(|_| restart())?;
    print!("{out}");
    println!("waiting {REENUM_WAIT_SECS}s for the modem to re-enumerate …");
    sleep(Duration::from_secs(REENUM_WAIT_SECS));
    restart();
    sleep(Duration::from_secs(REENUM_WAIT_SECS / 2));

    match probe_modem() {
        Some(m) if m.model.contains("L850-GL") || m.manufacturer.contains("Fibocom Wireless") => {
            println!(
                "modeswitch ok: {} {} (state {})",
                m.manufacturer, m.model, m.state
            );
            println!("run `sy wwan enable && sy wwan up` to bring the link online");
        }
        Some(m) => println!(
            "modem present ({} {}) but identity unchanged — re-check with `sy wwan status`",
            m.manufacturer, m.model
        ),
        None => println!(
            "modem not visible yet; give it a few more seconds and run `sy wwan status`"
        ),
    }
    Ok(())
}

/// Pipe the embedded mode-switch script to `sudo python3 -` with `args`.
fn run_modeswitch(args: &[&str]) -> Result<String> {
    let mut child = Command::new("sudo")
        .arg("python3")
        .arg("-")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawn sudo python3 for wwan modeswitch")?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow!("modeswitch: no stdin"))?
        .write_all(MODESWITCH_PY.as_bytes())
        .context("feed modeswitch script")?;
    let out = child
        .wait_with_output()
        .context("wait for modeswitch script")?;
    if !out.status.success() {
        return Err(anyhow!(
            "modeswitch script failed (exit {:?})",
            out.status.code()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn stop_modemmanager() -> Result<()> {
    let ok = Command::new("sudo")
        .args(["systemctl", "stop", "ModemManager"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err(anyhow!("could not stop ModemManager to free the AT port"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn megafon() -> Profile {
        Profile {
            name: "megafon".into(),
            apn: "internet".into(),
            autoconnect: true,
            home_only: false,
            username: None,
            password: None,
        }
    }

    #[test]
    fn parses_minimal_profile_with_defaults() {
        let cfg = parse_config(
            r#"
            [[profile]]
            name = "megafon"
            apn = "internet"
        "#,
        )
        .unwrap();
        assert_eq!(cfg.profiles.len(), 1);
        let p = &cfg.profiles[0];
        assert_eq!(p.apn, "internet");
        assert!(p.autoconnect, "autoconnect defaults to true");
        assert!(!p.home_only, "home_only defaults to false");
    }

    #[test]
    fn empty_config_yields_no_profiles() {
        assert!(parse_config("").unwrap().profiles.is_empty());
    }

    #[test]
    fn conn_name_is_prefixed() {
        assert_eq!(megafon().conn_name(), "sy-wwan-megafon");
    }

    #[test]
    fn add_args_declare_type_apn_and_ifname_wildcard() {
        let a = megafon().add_args();
        assert_eq!(a[0..4], ["connection", "add", "type", "gsm"]);
        // con-name and ifname wildcard are present
        assert!(a.windows(2).any(|w| w == ["con-name", "sy-wwan-megafon"]));
        assert!(a.windows(2).any(|w| w == ["ifname", "*"]));
        assert!(a.windows(2).any(|w| w == ["gsm.apn", "internet"]));
        assert!(a.windows(2).any(|w| w == ["connection.autoconnect", "yes"]));
        assert!(a.windows(2).any(|w| w == ["gsm.home-only", "no"]));
    }

    #[test]
    fn modify_args_target_the_named_connection() {
        let a = megafon().modify_args();
        assert_eq!(a[0..3], ["connection", "modify", "sy-wwan-megafon"]);
        assert!(a.windows(2).any(|w| w == ["gsm.apn", "internet"]));
    }

    #[test]
    fn home_only_and_autoconnect_flags_serialise() {
        let p = Profile {
            home_only: true,
            autoconnect: false,
            ..megafon()
        };
        let a = p.add_args();
        assert!(a.windows(2).any(|w| w == ["gsm.home-only", "yes"]));
        assert!(a.windows(2).any(|w| w == ["connection.autoconnect", "no"]));
    }

    #[test]
    fn credentials_clear_when_unset() {
        // Unset creds serialise as empty strings so modify mirrors config.
        let a = megafon().setting_pairs();
        assert!(a.windows(2).any(|w| w == ["gsm.username", ""]));
        assert!(a.windows(2).any(|w| w == ["gsm.password", ""]));
    }

    #[test]
    fn credentials_pass_through_when_set() {
        let p = Profile {
            username: Some("user".into()),
            password: Some("pw".into()),
            ..megafon()
        };
        let a = p.setting_pairs();
        assert!(a.windows(2).any(|w| w == ["gsm.username", "user"]));
        assert!(a.windows(2).any(|w| w == ["gsm.password", "pw"]));
    }

    #[test]
    fn field_extracts_mmcli_block_values() {
        let block = "  Status   |         state: connected\n           | signal quality: 41% (recent)";
        assert_eq!(field(block, "state").as_deref(), Some("connected"));
        assert_eq!(field(block, "signal quality").as_deref(), Some("41% (recent)"));
    }
}
