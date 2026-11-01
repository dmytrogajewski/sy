use std::process::Command;

#[test]
fn auth_help_works_without_desktop() {
    let output = Command::new(env!("CARGO_BIN_EXE_sy-system-auth"))
        .args(["syauth", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("install-pam"));
}

#[test]
fn auth_accepts_launcher_global_paths() {
    let output = Command::new(env!("CARGO_BIN_EXE_sy-system-auth"))
        .args([
            "--root",
            "/usr/share/sy",
            "--target",
            "/tmp/sy",
            "syauth",
            "--help",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn unanswered_phone_approval_keeps_the_applet_ready() {
    use std::{fs, os::unix::fs::PermissionsExt, time::SystemTime};

    let root = tempfile::tempdir().unwrap();
    let upstream = root.path().join("syauth");
    fs::write(
        &upstream,
        "#!/bin/sh\nprintf '%s\\n' '{\"daemon\":{\"state\":\"up\",\"peers\":[{\"peer_id\":\"phone\",\"in_flight_challenges\":0}]},\"bluez_adapter\":\"unknown\"}'\n",
    )
    .unwrap();
    fs::set_permissions(&upstream, fs::Permissions::from_mode(0o700)).unwrap();
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let audit = root.path().join("audit.log");
    fs::write(
        &audit,
        format!("phone,nonce,{now_ms},{now_ms},response-timeout,response-timeout\n"),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sy-system-auth"))
        .args(["syauth", "--waybar"])
        .env("SY_SYAUTH_BIN", &upstream)
        .env("SY_SYAUTH_AUDIT_LOG", &audit)
        .env("XDG_RUNTIME_DIR", root.path())
        .env("XDG_STATE_HOME", root.path())
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
        .output()
        .unwrap();
    assert!(output.status.success());
    let pill: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(pill["class"], "ok");
    let tooltip = pill["tooltip"].as_str().unwrap();
    assert!(tooltip.contains("daemon ready"));
    assert!(tooltip.contains("approval timed out (phone may be away)"));
    assert!(!tooltip.contains("reconnecting"));
    assert!(!tooltip.contains("phone reachable"));
}
