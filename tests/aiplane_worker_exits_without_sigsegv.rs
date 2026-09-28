//! Worker shutdown must be a clean exit, not a segfault.
//!
//! ONNX Runtime's VitisAI EP registers a shared-object finalizer that
//! touches state ORT has already destroyed, so every `sy aiplane worker`
//! stop used to die with SIGSEGV inside `libonnxruntime_providers_vitisai.so`
//! under `_dl_call_fini` — one ~330 MB core pair per plane restart, and a
//! `status=11/SEGV` line in journald (specs/bugs/BUG-20260927-0400.md).
//!
//! The faulting finalizer only exists once the EP is loaded, which needs
//! `/dev/accel` — so these tests lock the *shutdown contract* hermetically
//! (a sandboxed `HOME` makes `load()` fail before any EP is dlopen'd) and
//! the NPU-backed regression lives behind `test-npu`.

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn worker_cmd(home: &PathBuf, socket: &PathBuf, kind: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_sy"));
    c.args(["aiplane", "worker", "--kind", kind, "--socket"])
        .arg(socket)
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_RUNTIME_DIR", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    c
}

/// Each test needs its own sandbox: sharing one `HOME` between parallel
/// tests lets whichever finishes first `remove_dir_all` the other's socket.
fn spawn_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sy-worker-exit-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("tmp home");
    dir
}

/// Poll until the worker has bound its socket (or we give up).
fn wait_for_socket(socket: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if socket.exists() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

fn send_term(pid: u32) {
    let rc = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("kill available");
    assert!(rc.success(), "SIGTERM to worker pid {pid} failed");
}

#[test]
fn sigterm_shuts_the_worker_down_cleanly_and_reclaims_its_socket() {
    let home = spawn_home("term");
    let socket = home.join("sy-aiplane-worker-embed.sock");
    let mut child = worker_cmd(&home, &socket, "embed")
        .spawn()
        .expect("worker spawns");

    assert!(
        wait_for_socket(&socket, Duration::from_secs(20)),
        "worker never bound its socket"
    );
    send_term(child.id());

    // The wait status is the whole point: a shutdown that dies on signal 11
    // reports `success() == false` with `code() == None`.
    let status = child
        .wait()
        .expect("worker exits after SIGTERM (no zombie left behind)");
    assert!(
        status.code().is_some(),
        "worker shutdown was killed by a signal ({status:?}) — library finalizers are running again"
    );
    assert!(
        status.success(),
        "clean shutdown must exit 0, got {status:?}"
    );
    assert!(
        !socket.exists(),
        "worker must remove its own socket before it goes"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn unsupported_workload_kind_exits_rather_than_hanging() {
    let home = spawn_home("tts");
    let socket = home.join("sy-aiplane-worker-tts.sock");
    let mut child = worker_cmd(&home, &socket, "tts")
        .spawn()
        .expect("worker spawns");
    let status = child.wait().expect("worker exits");
    assert!(
        status.code().is_some(),
        "unsupported kind must exit with a code, not a signal ({status:?})"
    );
    assert!(
        !status.success(),
        "unsupported kind must not report success"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// The actual defect, on the hardware that has it: with a VitisAI session
/// loaded, teardown faults unless finalizers are skipped. Excluded from the
/// default run because it needs `/dev/accel` to itself.
#[cfg(feature = "test-npu")]
#[test]
fn npu_worker_shutdown_does_not_segfault() {
    let home = spawn_home("npu");
    let socket = home.join("sy-aiplane-worker-embed.sock");
    let mut child = worker_cmd(&home, &socket, "embed")
        .spawn()
        .expect("worker spawns");
    assert!(
        wait_for_socket(&socket, Duration::from_secs(30)),
        "worker never bound its socket"
    );
    // Let the model finish loading so the EP is actually resident.
    thread::sleep(Duration::from_secs(45));
    send_term(child.id());
    let status = child.wait().expect("worker exits");
    assert!(
        status.code().is_some(),
        "VitisAI EP finalizer faulted again ({status:?}); see BUG-20260927-0400"
    );
    let _ = std::fs::remove_dir_all(&home);
}
