//! Keep Waybar under the user manager so `sy apply` can safely restart it.

use std::process::Command;

/// Enable the bar for future sessions and restart it in the current one.
pub fn reload() {
    let enable = Command::new("systemctl")
        .args(["--user", "enable", "sy-waybar.service"])
        .status();
    if !matches!(enable, Ok(status) if status.success()) {
        eprintln!("  ! could not enable sy-waybar.service");
        return;
    }

    let session = Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "graphical-session.target"])
        .status();
    if !matches!(session, Ok(status) if status.success()) {
        return;
    }

    let restart = Command::new("systemctl")
        .args(["--user", "restart", "sy-waybar.service"])
        .status();
    if matches!(restart, Ok(status) if status.success()) {
        println!("  ↻ waybar restarted");
    } else {
        eprintln!("  ! could not restart sy-waybar.service");
    }
}
