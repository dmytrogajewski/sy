use sy_aiplane::aiplane::config;
use sy_core::workload::WorkloadKind;

#[test]
fn runtime_paths_follow_xdg_layout_and_explicit_ai_override() {
    // This integration-test binary has one test; the lock keeps future tests in it serialized.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = ENV_LOCK.lock().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let override_socket = runtime.path().join("alternate.sock");
    let old_runtime = std::env::var_os("XDG_RUNTIME_DIR");
    let old_state = std::env::var_os("XDG_STATE_HOME");
    let old_socket = std::env::var_os("SY_AIPLANE_SOCKET");

    // SAFETY: this test serializes its environment override with the existing test lock and restores it before release.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", runtime.path()) };
    // SAFETY: this test serializes its environment override with the existing test lock and restores it before release.
    unsafe { std::env::set_var("XDG_STATE_HOME", state.path()) };
    // SAFETY: this test serializes its environment override with the existing test lock and restores it before release.
    unsafe { std::env::remove_var("SY_AIPLANE_SOCKET") };

    assert_eq!(
        config::knowledge_socket_path(),
        runtime.path().join("sy-knowledge.sock")
    );
    assert_eq!(
        config::aiplane_socket_path(),
        runtime.path().join("sy-aiplane.sock")
    );
    assert_eq!(
        config::worker_socket_path(WorkloadKind::Embed),
        runtime.path().join("sy-aiplane-worker-embed.sock")
    );
    assert_eq!(
        config::state_root().unwrap(),
        state.path().join("sy/aiplane")
    );
    if !config::dedicated_installed() {
        assert_eq!(
            config::inference_socket_path(),
            config::knowledge_socket_path()
        );
    }

    // SAFETY: this test serializes its environment override with the existing test lock and restores it before release.
    unsafe { std::env::set_var("SY_AIPLANE_SOCKET", &override_socket) };
    assert_eq!(config::inference_socket_path(), override_socket);
    assert!(config::dedicated_selected());

    restore("XDG_RUNTIME_DIR", old_runtime);
    restore("XDG_STATE_HOME", old_state);
    restore("SY_AIPLANE_SOCKET", old_socket);
}

fn restore(key: &str, value: Option<std::ffi::OsString>) {
    match value {
        // SAFETY: the sole caller holds ENV_LOCK until this restore completes.
        Some(value) => unsafe { std::env::set_var(key, value) },
        // SAFETY: the sole caller holds ENV_LOCK until this restore completes.
        None => unsafe { std::env::remove_var(key) },
    }
}
