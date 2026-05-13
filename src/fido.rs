//! FIDO/U2F auth for swaylock via pam_u2f.
//!
//! Manages `/etc/pam.d/swaylock` declaratively. Subcommands:
//!   sy fido enable   — wire pam_u2f as `sufficient`, password as fallback
//!   sy fido disable  — restore the stock include-login-only PAM file
//!   sy fido status   — print enabled/disabled + registered key count
//!
//! Key registration lives in `~/.config/Yubico/u2f_keys` (default location
//! pam_u2f reads). Generate with:
//!   pamu2fcfg > ~/.config/Yubico/u2f_keys
//! and append further keys with `pamu2fcfg -n >> ~/.config/Yubico/u2f_keys`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};

const PAM_FILE: &str = "/etc/pam.d/swaylock";

const PAM_FIDO: &str = "# managed by `sy fido` — do not edit by hand
# pam_u2f succeeds on YubiKey touch; falls through to login (password) on
# absence/timeout, so you can still log in if the key is unplugged.
auth sufficient pam_u2f.so cue
auth include login
";

const PAM_STOCK: &str = "#
# PAM configuration file for the swaylock screen locker. By default, it includes
# the 'login' configuration file (see /etc/pam.d/login)
#

auth include login
";

pub fn run(action: Option<&str>) -> Result<()> {
    match action.unwrap_or("status") {
        "enable" => enable(),
        "disable" => disable(),
        "status" => status(),
        other => Err(anyhow!(
            "unknown fido action: {other} (enable|disable|status)"
        )),
    }
}

fn enable() -> Result<()> {
    ensure_pam_u2f_installed()?;
    ensure_keys_registered()?;
    if write_pam(PAM_FIDO)? {
        println!("fido auth enabled for swaylock ({PAM_FILE})");
    } else {
        println!("fido auth already enabled for swaylock");
    }
    println!("test: lock the screen, press Enter (empty password), touch your key");
    Ok(())
}

fn disable() -> Result<()> {
    if write_pam(PAM_STOCK)? {
        println!("fido auth disabled for swaylock; password-only");
    } else {
        println!("fido auth already disabled for swaylock");
    }
    Ok(())
}

fn status() -> Result<()> {
    let cur = fs::read_to_string(PAM_FILE).unwrap_or_default();
    let enabled = cur.contains("pam_u2f.so");
    let kp = keys_path();
    let keys = fs::read_to_string(&kp).unwrap_or_default();
    let users: Vec<&str> = keys
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split(':').next())
        .collect();
    println!(
        "pam:  {} ({PAM_FILE})",
        if enabled { "enabled" } else { "disabled" }
    );
    println!("keys: {} ({} users)", kp.display(), users.len());
    for u in users {
        println!("      - {u}");
    }
    Ok(())
}

fn write_pam(content: &str) -> Result<bool> {
    if Path::new(PAM_FILE).exists() {
        let cur = fs::read_to_string(PAM_FILE).unwrap_or_default();
        if cur == content {
            return Ok(false);
        }
    }
    let mut child = Command::new("sudo")
        .args(["tee", PAM_FILE])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn `sudo tee {PAM_FILE}`"))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("sudo tee: no stdin"))?;
        stdin.write_all(content.as_bytes())?;
    }
    let status = child.wait().context("wait sudo tee")?;
    if !status.success() {
        return Err(anyhow!(
            "`sudo tee {PAM_FILE}` failed (exit {:?})",
            status.code()
        ));
    }
    Ok(true)
}

fn ensure_pam_u2f_installed() -> Result<()> {
    let ok = Command::new("rpm")
        .args(["-q", "pam-u2f"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        return Ok(());
    }
    Err(anyhow!(
        "pam-u2f not installed. install with:\n  sudo dnf install -y pam-u2f"
    ))
}

fn keys_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/Yubico/u2f_keys")
}

fn ensure_keys_registered() -> Result<()> {
    let p = keys_path();
    let s = fs::read_to_string(&p).unwrap_or_default();
    let has_entry = s
        .lines()
        .any(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'));
    if has_entry {
        return Ok(());
    }
    Err(anyhow!(
        "no FIDO keys registered at {disp}\nregister with:\n  mkdir -p {dir}\n  pamu2fcfg > {disp}",
        disp = p.display(),
        dir = p.parent().unwrap_or(Path::new(".")).display(),
    ))
}
