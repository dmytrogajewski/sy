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

// ---- captive-portal / connectivity indicator (waybar tile) ----

/// NetworkManager per-device connectivity classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Conn {
    None,
    Unknown,
    Portal,
    Limited,
    Full,
}

impl Conn {
    fn parse(s: &str) -> Conn {
        match s.trim() {
            "full" => Conn::Full,
            "limited" => Conn::Limited,
            "portal" => Conn::Portal,
            "none" => Conn::None,
            _ => Conn::Unknown,
        }
    }

    /// A device with any of these states is up and worth judging; `none`/
    /// `unknown` means down or not-yet-probed, which is not a portal signal.
    fn is_live(self) -> bool {
        matches!(self, Conn::Full | Conn::Portal | Conn::Limited)
    }
}

/// A connected uplink device and its per-family connectivity.
struct DevConn {
    kind: String, // wifi | gsm | ethernet | tun | …
    name: String, // active connection name (watson, sy-wwan-megafon)
    ip4: Conn,
    ip6: Conn,
}

/// What the portal tile should render.
enum Portal {
    /// You have real internet (or nothing is connected) — hide the tile.
    Ok,
    /// One or more uplinks are behind a captive portal (login/top-up needed).
    Captive(Vec<String>),
    /// Connected but no route to the internet, with no portal detected.
    NoInternet(Vec<String>),
}

/// Decide the portal-tile state from NM's per-device connectivity. Only real
/// uplinks (wifi/gsm/ethernet) count — a VPN `tun` reporting `limited` is not a
/// captive-portal signal — and a device counts as online if *either* IPv4 or
/// IPv6 is `full` (IPv6 is routinely `limited` on working networks, so keying
/// on it alone would false-positive).
fn classify_portal(devs: &[DevConn]) -> Portal {
    let uplinks: Vec<&DevConn> = devs
        .iter()
        .filter(|d| matches!(d.kind.as_str(), "wifi" | "gsm" | "ethernet"))
        .filter(|d| d.ip4.is_live() || d.ip6.is_live())
        .collect();
    if uplinks.is_empty() {
        return Portal::Ok; // disconnected — the `network` tile covers that
    }
    if uplinks
        .iter()
        .any(|d| d.ip4 == Conn::Full || d.ip6 == Conn::Full)
    {
        return Portal::Ok;
    }
    let label = |d: &DevConn| {
        if d.name.is_empty() || d.name == "--" {
            d.kind.clone()
        } else {
            format!("{} ({})", d.kind, d.name)
        }
    };
    let captive: Vec<String> = uplinks
        .iter()
        .filter(|d| d.ip4 == Conn::Portal || d.ip6 == Conn::Portal)
        .map(|d| label(d))
        .collect();
    if !captive.is_empty() {
        return Portal::Captive(captive);
    }
    Portal::NoInternet(uplinks.iter().map(|d| label(d)).collect())
}

/// Render the portal state as a waybar custom-module JSON line. `Ok` emits
/// empty text + `hidden` class so the tile collapses to zero width.
fn portal_json(p: &Portal) -> String {
    use serde_json::json;
    let doc = match p {
        Portal::Ok => json!({"text": "", "class": "hidden"}),
        Portal::Captive(list) => json!({
            "text": "⚠",
            "class": "portal",
            "tooltip": format!(
                "captive portal — login / top-up required:\n  {}\nclick: open portal",
                list.join("\n  ")
            ),
        }),
        Portal::NoInternet(list) => json!({
            "text": "⚠",
            "class": "limited",
            "tooltip": format!("connected but no internet:\n  {}", list.join("\n  ")),
        }),
    };
    doc.to_string()
}

fn device_connectivity() -> Vec<DevConn> {
    let Some(out) = Command::new("nmcli")
        .args([
            "-t",
            "-f",
            "DEVICE,TYPE,CONNECTION,IP4-CONNECTIVITY,IP6-CONNECTIVITY",
            "device",
            "status",
        ])
        .output()
        .ok()
    else {
        return Vec::new();
    };
    parse_device_connectivity(&String::from_utf8_lossy(&out.stdout))
}

fn parse_device_connectivity(out: &str) -> Vec<DevConn> {
    out.lines()
        .filter_map(|l| {
            let p = wifi::parse_colon_fields(l);
            if p.len() < 5 {
                return None;
            }
            Some(DevConn {
                kind: p[1].clone(),
                name: p[2].clone(),
                ip4: Conn::parse(&p[3]),
                ip6: Conn::parse(&p[4]),
            })
        })
        .collect()
}

/// `sy net` entry point: `--waybar` prints the captive-portal tile JSON,
/// otherwise open the interactive dropdown.
pub fn run(waybar: bool) -> Result<()> {
    if waybar {
        println!("{}", portal_json(&classify_portal(&device_connectivity())));
        return Ok(());
    }
    menu()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(kind: &str, name: &str, ip4: Conn, ip6: Conn) -> DevConn {
        DevConn {
            kind: kind.into(),
            name: name.into(),
            ip4,
            ip6,
        }
    }

    #[test]
    fn captive_portal_on_wifi_is_flagged() {
        let devs = vec![dev("wifi", "watson", Conn::Portal, Conn::None)];
        assert!(matches!(classify_portal(&devs), Portal::Captive(l) if l == ["wifi (watson)"]));
    }

    #[test]
    fn captive_portal_on_gsm_is_flagged() {
        let devs = vec![dev("gsm", "sy-wwan-megafon", Conn::Portal, Conn::None)];
        assert!(
            matches!(classify_portal(&devs), Portal::Captive(l) if l == ["gsm (sy-wwan-megafon)"])
        );
    }

    #[test]
    fn ipv4_full_with_ipv6_limited_is_ok_not_a_portal() {
        // The common real-world case: working wifi, IPv6 only `limited`.
        let devs = vec![dev("wifi", "watson", Conn::Full, Conn::Limited)];
        assert!(matches!(classify_portal(&devs), Portal::Ok));
    }

    #[test]
    fn any_uplink_with_full_internet_hides_the_tile() {
        let devs = vec![
            dev("wifi", "watson", Conn::Full, Conn::None),
            dev("gsm", "sy-wwan-megafon", Conn::Portal, Conn::None),
        ];
        assert!(matches!(classify_portal(&devs), Portal::Ok));
    }

    #[test]
    fn connected_without_internet_reports_no_internet() {
        let devs = vec![dev("wifi", "watson", Conn::Limited, Conn::None)];
        assert!(matches!(classify_portal(&devs), Portal::NoInternet(l) if l == ["wifi (watson)"]));
    }

    #[test]
    fn vpn_tun_limited_is_not_a_portal_signal() {
        let devs = vec![
            dev("wifi", "watson", Conn::Full, Conn::None),
            dev("tun", "prrr0", Conn::Limited, Conn::None),
        ];
        assert!(matches!(classify_portal(&devs), Portal::Ok));
        // and with no real uplink online, a lone tun never triggers the tile
        let only_tun = vec![dev("tun", "prrr0", Conn::Limited, Conn::None)];
        assert!(matches!(classify_portal(&only_tun), Portal::Ok));
    }

    #[test]
    fn nothing_connected_hides_the_tile() {
        let devs = vec![dev("wifi", "--", Conn::None, Conn::None)];
        assert!(matches!(classify_portal(&devs), Portal::Ok));
    }

    #[test]
    fn parse_device_connectivity_reads_nmcli_terse() {
        // wifi limited (no internet), gsm behind a portal, loopback ignored.
        let out = "wlp99s0:wifi:watson:limited:limited\ncdc-wdm0:gsm:sy-wwan-megafon:portal:none\nlo:loopback::unknown:unknown\n";
        let devs = parse_device_connectivity(out);
        assert_eq!(devs.len(), 3);
        assert_eq!(devs[1].kind, "gsm");
        assert_eq!(devs[1].ip4, Conn::Portal);
        // parse feeds the classifier: only the portal device is named.
        assert!(matches!(
            classify_portal(&devs),
            Portal::Captive(l) if l == ["gsm (sy-wwan-megafon)"]
        ));
    }

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
