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

/// A bluetooth device enriched with a friendly name and signal.
#[derive(Debug, Clone)]
struct Device {
    mac: String,
    /// Resolved human-friendly name (never a bare MAC).
    name: String,
    /// Signal strength in dBm — higher (closer to 0) is stronger.
    /// `None` when the peer isn't currently in range / advertising,
    /// in which case bluez reports no RSSI.
    rssi: Option<i32>,
}

/// Parse a single `RSSI:` line from `bluetoothctl info`. bluez prints
/// either a plain signed value (`RSSI: -53`) or the legacy hex form
/// with the decimal in parens (`RSSI: 0xffffffcb (-53)`); accept both.
fn parse_rssi(line: &str) -> Option<i32> {
    let v = line.trim().strip_prefix("RSSI:")?.trim();
    if let Some(start) = v.find('(') {
        return v[start + 1..].split(')').next()?.trim().parse::<i32>().ok();
    }
    if let Some(hex) = v.strip_prefix("0x") {
        return u32::from_str_radix(hex, 16).ok().map(|u| u as i32);
    }
    v.split_whitespace().next()?.parse::<i32>().ok()
}

/// Pull the best name (`Alias:` over `Name:`, ignoring MAC-like
/// values) and the RSSI out of a `bluetoothctl info` body.
fn parse_info(body: &str) -> (Option<String>, Option<i32>) {
    let (mut name, mut alias, mut rssi) = (None, None, None);
    for line in body.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("Alias:") {
            let v = v.trim();
            if !v.is_empty() && !is_mac_like(v) {
                alias = Some(v.to_string());
            }
        } else if let Some(v) = t.strip_prefix("Name:") {
            let v = v.trim();
            if !v.is_empty() && !is_mac_like(v) {
                name = Some(v.to_string());
            }
        } else if t.starts_with("RSSI:") {
            rssi = parse_rssi(t).or(rssi);
        }
    }
    (alias.or(name), rssi)
}

/// Run `bluetoothctl info <MAC>` once and return (best name, RSSI).
fn fetch_info(mac: &str) -> (Option<String>, Option<i32>) {
    match Command::new("bluetoothctl").args(["info", mac]).output() {
        Ok(o) => parse_info(&String::from_utf8_lossy(&o.stdout)),
        Err(_) => (None, None),
    }
}

/// `AA:BB:CC:DD:EE:FF` → `device EE:FF`. Last-resort label so we
/// never surface the full 17-char hex blob.
fn mac_tail_name(mac: &str) -> String {
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

/// Resolve a human-friendly display name for a bluetooth device.
///
/// `bluetoothctl devices` returns the device's `Name` field verbatim
/// — and when the peer hasn't advertised one yet, or just after
/// pairing, that field is set to the MAC itself. This checks for that
/// case and falls back to `bluetoothctl info <MAC>` (`Alias:`/`Name:`),
/// then to a compact MAC tail so the bar always shows something.
fn display_name(mac: &str, raw_name: &str) -> String {
    if !is_mac_like(raw_name) && !raw_name.is_empty() {
        return raw_name.to_string();
    }
    if let (Some(name), _) = fetch_info(mac) {
        return name;
    }
    mac_tail_name(mac)
}

/// Enrich a raw (MAC, name) pair into a [`Device`]: a friendly name
/// (resolving MAC-like names via bluez) plus its current RSSI.
fn resolve(mac: &str, raw_name: &str) -> Device {
    let good = (!is_mac_like(raw_name) && !raw_name.is_empty()).then(|| raw_name.to_string());
    let (info_name, rssi) = fetch_info(mac);
    let name = good.or(info_name).unwrap_or_else(|| mac_tail_name(mac));
    Device {
        mac: mac.to_string(),
        name,
        rssi,
    }
}

/// Sort strongest signal first; devices with no RSSI (out of range /
/// not advertising) sink to the bottom, ties break by name so the
/// order is stable across menu redraws.
fn sort_by_signal(devs: &mut [Device]) {
    devs.sort_by(|a, b| b.rssi.cmp(&a.rssi).then_with(|| a.name.cmp(&b.name)));
}

/// Resolve every raw device and return them sorted strongest-first.
fn resolved_sorted(raw: &[(String, String)]) -> Vec<Device> {
    let mut v: Vec<Device> = raw.iter().map(|(m, n)| resolve(m, n)).collect();
    sort_by_signal(&mut v);
    v
}

/// Compact 4-bar signal indicator from RSSI (dBm). `None` → unknown.
fn signal_glyph(rssi: Option<i32>) -> &'static str {
    match rssi {
        Some(r) if r >= -50 => "▰▰▰▰",
        Some(r) if r >= -65 => "▰▰▰▱",
        Some(r) if r >= -78 => "▰▰▱▱",
        Some(_) => "▰▱▱▱",
        None => "▱▱▱▱",
    }
}

/// Strip CSI escape sequences (`\x1b[...m`) from a line. `bluetoothctl`
/// colours its `[NEW]`/`[CHG]` event tags even when stdout is piped.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Build the discovered-device list from a `bluetoothctl scan` stdout
/// capture. RSSI is only published as live `[CHG] Device <MAC> RSSI:`
/// events *during* discovery, so we read it straight from the stream
/// rather than querying `info` after the scan has ended (by which
/// point bluez has dropped it). `[NEW]`/`[CHG] Name:`/`Alias:` events
/// supply the friendly name; dash-MAC stand-ins are ignored so an
/// unnamed peer falls back to a compact MAC tail. First-seen order is
/// preserved; the caller sorts by signal.
fn parse_scan(out: &str) -> Vec<Device> {
    use std::collections::HashMap;
    let mut order: Vec<String> = Vec::new();
    let mut names: HashMap<String, String> = HashMap::new();
    let mut aliases: HashMap<String, String> = HashMap::new();
    let mut rssis: HashMap<String, i32> = HashMap::new();

    for raw in out.lines() {
        let line = strip_ansi(raw);
        let Some(pos) = line.find("Device ") else {
            continue;
        };
        // The event tag tells us whether the trailing token is the
        // advertised name (`[NEW] Device <MAC> <name>`) or an attribute
        // update (`[CHG] Device <MAC> <Attribute>: <value>`). Only NEW
        // carries a bare name — every CHG line is `Key: value`, so the
        // bare-name branch must be gated on NEW or it picks up junk like
        // `ManufacturerData.Key:` / `ServiceData.<uuid>:` / `UUIDs:`.
        let is_new = line[..pos].contains("[NEW]");
        let Some((mac, tail)) = line[pos + "Device ".len()..].split_once(' ') else {
            continue;
        };
        if !is_mac_with(mac, ':') {
            continue;
        }
        let (mac, tail) = (mac.to_string(), tail.trim());
        if !order.contains(&mac) {
            order.push(mac.clone());
        }
        if tail.starts_with("RSSI:") {
            if let Some(r) = parse_rssi(tail) {
                rssis.insert(mac, r);
            }
        } else if let Some(v) = tail.strip_prefix("Alias:").map(str::trim) {
            if !v.is_empty() && !is_mac_like(v) {
                aliases.insert(mac, v.to_string());
            }
        } else if let Some(v) = tail.strip_prefix("Name:").map(str::trim) {
            if !v.is_empty() && !is_mac_like(v) {
                names.insert(mac, v.to_string());
            }
        } else if is_new && !tail.is_empty() && !is_mac_like(tail) {
            names.entry(mac).or_insert_with(|| tail.to_string());
        }
    }

    order
        .into_iter()
        .map(|mac| {
            let name = aliases
                .get(&mac)
                .or_else(|| names.get(&mac))
                .cloned()
                .unwrap_or_else(|| mac_tail_name(&mac));
            let rssi = rssis.get(&mac).copied();
            Device { mac, name, rssi }
        })
        .collect()
}

/// MAC pattern `XX:XX:XX:XX:XX:XX` (case-insensitive hex octets).
/// Used by [`display_name`] to detect bluetoothctl's "no name yet"
/// fallback. bluez surfaces unnamed peers two ways: as a colon MAC in
/// `info`, and as a *dash*-separated alias (`50-32-37-A2-A0-D1`) in
/// `devices`/scan output — both must be treated as "no real name".
/// Pure-fn so the unit tests don't need bluez.
fn is_mac_like(s: &str) -> bool {
    is_mac_with(s, ':') || is_mac_with(s, '-')
}

fn is_mac_with(s: &str, sep: char) -> bool {
    let parts: Vec<&str> = s.split(sep).collect();
    parts.len() == 6
        && parts
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
        let connected = resolved_sorted(&devices("Connected"));
        let paired = resolved_sorted(&devices("Paired"));

        if !connected.is_empty() {
            items.push(("── connected ──".to_string(), Action::Noop));
            for d in &connected {
                items.push((
                    format!("● {} {}", signal_glyph(d.rssi), d.name),
                    Action::Disconnect(d.mac.clone()),
                ));
            }
        }

        let other: Vec<&Device> = paired
            .iter()
            .filter(|d| !connected.iter().any(|c| c.mac == d.mac))
            .collect();
        if !other.is_empty() {
            items.push((
                "── paired (signal: strong → weak) ──".to_string(),
                Action::Noop,
            ));
            for d in other {
                items.push((
                    format!("○ {} {}", signal_glyph(d.rssi), d.name),
                    Action::Connect(d.mac.clone()),
                ));
            }
        }

        items.push(("──".to_string(), Action::Noop));
        items.push(("scan & pair new…".to_string(), Action::Scan));
        items.push(("──".to_string(), Action::Noop));
        for d in &paired {
            items.push((format!("forget: {}", d.name), Action::Forget(d.mac.clone())));
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
    // bluez only emits RSSI as live `[CHG]` events once a device has
    // been advertising for a few seconds, so dwell long enough to
    // collect them — an 8s scan reliably surfaces names but no signal.
    notify("Scanning (12s)…");
    let scan_text = Command::new("bluetoothctl")
        .args(["--timeout", "12", "scan", "on"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();

    let paired = devices("Paired");
    let mut new_only: Vec<Device> = parse_scan(&scan_text)
        .into_iter()
        .filter(|d| !paired.iter().any(|(pm, _)| pm == &d.mac))
        .collect();
    sort_by_signal(&mut new_only);

    if new_only.is_empty() {
        notify("No new devices found");
        return Ok(());
    }

    let lines = new_only
        .iter()
        .map(|d| format!("{} {}  [{}]", signal_glyph(d.rssi), d.name, d.mac))
        .collect::<Vec<_>>()
        .join("\n");
    let picked = wifi::run_fuzzel(&lines, "pair » ", false)?;
    let picked = picked.trim();
    if picked.is_empty() {
        return Ok(());
    }

    let mac = new_only
        .iter()
        .find(|d| picked.contains(d.mac.as_str()))
        .map(|d| d.mac.clone());
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

    #[test]
    fn parse_rssi_handles_plain_and_hex_forms() {
        assert_eq!(parse_rssi("RSSI: -53"), Some(-53));
        assert_eq!(parse_rssi("        RSSI: -72"), Some(-72));
        // legacy hex form with decimal in parens
        assert_eq!(parse_rssi("RSSI: 0xffffffcb (-53)"), Some(-53));
        // bare hex (two's-complement i32)
        assert_eq!(parse_rssi("RSSI: 0xffffffcb"), Some(-53));
        assert_eq!(parse_rssi("Connected: yes"), None);
        assert_eq!(parse_rssi("RSSI:"), None);
    }

    #[test]
    fn parse_info_prefers_alias_over_name_and_reads_rssi() {
        let body = "Device AA:BB:CC:DD:EE:FF (public)\n\
                    \tName: WH-1000XM5\n\
                    \tAlias: Sony Headphones\n\
                    \tConnected: no\n\
                    \tRSSI: -61\n";
        assert_eq!(
            parse_info(body),
            (Some("Sony Headphones".to_string()), Some(-61))
        );
    }

    #[test]
    fn parse_info_ignores_mac_like_name_and_missing_rssi() {
        let body = "Device AA:BB:CC:DD:EE:FF (public)\n\
                    \tName: AA:BB:CC:DD:EE:FF\n\
                    \tAlias: AA:BB:CC:DD:EE:FF\n";
        assert_eq!(parse_info(body), (None, None));
    }

    #[test]
    fn mac_tail_name_uses_last_two_octets() {
        assert_eq!(mac_tail_name("AA:BB:CC:DD:EE:FF"), "device EE:FF");
        assert_eq!(mac_tail_name(""), "device");
    }

    #[test]
    fn sort_by_signal_orders_strongest_first_unknown_last() {
        let dev = |mac: &str, name: &str, rssi: Option<i32>| Device {
            mac: mac.to_string(),
            name: name.to_string(),
            rssi,
        };
        let mut v = vec![
            dev("00:00:00:00:00:01", "far", Some(-85)),
            dev("00:00:00:00:00:02", "unknown", None),
            dev("00:00:00:00:00:03", "close", Some(-42)),
            dev("00:00:00:00:00:04", "mid", Some(-65)),
        ];
        sort_by_signal(&mut v);
        let order: Vec<&str> = v.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(order, ["close", "mid", "far", "unknown"]);
    }

    #[test]
    fn sort_by_signal_breaks_ties_by_name() {
        let dev = |name: &str| Device {
            mac: "x".to_string(),
            name: name.to_string(),
            rssi: None,
        };
        let mut v = vec![dev("Zeta"), dev("Alpha"), dev("Mike")];
        sort_by_signal(&mut v);
        let order: Vec<&str> = v.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(order, ["Alpha", "Mike", "Zeta"]);
    }

    #[test]
    fn signal_glyph_buckets_by_strength() {
        assert_eq!(signal_glyph(Some(-30)), "▰▰▰▰");
        assert_eq!(signal_glyph(Some(-60)), "▰▰▰▱");
        assert_eq!(signal_glyph(Some(-70)), "▰▰▱▱");
        assert_eq!(signal_glyph(Some(-90)), "▰▱▱▱");
        assert_eq!(signal_glyph(None), "▱▱▱▱");
    }

    #[test]
    fn is_mac_like_recognises_dash_separated_form() {
        // bluez's stand-in alias for an unnamed peer.
        assert!(is_mac_like("50-32-37-A2-A0-D1"));
        assert!(is_mac_like("5b-a7-b3-d3-e9-6a"));
        // not MACs even though dashed
        assert!(!is_mac_like("Pixel-Buds"));
        assert!(!is_mac_like("WH-1000XM5"));
    }

    #[test]
    fn strip_ansi_removes_csi_sequences() {
        assert_eq!(
            strip_ansi("[\u{1b}[0;92mNEW\u{1b}[0m] Device AA:BB:CC:DD:EE:FF Phone"),
            "[NEW] Device AA:BB:CC:DD:EE:FF Phone"
        );
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn parse_scan_reads_names_rssi_and_sorts_by_signal() {
        // Realistic capture: ANSI-coloured tags, NEW name events, CHG
        // RSSI events in hex(paren) form, and a dash-MAC stand-in.
        let out = "\
[\u{1b}[0;92mNEW\u{1b}[0m] Device 3C:0F:02:EB:77:4D Meshtastic_774c\n\
[\u{1b}[0;92mNEW\u{1b}[0m] Device 50:32:37:A2:A0:D1 50-32-37-A2-A0-D1\n\
[\u{1b}[0;92mNEW\u{1b}[0m] Device 70:C9:12:89:1F:CA MiTV-MOOQ1\n\
[\u{1b}[0;93mCHG\u{1b}[0m] Device 3C:0F:02:EB:77:4D RSSI: 0xffffffac (-84)\n\
[\u{1b}[0;93mCHG\u{1b}[0m] Device 70:C9:12:89:1F:CA RSSI: 0xffffffbe (-66)\n\
[\u{1b}[0;93mCHG\u{1b}[0m] Device 50:32:37:A2:A0:D1 RSSI: -92\n";

        let mut devs = parse_scan(out);
        // named device keeps its name; dash-MAC stand-in is dropped
        // for a compact MAC tail.
        let by_mac = |m: &str| {
            devs.iter()
                .find(|d| d.mac == m)
                .cloned()
                .unwrap_or_else(|| panic!("missing {m}"))
        };
        assert_eq!(by_mac("3C:0F:02:EB:77:4D").name, "Meshtastic_774c");
        assert_eq!(by_mac("3C:0F:02:EB:77:4D").rssi, Some(-84));
        assert_eq!(by_mac("70:C9:12:89:1F:CA").rssi, Some(-66));
        assert_eq!(by_mac("50:32:37:A2:A0:D1").name, "device A0:D1");
        assert_eq!(by_mac("50:32:37:A2:A0:D1").rssi, Some(-92));

        sort_by_signal(&mut devs);
        let order: Vec<&str> = devs.iter().map(|d| d.mac.as_str()).collect();
        // -66 (MiTV) strongest, then -84, then -92.
        assert_eq!(
            order,
            [
                "70:C9:12:89:1F:CA",
                "3C:0F:02:EB:77:4D",
                "50:32:37:A2:A0:D1"
            ]
        );
    }

    #[test]
    fn parse_scan_prefers_alias_over_new_name() {
        let out = "\
[NEW] Device AA:BB:CC:DD:EE:FF SomeDevice\n\
[CHG] Device AA:BB:CC:DD:EE:FF Alias: My Headphones\n";
        let devs = parse_scan(out);
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].name, "My Headphones");
    }

    #[test]
    fn parse_scan_ignores_chg_attribute_events_as_names() {
        // A device that only ever appears via CHG attribute events
        // (no NEW name) must fall back to a MAC tail — never adopt
        // `ManufacturerData.Key:` / `ServiceData.<uuid>:` as its name.
        let out = "\
[CHG] Device 50:32:37:A2:A0:D1 ManufacturerData.Key: 0x0006 (6)\n\
[CHG] Device 50:32:37:A2:A0:D1 ServiceData.0000fef3-0000-1000-8000-00805f9b34fb:\n\
[CHG] Device 50:32:37:A2:A0:D1 UUIDs: 0000fef3-0000-1000-8000-00805f9b34fb\n\
[CHG] Device 50:32:37:A2:A0:D1 RSSI: -71\n";
        let devs = parse_scan(out);
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].name, "device A0:D1");
        assert_eq!(devs[0].rssi, Some(-71));
    }
}
