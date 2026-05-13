use std::{collections::HashSet, process::Command};

use anyhow::Result;

use crate::{popup, wifi};

enum Action {
    Noop,
    ToggleWifi(bool),
    ToggleNet(bool),
    Nmtui,
    ConnUp(String),
    ConnDown(String),
    Wifi(String),
}

/// Fuzzel-based network control dropdown: status, toggles, VPNs, wi-fi.
pub fn menu() -> Result<()> {
    let wifi_on = radio_enabled("wifi");
    let net_on = networking_enabled();
    let active = active_connections();

    // Fire-and-forget rescan so the next open shows fresher results;
    // the current menu uses the cache so fuzzel appears instantly.
    if wifi_on {
        let _ = Command::new("nmcli")
            .args(["dev", "wifi", "rescan"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    let wifi_list = if wifi_on { wifi::list() } else { Vec::new() };
    let vpns = saved_vpns(&active);

    let mut items: Vec<(String, Action)> = Vec::new();

    if active.is_empty() {
        items.push(("  (disconnected)".into(), Action::Noop));
    } else {
        for (name, typ, dev) in &active {
            items.push((
                format!("* {:<10} {}  [{}]", typ, name, dev),
                Action::ConnDown(name.clone()),
            ));
        }
    }

    items.push((
        format!(
            "  wi-fi       {}",
            if wifi_on {
                "on   (click to disable)"
            } else {
                "off  (click to enable)"
            }
        ),
        Action::ToggleWifi(wifi_on),
    ));
    items.push((
        format!(
            "  networking  {}",
            if net_on {
                "on   (click to disable)"
            } else {
                "off  (click to enable)"
            }
        ),
        Action::ToggleNet(net_on),
    ));
    items.push(("  nmtui…".into(), Action::Nmtui));

    for (name, up) in &vpns {
        let mark = if *up { "*" } else { " " };
        let verb = if *up { "down" } else { "up  " };
        items.push((
            format!("{mark} vpn  {verb}  {name}"),
            if *up {
                Action::ConnDown(name.clone())
            } else {
                Action::ConnUp(name.clone())
            },
        ));
    }

    for (active, ssid, meta) in &wifi_list {
        let mark = if *active { "*" } else { " " };
        items.push((
            format!("{mark} wifi {ssid:<24} {meta}"),
            Action::Wifi(ssid.clone()),
        ));
    }

    let input: String = items
        .iter()
        .map(|(l, _)| l.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let choice = wifi::run_fuzzel(&input, "net » ", false)?;
    let choice = choice.trim_end_matches('\n');
    if choice.is_empty() {
        return Ok(());
    }

    let Some((_, action)) = items.into_iter().find(|(l, _)| l == choice) else {
        return Ok(());
    };

    match action {
        Action::Noop => Ok(()),
        Action::ToggleWifi(on) => {
            let v = if on { "off" } else { "on" };
            let ok = Command::new("nmcli")
                .args(["radio", "wifi", v])
                .status()?
                .success();
            wifi::notify(
                "net",
                &format!("wi-fi {v}{}", if ok { "" } else { " (failed)" }),
            );
            Ok(())
        }
        Action::ToggleNet(on) => {
            let v = if on { "off" } else { "on" };
            let ok = Command::new("nmcli")
                .args(["networking", v])
                .status()?
                .success();
            wifi::notify(
                "net",
                &format!("networking {v}{}", if ok { "" } else { " (failed)" }),
            );
            Ok(())
        }
        Action::Nmtui => popup::toggle("nmtui"),
        Action::ConnUp(name) => {
            let ok = Command::new("nmcli")
                .args(["connection", "up", "id", &name])
                .status()?
                .success();
            wifi::notify(
                "net",
                &format!("{name} up{}", if ok { "" } else { " (failed)" }),
            );
            Ok(())
        }
        Action::ConnDown(name) => {
            let ok = Command::new("nmcli")
                .args(["connection", "down", "id", &name])
                .status()?
                .success();
            wifi::notify(
                "net",
                &format!("{name} down{}", if ok { "" } else { " (failed)" }),
            );
            Ok(())
        }
        Action::Wifi(ssid) => wifi::connect(&ssid),
    }
}

fn radio_enabled(kind: &str) -> bool {
    Command::new("nmcli")
        .args(["radio", kind])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "enabled")
        .unwrap_or(false)
}

fn networking_enabled() -> bool {
    Command::new("nmcli")
        .arg("networking")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "enabled")
        .unwrap_or(false)
}

fn active_connections() -> Vec<(String, String, String)> {
    let Some(out) = Command::new("nmcli")
        .args([
            "-t",
            "-f",
            "NAME,TYPE,DEVICE",
            "connection",
            "show",
            "--active",
        ])
        .output()
        .ok()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let p = wifi::parse_colon_fields(l);
            if p.len() < 3 {
                return None;
            }
            Some((p[0].clone(), p[1].clone(), p[2].clone()))
        })
        .filter(|(_, t, _)| t != "loopback")
        .collect()
}

fn saved_vpns(active: &[(String, String, String)]) -> Vec<(String, bool)> {
    let Some(out) = Command::new("nmcli")
        .args(["-t", "-f", "NAME,TYPE", "connection", "show"])
        .output()
        .ok()
    else {
        return Vec::new();
    };
    let up: HashSet<&str> = active.iter().map(|(n, _, _)| n.as_str()).collect();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let p = wifi::parse_colon_fields(l);
            if p.len() < 2 {
                return None;
            }
            let is_vpn = p[1] == "vpn" || p[1] == "wireguard";
            is_vpn.then(|| (p[0].clone(), up.contains(p[0].as_str())))
        })
        .collect()
}
