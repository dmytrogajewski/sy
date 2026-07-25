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
    WwanUp(String),
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

    for (name, up) in saved_wwan(&active) {
        let mark = if up { "*" } else { " " };
        let verb = if up { "down" } else { "up  " };
        items.push((
            format!("{mark} wwan {verb}  {name}"),
            if up {
                Action::ConnDown(name.clone())
            } else {
                Action::WwanUp(name.clone())
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
        Action::WwanUp(name) => {
            // A gsm device stays `unavailable` while the wwan radio is off, so
            // the activation would fail with "no suitable device". Flip it on
            // first (idempotent) before dialling.
            let _ = Command::new("nmcli")
                .args(["radio", "wwan", "on"])
                .status();
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
    saved_by_type(active, |t| t == "vpn" || t == "wireguard")
}

fn saved_wwan(active: &[(String, String, String)]) -> Vec<(String, bool)> {
    saved_by_type(active, |t| t == "gsm")
}

/// List saved connections whose TYPE matches `want`, each paired with whether
/// it is currently active. Shared by the VPN and WWAN menu sections.
fn saved_by_type(
    active: &[(String, String, String)],
    want: impl Fn(&str) -> bool,
) -> Vec<(String, bool)> {
    let Some(out) = Command::new("nmcli")
        .args(["-t", "-f", "NAME,TYPE", "connection", "show"])
        .output()
        .ok()
    else {
        return Vec::new();
    };
    let up: HashSet<&str> = active.iter().map(|(n, _, _)| n.as_str()).collect();
    parse_saved_by_type(&String::from_utf8_lossy(&out.stdout), &up, &want)
}

/// Pure parser for `nmcli -t -f NAME,TYPE connection show` output: keep the
/// rows whose type satisfies `want`, tagging each with its active state.
fn parse_saved_by_type(
    out: &str,
    up: &HashSet<&str>,
    want: &impl Fn(&str) -> bool,
) -> Vec<(String, bool)> {
    out.lines()
        .filter_map(|l| {
            let p = wifi::parse_colon_fields(l);
            if p.len() < 2 {
                return None;
            }
            want(&p[1]).then(|| (p[0].clone(), up.contains(p[0].as_str())))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wwan_rows_are_selected_and_active_state_tagged() {
        let out = "watson:802-11-wireless\nsy-wwan-megafon:gsm\nDigitalocean-USA:vpn\n";
        let up: HashSet<&str> = ["sy-wwan-megafon"].into_iter().collect();
        let gsm = parse_saved_by_type(out, &up, &|t| t == "gsm");
        assert_eq!(gsm, vec![("sy-wwan-megafon".to_string(), true)]);
    }

    #[test]
    fn inactive_wwan_is_offered_as_bring_up() {
        let out = "sy-wwan-megafon:gsm\n";
        let up: HashSet<&str> = HashSet::new();
        let gsm = parse_saved_by_type(out, &up, &|t| t == "gsm");
        assert_eq!(gsm, vec![("sy-wwan-megafon".to_string(), false)]);
    }

    #[test]
    fn vpn_and_wireguard_types_match_but_gsm_does_not() {
        let out = "wg0:wireguard\ncorp:vpn\nsy-wwan-megafon:gsm\n";
        let up: HashSet<&str> = HashSet::new();
        let vpns = parse_saved_by_type(out, &up, &|t| t == "vpn" || t == "wireguard");
        let names: Vec<&str> = vpns.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["wg0", "corp"]);
    }
}
