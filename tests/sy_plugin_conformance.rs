//! Plugin protocol conformance harness for the sy-file-manager runtime
//! (SPEC §4.6 "8-scenario contract").
//!
//! Spawns the in-tree [`sy-plugin-fake`][fake] binary under the real
//! [`crate::plugin::proc`] supervisor and exercises every method of
//! the SPEC §4.2 wire end-to-end. Each scenario is one `#[tokio::test]`
//! so a regression on any single contract row fails one named test:
//!
//! 1. [`spawn_then_ready_within_250ms`] — spawn → handshake → Ready ≤ 250 ms.
//! 2. [`preview_roundtrip_under_100ms_warm`] — second `preview` ≤ 100 ms.
//! 3. [`crash_then_restart_with_backoff`] — SIGKILL → respawn within budget.
//! 4. [`cap_violation_returns_32099`] — plugin calls `host.fs.read` it
//!    didn't declare → host returns `-32099 CAP_NOT_GRANTED`.
//! 5. [`rlimit_breach_returns_32097`] — fake allocates >`memory_mb`,
//!    surfaces `-32097 LIMIT_EXCEEDED` over the wire.
//! 6. [`signature_mismatch_refuses_spawn`] — `plugin::install::install`
//!    with a corrupted minisign signature returns
//!    [`InstallError::SignatureInvalid`] (CLI exit 7).
//! 7. [`shutdown_then_exit_within_timeout`] — `proc.shutdown()` returns
//!    within `shutdown_timeout_ms`, child exits 0.
//! 8. [`ping_then_pong_roundtrip`] — fake echoes the ping ts in ≤ 50 ms.
//!
//! [fake]: ../crates/sy-plugin-fake/src/main.rs

// Re-import the plugin modules the same way `sy_file_journey_e2e.rs`
// does — the `sy` package has no `lib.rs` so each integration test
// pulls the production sources in via `#[path]`. Keeps the conformance
// test driving the EXACT same supervisor / capability / install code
// the bin runs in production.
#[path = "../src/plugin/capability.rs"]
mod capability;
#[path = "../src/plugin/host_fns.rs"]
mod host_fns;
#[path = "../src/plugin/install.rs"]
mod install;
#[path = "../src/plugin/manifest.rs"]
mod manifest;
#[path = "../src/plugin/proc.rs"]
mod proc_mod;
#[path = "../src/plugin/registry.rs"]
mod registry;
#[path = "../src/plugin/rpc.rs"]
mod rpc;
#[path = "../src/plugin/sandbox.rs"]
mod sandbox;
#[path = "../src/plugin/transport.rs"]
mod transport;

/// Side-shim re-exports so the `#[path]`-imported source files'
/// `use crate::plugin::…` lines resolve under this test binary.
pub(crate) mod plugin {
    pub(crate) use super::capability;
    pub(crate) use super::host_fns;
    pub(crate) use super::install;
    pub(crate) use super::manifest;
    pub(crate) use super::proc_mod as proc;
    pub(crate) use super::rpc;
    pub(crate) use super::sandbox;
    pub(crate) use super::transport;
}

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Hard upper bound on `spawn → Ready`. SPEC §2.2 calls for p99 < 250 ms;
/// on the same machine the scenario also tolerates a 2× CI slack via
/// the `SY_CONFORMANCE_PERF_X2` env var documented in the Step 10
/// brief — flip on a busy runner with `cargo test --features ci_slow`
/// reading the same var. Default is the strict SPEC budget.
const SPAWN_READY_BUDGET_MS: u64 = 250;

/// Hard upper bound on a warm `preview` round-trip. SPEC §2.2 p99 < 100 ms.
const PREVIEW_WARM_BUDGET_MS: u64 = 100;

/// Hard upper bound on a `ping`-`pong` round-trip. The SPEC doesn't
/// pin a number; the brief's scenario 8 calls for ≤ 50 ms.
const PING_BUDGET_MS: u64 = 50;

/// Hard upper bound on graceful shutdown. Matches the manifest's
/// `shutdown_timeout_ms = 1000` with a 2× slack so the test never
/// races the production budget.
const SHUTDOWN_BUDGET_MS: u64 = 2_000;

/// Resolve the per-test perf-budget multiplier. Default `1×` honours
/// the SPEC §2.2 targets verbatim; setting `SY_CONFORMANCE_PERF_X2=1`
/// relaxes every budget by 2× for slow CI runners. Documented inline
/// so a future runner config can flip the env without grepping.
fn perf_multiplier() -> u64 {
    if std::env::var_os("SY_CONFORMANCE_PERF_X2").is_some() {
        2
    } else {
        1
    }
}

/// Locate the just-built `sy-plugin-fake` binary by walking up from
/// the conformance test binary's path. `CARGO_BIN_EXE_<name>` is
/// per-package (only the package containing the bin gets it), so for
/// a cross-crate bin we walk to the workspace target dir and pick up
/// `target/<profile>/sy-plugin-fake`. If the binary is missing
/// (e.g. someone ran `cargo test --test sy_plugin_conformance`
/// without `--workspace`), shell out to `cargo build -p sy-plugin-fake`
/// as a one-shot fallback so the test runs hermetically either way.
fn locate_fake_binary() -> PathBuf {
    let mut current = std::env::current_exe().expect("current_exe");
    // current_exe is `…/target/<profile>/deps/sy_plugin_conformance-<hash>`.
    // Walk up to `…/target/<profile>/`.
    current.pop(); // sy_plugin_conformance-<hash>
    current.pop(); // deps
                   // Now we're at `<profile>/`. The fake bin sits next to us.
    let candidate = current.join("sy-plugin-fake");
    if !candidate.is_file() {
        // Fallback: build the missing binary directly. `make test`
        // (cargo test --workspace --all-targets) builds it for us;
        // single-test invocations may not, so we self-heal here.
        // Honour the test binary's profile so `cargo test --release`
        // also finds the right artefact.
        let profile_dir = current
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("debug");
        let mut args = vec!["build", "-p", "sy-plugin-fake"];
        if profile_dir == "release" {
            args.push("--release");
        }
        let status = std::process::Command::new("cargo")
            .args(&args)
            .status()
            .expect("spawn cargo build -p sy-plugin-fake");
        assert!(
            status.success(),
            "cargo build -p sy-plugin-fake failed (status={status:?})"
        );
    }
    assert!(
        candidate.is_file(),
        "sy-plugin-fake binary still missing at {} after fallback build",
        candidate.display()
    );
    candidate
}

/// Build a manifest that points at the just-built `sy-plugin-fake`
/// binary and lets the caller tweak `[needs]` + `[limits]` per
/// scenario. The `needs_*` flags steer the cap-violation /
/// rlimit-breach scenarios; the limits override drives the
/// rlimit-breach budget.
fn fake_manifest(needs_fs_read: bool, memory_mb: u32) -> manifest::Manifest {
    let exec_path = locate_fake_binary();
    let exec = exec_path.to_string_lossy();
    let fs_read = if needs_fs_read {
        r#"fs_read = ["/etc/**"]"#
    } else {
        r#"fs_read = []"#
    };
    let src = format!(
        r#"
api = "1"

[plugin]
id = "sy-plugin-fake"
name = "sy Plugin Fake"
version = "0.1.0"
api_min = "1"
api_max = "1"

[plugin.binary]
exec = "{exec}"

[[capability]]
kind = "previewer"
mime = "text/markdown"

[needs]
{fs_read}
fs_write = []
preview = []
knowledge = []
network = []
exec = []

[limits]
memory_mb = {memory_mb}
cpu_seconds = 10
nofile = 64
spawn_timeout_ms = 1500
shutdown_timeout_ms = 1000

[env]
PATH = "/usr/bin:/bin"
"#
    );
    manifest::load(&src).expect("fake manifest parses + validates")
}

/// Build a `SpawnOpts` block with the supervisor wired to a HostCtx
/// so plugin-initiated `host.*` requests route through the real
/// dispatch path (the cap_violation scenario relies on this).
fn opts_with_host(workdir: &Path) -> (proc_mod::SpawnOpts, host_fns::HostCtx) {
    let (ctx, _rx) = host_fns::ctx_for(workdir.to_path_buf(), serde_json::Value::Null);
    let mut o = proc_mod::SpawnOpts::new(workdir.to_path_buf());
    o.ping_interval = Duration::from_millis(80);
    o.ping_timeout = Duration::from_millis(800);
    o.request_timeout = Duration::from_secs(2);
    o.host_ctx = Some(ctx.clone());
    (o, ctx)
}

/// SPEC §4.6 scenario 1 — spawn → Ready inside the §2.2 budget.
/// Drives the canonical happy path: spawn the fake under the real
/// supervisor, time the wall-clock from `spawn()` entry to the
/// `State::Ready` return, assert it stays under `SPAWN_READY_BUDGET_MS`
/// (× the perf multiplier).
#[tokio::test(flavor = "current_thread")]
async fn spawn_then_ready_within_250ms() {
    let tmp = tempfile::tempdir().expect("tmp");
    let m = fake_manifest(false, 64);
    let (opts, _ctx) = opts_with_host(tmp.path());
    let start = std::time::Instant::now();
    let mut proc = proc_mod::spawn(m, opts).await.expect("spawn");
    let elapsed = start.elapsed();
    assert_eq!(proc.health(), proc_mod::State::Ready);
    let budget = Duration::from_millis(SPAWN_READY_BUDGET_MS * perf_multiplier());
    assert!(
        elapsed < budget,
        "spawn→Ready took {elapsed:?}, budget {budget:?}"
    );
    let _ = proc.shutdown().await;
}

/// SPEC §4.6 scenario 2 — second (warm) `preview` round-trips inside
/// the §2.2 < 100 ms budget. The first preview may be cold (process
/// just spawned); the second one runs the same code on a warm
/// process and is the one we time.
#[tokio::test(flavor = "current_thread")]
async fn preview_roundtrip_under_100ms_warm() {
    let tmp = tempfile::tempdir().expect("tmp");
    let m = fake_manifest(false, 64);
    let (opts, _ctx) = opts_with_host(tmp.path());
    let mut proc = proc_mod::spawn(m, opts).await.expect("spawn");
    // Cold preview — measured but not asserted on. Forces any
    // pagecache misses on the binary into the warm budget below.
    let _ = proc
        .request("preview", serde_json::json!({ "path": "x" }))
        .await
        .expect("first preview");
    let start = std::time::Instant::now();
    let v = proc
        .request("preview", serde_json::json!({ "path": "x" }))
        .await
        .expect("warm preview");
    let elapsed = start.elapsed();
    let budget = Duration::from_millis(PREVIEW_WARM_BUDGET_MS * perf_multiplier());
    assert!(
        elapsed < budget,
        "warm preview took {elapsed:?}, budget {budget:?}"
    );
    assert_eq!(v["image"]["w"], 1, "preview reply must carry a 1×1 PNG");
    let _ = proc.shutdown().await;
}

/// SPEC §4.6 scenario 3 — kill the running child mid-flight; the
/// supervisor walks the `2^n * 100 ms` restart ladder and the
/// re-handshake lands back on `State::Ready`. A second `preview`
/// against the restarted process succeeds.
#[tokio::test(flavor = "current_thread")]
async fn crash_then_restart_with_backoff() {
    let tmp = tempfile::tempdir().expect("tmp");
    let m = fake_manifest(false, 64);
    let (mut opts, _ctx) = opts_with_host(tmp.path());
    // Lengthen ping so the test only exercises the EOF→restart path.
    opts.ping_interval = Duration::from_secs(30);
    opts.ping_timeout = Duration::from_secs(30);
    opts.max_restart_attempts = 3;
    let mut proc = proc_mod::spawn(m, opts).await.expect("spawn");
    assert_eq!(proc.health(), proc_mod::State::Ready);
    // Find the running fake binary by /proc walk + SIGKILL it. The
    // supervisor's reader loop sees EOF and walks the backoff ladder.
    let pids = find_children_by_cmdline(b"sy-plugin-fake\0");
    assert!(!pids.is_empty(), "fake child must be alive before kill");
    for pid in &pids {
        // SAFETY: SIGKILL on a pid we just observed alive is a
        // single async-signal-safe syscall; ESRCH on a races-to-exit
        // pid still produces the EOF the supervisor needs.
        unsafe { libc::kill(*pid as libc::pid_t, libc::SIGKILL) };
    }
    let restart_start = std::time::Instant::now();
    proc.wait_state_change_then_ready()
        .await
        .expect("supervisor restarts to Ready");
    let restart_elapsed = restart_start.elapsed();
    // SPEC §4.4 backoff sum: 100 + 200 + 400 ms = 700 ms; with
    // spawn overhead a 3 s ceiling is the conservative upper bound.
    assert!(
        restart_elapsed < Duration::from_secs(3),
        "restart must complete inside backoff budget, took {restart_elapsed:?}"
    );
    // Issue a fresh preview against the restarted child.
    let v = proc
        .request("preview", serde_json::json!({ "path": "x" }))
        .await
        .expect("preview after restart");
    assert_eq!(v["image"]["w"], 1);
    let _ = proc.shutdown().await;
}

/// SPEC §4.6 scenario 4 — cap violation. The fake plugin's manifest
/// declares `fs_read = []`, so when the fake issues a `host.fs.read`
/// (driven by `SY_FAKE_TRIGGER=cap_violation`) the host's dispatcher
/// returns `-32099 CAP_NOT_GRANTED`. The fake folds that error into
/// its preview reply so we can read the code directly off the wire.
#[tokio::test(flavor = "current_thread")]
async fn cap_violation_returns_32099() {
    let tmp = tempfile::tempdir().expect("tmp");
    let m = fake_manifest(false, 64);
    let (mut opts, _ctx) = opts_with_host(tmp.path());
    // Inject the trigger via env. The fake reads
    // `SY_FAKE_TRIGGER=cap_violation` as a fallback when the request
    // params don't carry an explicit `trigger` — but we set the
    // param directly so the test is hermetic to the env.
    opts.request_timeout = Duration::from_secs(3);
    let mut proc = proc_mod::spawn(m, opts).await.expect("spawn");
    let v = proc
        .request(
            "preview",
            serde_json::json!({ "path": "x", "trigger": "cap_violation" }),
        )
        .await
        .expect("cap_violation preview");
    // The fake folds the received host error object into
    // `result.host_error`.
    let host_err = &v["host_error"];
    assert_eq!(
        host_err["code"],
        rpc::CAP_NOT_GRANTED,
        "host must return -32099 CAP_NOT_GRANTED, got: {v}"
    );
    assert_eq!(host_err["message"], "CAP_NOT_GRANTED");
    let _ = proc.shutdown().await;
}

/// SPEC §4.6 scenario 5 — rlimit breach. The fake attempts a 4 GiB
/// `try_reserve_exact` against a manifest with `memory_mb = 64`,
/// `RLIMIT_AS` denies the allocation, the fake catches
/// `TryReserveError` and emits a `-32097 LIMIT_EXCEEDED` JSON-RPC
/// error response. The supervisor surfaces it as
/// `RpcError::Peer { code: -32097, .. }`.
#[tokio::test(flavor = "current_thread")]
async fn rlimit_breach_returns_32097() {
    let tmp = tempfile::tempdir().expect("tmp");
    // Tight 64 MiB ceiling so the 4 GiB try_reserve must fail.
    let m = fake_manifest(false, 64);
    let (opts, _ctx) = opts_with_host(tmp.path());
    let mut proc = proc_mod::spawn(m, opts).await.expect("spawn");
    let err = proc
        .request(
            "preview",
            serde_json::json!({ "path": "x", "trigger": "rlimit_breach" }),
        )
        .await
        .expect_err("rlimit breach must fail the request");
    match err {
        proc_mod::RpcError::Peer { code, .. } => {
            assert_eq!(
                code,
                rpc::LIMIT_EXCEEDED,
                "rlimit breach must surface -32097 LIMIT_EXCEEDED, got code={code}"
            );
        }
        other => panic!("expected Peer(-32097), got {other:?}"),
    }
    let _ = proc.shutdown().await;
}

/// SPEC §4.6 scenario 6 — installing a fixture whose minisign
/// signature does not verify against the manifest's pubkey must
/// fail with [`install::InstallError::SignatureInvalid`] (CLI exit 7).
/// We don't drive the supervisor here — the install path refuses to
/// stage the plugin in the first place, so the bin's spawn never
/// happens.
#[test]
fn signature_mismatch_refuses_spawn() {
    let tmp = tempfile::tempdir().expect("tmp");
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).expect("mkdir src");
    let install_root = tmp.path().join("install");
    let publishers = tmp.path().join("pub");
    std::fs::create_dir_all(&publishers).expect("mkdir pub");

    // Plant a binary + manifest with a syntactically-valid but
    // wrong-key minisign signature. The publisher's keypair never
    // signed this payload, so verify must fail.
    std::fs::create_dir_all(src.join("bin")).expect("mkdir bin");
    std::fs::write(
        src.join("bin").join("badsig-plugin"),
        b"#!/bin/sh\necho hi\n",
    )
    .expect("write bin");
    // The signature block has a syntactically-correct shape (so the
    // parser accepts it), but the sig string is a freshly-minted
    // signature for the WRONG payload (the literal bytes "wrong").
    let (good_pk, sk) = fresh_keypair();
    let wrong_sig = sign_with_secret(&sk, b"wrong-payload");
    let manifest_body = format!(
        r#"api = "1"

[plugin]
id = "badsig-plugin"
name = "Bad Sig Plugin"
version = "0.1.0"
api_min = "1"
api_max = "1"

[plugin.binary]
exec = "./bin/badsig-plugin"

[plugin.signature]
sig = '''
{wrong_sig}'''
pubkey = "{good_pk}"

[[capability]]
kind = "previewer"
mime = "text/markdown"

[needs]
fs_read = []
fs_write = []
preview = []
knowledge = []
network = []
exec = []

[limits]
memory_mb = 64
cpu_seconds = 10
nofile = 64
spawn_timeout_ms = 1500
shutdown_timeout_ms = 1000
"#
    );
    std::fs::write(src.join("plugin.toml"), manifest_body).expect("write manifest");

    let mut opts = install::InstallOpts::new(install_root.clone());
    opts.publishers_dir = publishers;
    let err = install::install(install::InstallSource::Path(src), opts)
        .expect_err("signature mismatch must fail install");
    match err {
        install::InstallError::SignatureInvalid(_) => {}
        other => panic!("expected SignatureInvalid, got {other:?}"),
    }
    // Atomicity check: no `<install_root>/badsig-plugin/` left behind.
    let leaked = install_root.join("badsig-plugin");
    assert!(
        !leaked.exists(),
        "failed install must not leave a partial dir at {}",
        leaked.display()
    );
}

/// SPEC §4.6 scenario 7 — `shutdown` request → reply → `exit`
/// notification → child exit(0) within the manifest's
/// `shutdown_timeout_ms` (1000 ms). We time the whole sequence with
/// a 2× slack so a slow CI box doesn't flake.
#[tokio::test(flavor = "current_thread")]
async fn shutdown_then_exit_within_timeout() {
    let tmp = tempfile::tempdir().expect("tmp");
    let m = fake_manifest(false, 64);
    let (opts, _ctx) = opts_with_host(tmp.path());
    let mut proc = proc_mod::spawn(m, opts).await.expect("spawn");
    assert_eq!(proc.health(), proc_mod::State::Ready);
    let start = std::time::Instant::now();
    proc.shutdown().await.expect("graceful shutdown");
    let elapsed = start.elapsed();
    let budget = Duration::from_millis(SHUTDOWN_BUDGET_MS * perf_multiplier());
    assert!(
        elapsed < budget,
        "shutdown took {elapsed:?}, budget {budget:?}"
    );
}

/// SPEC §4.6 scenario 8 — `ping` request → `pong` reply round-trip in
/// under [`PING_BUDGET_MS`]. The fake echoes the `ts` field per the
/// SPEC §4.2.3 ping schema. We send the ping as a regular
/// `proc.request("ping", …)` because the supervisor's periodic
/// ping arm is internal; for a wire-level scenario the manual
/// request is the cleaner probe.
#[tokio::test(flavor = "current_thread")]
async fn ping_then_pong_roundtrip() {
    let tmp = tempfile::tempdir().expect("tmp");
    let m = fake_manifest(false, 64);
    let (opts, _ctx) = opts_with_host(tmp.path());
    let mut proc = proc_mod::spawn(m, opts).await.expect("spawn");
    let start = std::time::Instant::now();
    let v = proc
        .request("ping", serde_json::json!({ "ts": 42 }))
        .await
        .expect("ping rpc");
    let elapsed = start.elapsed();
    let budget = Duration::from_millis(PING_BUDGET_MS * perf_multiplier());
    assert!(
        elapsed < budget,
        "ping→pong took {elapsed:?}, budget {budget:?}"
    );
    assert_eq!(v["ts"], 42, "fake must echo the inbound ts");
    let _ = proc.shutdown().await;
}

// ── Helpers shared with `tests/sy_plugin_install.rs` (in spirit;
// copied verbatim here so the conformance binary is self-contained
// and doesn't grow a cross-test `mod common` import surface) ──

/// Walk `/proc/<pid>/cmdline` and return every pid whose argv
/// contains the given NUL-suffixed needle. Used by scenario 3 to
/// locate the running fake binary without exposing the supervisor's
/// internal `Child` handle.
fn find_children_by_cmdline(needle_with_nul: &[u8]) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ent in entries.flatten() {
        let name = ent.file_name();
        let Some(name_s) = name.to_str() else {
            continue;
        };
        let Ok(pid) = name_s.parse::<u32>() else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        if cmdline
            .windows(needle_with_nul.len())
            .any(|w| w == needle_with_nul)
        {
            out.push(pid);
        }
    }
    out
}

/// Generate a fresh minisign keypair and return `(pubkey_b64, secret_key)`.
/// Mirrors the helper in `tests/sy_plugin_install.rs` so scenario 6
/// can mint a signed fixture hermetically inside this test binary.
fn fresh_keypair() -> (String, minisign::SecretKey) {
    let kp = minisign::KeyPair::generate_unencrypted_keypair().expect("generate keypair");
    let pk_box = kp.pk.to_box().expect("pk to_box");
    let pk_str = pk_box.to_string();
    let pk_b64 = pk_str
        .lines()
        .find(|l| !l.starts_with("untrusted comment") && !l.is_empty())
        .expect("pubkey base64 line present")
        .to_string();
    (pk_b64, kp.sk)
}

/// Sign `payload` with `sk` and return the minisign signature text.
fn sign_with_secret(sk: &minisign::SecretKey, payload: &[u8]) -> String {
    let sig =
        minisign::sign(None, sk, std::io::Cursor::new(payload), None, None).expect("sign payload");
    sig.into_string()
}

/// Force the `_PathBuf` import in `install` to stay referenced under
/// the `#[path]`-imported integration-test build. `install.rs`
/// references `PathBuf` via its full public surface; without this
/// the test binary's compilation flags unused imports the bin
/// already consumes at runtime.
///
/// Same shim as `tests/sy_file_journey_e2e.rs`'s
/// `_force_install_module_used_under_integration_test` — kept inline
/// here so this test binary stays self-contained.
#[allow(dead_code)]
fn _force_install_used() {
    let _ = install::install;
    let _ = install::verify_signature;
    let _ = install::strip_signature_block;
    let _ = install::NO_SIGNATURE_ENV;
    let _ = install::InstallOpts::new(PathBuf::from("/tmp/sy-plugins"));
    let _ = install::InstallSource::Path(PathBuf::from("/tmp"));
    let _ = install::InstallSource::Git {
        url: "git+file:///tmp".into(),
        rev: None,
    };
    let _ = install::InstalledPlugin {
        id: String::new(),
        dir: PathBuf::new(),
    };
    let _ = install::InstallError::Io("x".into());
    let _ = install::InstallError::ManifestInvalid("x".into());
    let _ = install::InstallError::SignatureInvalid("x".into());
    let _ = registry::Registry::manifest_dir;
    // Step 27 additions — `Registry::empty` + `discover_empty` are
    // bin-internal fallbacks for the file plane's `app::run` and
    // aren't reached from this binary's `#[test]` bodies.
    let _ = registry::Registry::empty;
    let _ = registry::discover_empty;
}
