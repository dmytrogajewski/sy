use std::process::Command;
use std::time::Duration;

fn monitor() -> Command {
    Command::new(env!("CARGO_BIN_EXE_sy-system-monitor"))
}

#[test]
fn standalone_monitor_exposes_the_existing_subcommands() {
    let output = monitor()
        .arg("mon")
        .arg("--help")
        .output()
        .expect("launch monitor");
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).expect("utf8 help");
    for command in ["collect", "snapshot", "mcp", "doctor", "waybar"] {
        assert!(help.contains(command), "missing {command} in {help}");
    }
}

#[test]
fn standalone_monitor_accepts_launcher_global_options() {
    let output = monitor()
        .args([
            "--root",
            "/usr/share/sy",
            "--target",
            "/tmp",
            "mon",
            "--help",
        ])
        .output()
        .expect("launch monitor with global options");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn missing_collector_preserves_exit_code_three() {
    let runtime = tempfile::tempdir().expect("runtime dir");
    let output = monitor()
        .args(["mon", "snapshot", "--json"])
        .env("XDG_RUNTIME_DIR", runtime.path())
        .output()
        .expect("launch monitor");
    assert_eq!(output.status.code(), Some(3));
}

#[test]
fn standalone_doctor_reports_monitor_checks() {
    let runtime = tempfile::tempdir().expect("runtime dir");
    let output = monitor()
        .args(["mon", "doctor", "--json"])
        .env("XDG_RUNTIME_DIR", runtime.path())
        .output()
        .expect("launch monitor");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("doctor json");
    assert_eq!(report["checks"][0]["name"], "mon.collect.running");
}

#[test]
fn standalone_collector_serves_a_snapshot_over_its_socket() {
    let runtime = tempfile::tempdir().expect("runtime dir");
    let mut child = monitor()
        .args(["mon", "collect", "--tick-ms", "50", "--history-size", "4"])
        .env("XDG_RUNTIME_DIR", runtime.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start collector");
    let socket = runtime.path().join("sy/mon.sock");
    let mut ready = false;
    for _ in 0..40 {
        if socket.exists() {
            ready = true;
            break;
        }
        if let Some(status) = child.try_wait().expect("collector status") {
            panic!("collector exited before binding: {status}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = if ready {
        monitor()
            .args(["mon", "snapshot", "--json"])
            .env("XDG_RUNTIME_DIR", runtime.path())
            .output()
            .expect("snapshot")
    } else {
        child.kill().expect("stop collector");
        child.wait().expect("reap collector");
        panic!("collector did not bind {}", socket.display());
    };
    child.kill().expect("stop collector");
    child.wait().expect("reap collector");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let snapshot: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("snapshot json");
    assert!(snapshot["schema_version"].is_number());
}
