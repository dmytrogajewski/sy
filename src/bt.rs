use std::process::Command;

use anyhow::{Context, Result};

use crate::wifi;

/// Main dispatcher. `waybar` → JSON output for the bar; else → fuzzel menu.
pub fn run(waybar: bool) -> Result<()> {
    if waybar {
        waybar_out()
    } else {
        menu()
    }
}

// -- state ------------------------------------------------------------------

fn is_powered() -> bool {
    let out = Command::new("bluetoothctl").arg("show").output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).contains("Powered: yes"),
        Err(_) => false,
    }
}

fn devices(scope: &str) -> Vec<(String, String)> {
    let mut cmd = Command::new("bluetoothctl");
    cmd.arg("devices");
    if !scope.is_empty() {
        cmd.arg(scope);
    }
    let out = match cmd.output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(parse_device)
        .collect()
}

fn parse_device(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("Device ")?;
    let (mac, name) = rest.split_once(' ')?;
    Some((mac.to_string(), name.to_string()))
}

/// Resolve a human-friendly display name for a bluetooth device.
///
/// `bluetoothctl devices Connected` returns the device's `Name`
/// field verbatim — and when the peer hasn't advertised one yet,
/// or just after pairing, that field is set to the MAC itself
/// (`AA:BB:CC:DD:EE:FF`). The bar then shows a hex blob instead of
/// the actual device name. This helper checks for that case and
/// falls back to `bluetoothctl info <MAC>` to read `Alias:` (the
/// user-overridable label, preferred over `Name:`). If both still
/// look like a MAC or are empty, returns `device <last-octets>` so
/// the bar at least shows something compact and consistent.
fn display_name(mac: &str, raw_name: &str) -> String {
    if !is_mac_like(raw_name) && !raw_name.is_empty() {
        return raw_name.to_string();
    }
    let info = Command::new("bluetoothctl")
        .args(["info", mac])
        .output()
        .ok();
    if let Some(o) = info {
        let body = String::from_utf8_lossy(&o.stdout);
        for key in ["Alias:", "Name:"] {
            if let Some(v) = body
                .lines()
                .find_map(|l| l.trim().strip_prefix(key))
                .map(|s| s.trim().to_string())
            {
                if !v.is_empty() && !is_mac_like(&v) {
                    return v;
                }
            }
        }
    }
    // Last-resort fallback: compact MAC tail so the bar doesn't
    // emit the full 17-char hex string. `AA:BB:CC:DD:EE:FF` →
    // `device EE:FF`.
    let tail = mac
        .split(':')
        .rev()
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(":");
    if tail.is_empty() {
        "device".to_string()
    } else {
        format!("device {tail}")
    }
}

/// MAC pattern `XX:XX:XX:XX:XX:XX` (case-insensitive hex octets).
/// Used by [`display_name`] to detect bluetoothctl's "no name yet"
/// fallback. Pure-fn so the unit tests don't need bluez.
fn is_mac_like(s: &str) -> bool {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6 {
        return false;
    }
    parts
        .iter()
        .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Cap the bar-text portion of a device name to two whitespace-
/// separated words. The full name still surfaces in the tooltip;
/// the bar just gets the leading two tokens so a long peer name
/// (`Galaxy S25 Ultra пользователя Dmytro`) doesn't crowd out the
/// network / notif / clock tiles to its right.
fn two_words(s: &str) -> String {
    s.split_whitespace().take(2).collect::<Vec<_>>().join(" ")
}

// -- waybar -----------------------------------------------------------------

fn waybar_out() -> Result<()> {
    if !is_powered() {
        println!(
            r#"{{"text":"BT:off","class":"off","tooltip":"Bluetooth off\nclick: open menu"}}"#
        );
        return Ok(());
    }
    let connected = devices("Connected");
    let (text, class, tip) = if let Some((mac, raw_name)) = connected.first() {
        let full = display_name(mac, raw_name);
        let short = truncate(&two_words(&full), 16);
        (
            format!("BT:{short}"),
            "connected",
            format!("Connected: {full}"),
        )
    } else {
        (
            "BT:on".to_string(),
            "on",
            "Bluetooth on, no device connected".to_string(),
        )
    };
    let tip = format!("{tip}\\nclick: menu · right-click: toggle");
    println!(r#"{{"text":"{text}","class":"{class}","tooltip":"{tip}"}}"#);
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

// -- menu -------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Action {
    Noop,
    Toggle,
    Connect(String),
    Disconnect(String),
    Forget(String),
    Scan,
}

fn menu() -> Result<()> {
    let powered = is_powered();

    let mut items: Vec<(String, Action)> = Vec::new();

    if powered {
        items.push(("bt power: on   → disable".to_string(), Action::Toggle));
    } else {
        items.push(("bt power: off  → enable".to_string(), Action::Toggle));
    }

    if powered {
        let connected = devices("Connected");
        let paired = devices("Paired");

        if !connected.is_empty() {
            items.push(("── connected ──".to_string(), Action::Noop));
            for (mac, name) in &connected {
                items.push((format!("● {name}"), Action::Disconnect(mac.clone())));
            }
        }

        let other: Vec<_> = paired
            .iter()
            .filter(|(m, _)| !connected.iter().any(|(cm, _)| cm == m))
            .collect();
        if !other.is_empty() {
            items.push(("── paired ──".to_string(), Action::Noop));
            for (mac, name) in other {
                items.push((format!("○ {name}"), Action::Connect(mac.clone())));
            }
        }

        items.push(("──".to_string(), Action::Noop));
        items.push(("scan & pair new…".to_string(), Action::Scan));
        items.push(("──".to_string(), Action::Noop));
        for (mac, name) in &paired {
            items.push((format!("forget: {name}"), Action::Forget(mac.clone())));
        }
    }

    let input = items
        .iter()
        .map(|(l, _)| l.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let picked = wifi::run_fuzzel(&input, "bt » ", false)?;
    let picked = picked.trim();
    if picked.is_empty() {
        return Ok(());
    }

    let action = items
        .into_iter()
        .find(|(l, _)| l == picked)
        .map(|(_, a)| a)
        .unwrap_or(Action::Noop);

    dispatch(action)
}

fn dispatch(a: Action) -> Result<()> {
    match a {
        Action::Noop => Ok(()),
        Action::Toggle => {
            let target = if is_powered() { "off" } else { "on" };
            Command::new("bluetoothctl")
                .args(["power", target])
                .status()
                .context("power")?;
            notify(&format!("Bluetooth {target}"));
            Ok(())
        }
        Action::Connect(mac) => connect(&mac),
        Action::Disconnect(mac) => {
            let _ = Command::new("bluetoothctl")
                .args(["disconnect", &mac])
                .status();
            notify("Disconnected");
            Ok(())
        }
        Action::Forget(mac) => {
            let _ = Command::new("bluetoothctl").args(["remove", &mac]).status();
            notify("Forgot device");
            Ok(())
        }
        Action::Scan => scan_and_pair(),
    }
}

fn connect(mac: &str) -> Result<()> {
    notify("Connecting…");
    let _ = Command::new("bluetoothctl").args(["trust", mac]).status();
    let out = Command::new("bluetoothctl")
        .args(["connect", mac])
        .output()?;
    if out.status.success() {
        notify("Connected");
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let msg = if err.is_empty() {
            String::from_utf8_lossy(&out.stdout).to_string()
        } else {
            err.to_string()
        };
        notify(&format!(
            "Connect failed: {}",
            msg.trim().lines().next().unwrap_or("")
        ));
    }
    Ok(())
}

fn scan_and_pair() -> Result<()> {
    notify("Scanning (8s)…");
    let _ = Command::new("bluetoothctl")
        .args(["--timeout", "8", "scan", "on"])
        .status();

    let all = devices("");
    let paired = devices("Paired");
    let new_only: Vec<_> = all
        .into_iter()
        .filter(|(m, _)| !paired.iter().any(|(pm, _)| pm == m))
        .collect();

    if new_only.is_empty() {
        notify("No new devices found");
        return Ok(());
    }

    let lines = new_only
        .iter()
        .map(|(m, n)| format!("{n}  [{m}]"))
        .collect::<Vec<_>>()
        .join("\n");
    let picked = wifi::run_fuzzel(&lines, "pair » ", false)?;
    let picked = picked.trim();
    if picked.is_empty() {
        return Ok(());
    }

    let mac = new_only
        .iter()
        .find(|(m, _)| picked.contains(m.as_str()))
        .map(|(m, _)| m.clone());
    let Some(mac) = mac else {
        notify("Could not resolve pick");
        return Ok(());
    };

    notify("Pairing…");
    let _ = Command::new("bluetoothctl").args(["pair", &mac]).status();
    let _ = Command::new("bluetoothctl").args(["trust", &mac]).status();
    connect(&mac)
}

fn notify(body: &str) {
    let _ = Command::new("notify-send")
        .args(["-a", "sy", "-t", "1200", "bluetooth", body])
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_mac_like_recognises_canonical_form() {
        assert!(is_mac_like("AA:BB:CC:DD:EE:FF"));
        assert!(is_mac_like("aa:bb:cc:dd:ee:ff"));
        assert!(is_mac_like("BA:34:12:34:56:78"));
    }

    #[test]
    fn is_mac_like_rejects_human_names_and_partials() {
        assert!(!is_mac_like(""));
        assert!(!is_mac_like("Galaxy S25 Ultra"));
        assert!(!is_mac_like("Pixel Buds Pro"));
        // 5 octets — too short
        assert!(!is_mac_like("AA:BB:CC:DD:EE"));
        // non-hex
        assert!(!is_mac_like("AA:BB:CC:DD:EE:ZZ"));
        // 7 octets — too long
        assert!(!is_mac_like("AA:BB:CC:DD:EE:FF:00"));
    }

    #[test]
    fn two_words_caps_at_two_tokens() {
        assert_eq!(two_words("Galaxy S25 Ultra"), "Galaxy S25");
        assert_eq!(two_words("S25 Ultra пользователя Dmytro"), "S25 Ultra");
    }

    #[test]
    fn two_words_passes_through_short_names() {
        assert_eq!(two_words("AirPods"), "AirPods");
        assert_eq!(two_words("Pixel Buds"), "Pixel Buds");
        assert_eq!(two_words(""), "");
    }

    #[test]
    fn two_words_collapses_internal_whitespace() {
        assert_eq!(two_words("  Pixel    Buds   Pro  "), "Pixel Buds");
    }
}
