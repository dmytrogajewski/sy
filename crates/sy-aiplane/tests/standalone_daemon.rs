use std::{
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn daemon_owns_a_dedicated_socket_and_cleans_it_on_sigterm() {
    let runtime = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let process = Command::new(env!("CARGO_BIN_EXE_sy-system-aiplane"))
        .args(["aiplane", "daemon"])
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("XDG_STATE_HOME", state.path())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut daemon = Guard(process);
    let socket = runtime.path().join("sy-aiplane.sock");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        if let Some(status) = daemon.0.try_wait().unwrap() {
            panic!("daemon exited before binding: {status}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(socket.exists());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut client = sy_ipc::Client::connect(&socket).await.unwrap();
        let response = client
            .call(
                "system.describe",
                serde_json::json!({}),
                sy_ipc::CallOpts::default(),
            )
            .await
            .unwrap();
        let sy_ipc::Response::Ok { result, .. } = response else {
            panic!("describe failed")
        };
        assert_eq!(result["build_info"]["name"], "sy-aiplane");
    });
    let status = Command::new(env!("CARGO_BIN_EXE_sy-system-aiplane"))
        .args(["aiplane", "status", "--json"])
        .env("SY_AIPLANE_SOCKET", &socket)
        .env("XDG_RUNTIME_DIR", runtime.path())
        .output()
        .unwrap();
    assert!(status.status.success());
    let body: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(body["daemon_running"], true);
    assert_eq!(body["embed_backend"], "unloaded");
    let runtime_snapshot = state.path().join("sy/aiplane/runtime.json");
    let snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&runtime_snapshot).unwrap()).unwrap();
    assert_eq!(snapshot["daemon_running"], true);
    let duplicate = Command::new(env!("CARGO_BIN_EXE_sy-system-aiplane"))
        .args(["aiplane", "daemon"])
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("XDG_STATE_HOME", state.path())
        .output()
        .unwrap();
    assert!(!duplicate.status.success());
    assert!(socket.exists());
    // SAFETY: this test spawned `daemon.0` and its pid is still owned by
    // this Guard; SIGTERM is the service's documented graceful stop path.
    assert_eq!(
        unsafe { libc::kill(daemon.0.id() as i32, libc::SIGTERM) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while daemon.0.try_wait().unwrap().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!socket.exists());
    let snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&runtime_snapshot).unwrap()).unwrap();
    assert_eq!(snapshot["daemon_running"], false);
}

#[test]
fn missing_dedicated_daemon_never_runs_a_local_npu_session() {
    let runtime = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sy-system-aiplane"))
        .args([
            "aiplane",
            "run",
            "--workload",
            "embed",
            "--input",
            "{\"kind\":\"text\",\"text\":\"hello\"}",
            "--json",
        ])
        .env("SY_AIPLANE_SOCKET", runtime.path().join("absent.sock"))
        .env("XDG_RUNTIME_DIR", runtime.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("start sy-aiplane.service"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
