use std::process::Command;

use anyhow::{anyhow, Result};

// Plain-Unicode brightness ramp — chosen over nerd-font MD codepoints
// because the JetBrainsMono Nerd Font renders some brightness-* slots as
// unrelated glyphs (notably U+F00E2 looked like a cleaning broom).
const BR_LOW: &str = "\u{263C}"; // ☼ white sun with rays
const BR_MID: &str = "\u{25D1}"; // ◑ circle with right half black
const BR_HIGH: &str = "\u{2600}"; // ☀ black sun with rays

pub fn run(action: Option<&str>, waybar: bool) -> Result<()> {
    if waybar {
        return waybar_out();
    }
    let Some(a) = action else {
        return Err(anyhow!(
            "missing action (expected up|down) — pass --waybar for bar JSON"
        ));
    };
    match a {
        "up" => step("5%+"),
        "down" => step("5%-"),
        _ => Err(anyhow!("unknown bright action: {a} (expected up|down)")),
    }
}

fn step(delta: &str) -> Result<()> {
    Command::new("brightnessctl").args(["set", delta]).status()?;
    notify()?;
    refresh_waybar();
    Ok(())
}

fn read_pct() -> i32 {
    let Ok(out) = Command::new("brightnessctl").arg("-m").output() else {
        return 0;
    };
    let s = String::from_utf8_lossy(&out.stdout);
    // format: "intel_backlight,backlight,NNN,NN%,MMM"
    s.trim()
        .split(',')
        .nth(3)
        .and_then(|t| t.trim_end_matches('%').parse().ok())
        .unwrap_or(0)
}

fn waybar_out() -> Result<()> {
    let pct = read_pct();
    let (glyph, class) = if pct <= 33 {
        (BR_LOW, "low")
    } else if pct <= 66 {
        (BR_MID, "mid")
    } else {
        (BR_HIGH, "high")
    };
    let tooltip = format!("brightness: {pct}%");
    println!(r#"{{"text":"{glyph}","class":"{class}","tooltip":"{tooltip}","alt":"{pct}"}}"#);
    Ok(())
}

fn notify() -> Result<()> {
    let pct = read_pct();
    let _ = Command::new("notify-send")
        .args([
            "-a",
            "sy",
            "-t",
            "800",
            "-h",
            "string:x-canonical-private-synchronous:sy-brightness",
            "-h",
            &format!("int:value:{}", pct),
            &format!("Brightness: {pct}%"),
        ])
        .status();
    Ok(())
}

fn refresh_waybar() {
    let _ = Command::new("sh")
        .arg("-c")
        .arg("pkill -RTMIN+12 waybar 2>/dev/null")
        .status();
}
