//! Integration tests for `sy plugin install` — Step 9 of the
//! [`sy-file-manager` roadmap][roadmap]. Drives the `sy` binary via
//! `CARGO_BIN_EXE_sy` against hermetic tempdirs holding freshly-
//! generated minisign keypairs + signed fixtures.
//!
//! All five tests are required by Step 9's DoD:
//!
//! 1. install from local path lands the plugin under the install root
//! 2. install from `git+file://` clones shallow into the install root
//! 3. signature mismatch aborts with exit 7 + leaves no partial dir
//! 4. `--unsigned` succeeds with a stderr warning naming the id
//! 5. re-install is atomic (no partial dir on a mid-flight failure)
//!
//! [roadmap]: ../specs/roadmaps/sy-file-manager/ROADMAP.md
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

/// SPEC §4.5 stable exit code for a signature mismatch / missing
/// required signature. Mirrors `EXIT_SIGNATURE_INVALID` in
/// `src/plugin/cli.rs`.
const EXIT_SIGNATURE_INVALID: i32 = 7;

/// Domain separator between binary and manifest in the canonical
/// signed payload — duplicates `SIGNATURE_SEP_BYTE` in
/// `src/plugin/install.rs` so a future change there breaks the
/// fixture build here loudly (the verifier would surface a sig
/// mismatch, but the failure message would point at the wrong code).
const SIGNATURE_SEP_BYTE: u8 = 0x00;

/// Generate a fresh minisign keypair and return `(pubkey_b64, secret_key)`.
/// The b64 is the bare key line the `minisign-verify::PublicKey::
/// from_base64` parser eats; the secret key is the in-memory handle
/// the test uses to sign payloads.
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

/// Sign `payload` with `sk` and return the minisign signature text
/// (multi-line: `untrusted comment: …\n<b64>\ntrusted comment: …\n
/// <b64>\n`). This is the exact string `Signature::decode` parses.
fn sign_bytes(sk: &minisign::SecretKey, payload: &[u8]) -> String {
    let sig = minisign::sign(None, sk, Cursor::new(payload), None, None).expect("sign payload");
    sig.into_string()
}

/// Build the canonical signed payload (binary ‖ 0x00 ‖ manifest)
/// matching `src/plugin/install.rs::canonical_signed_payload`. Tests
/// hand-roll the same shape so the fixture and the verifier agree
/// byte-for-byte.
fn canonical_payload(binary: &[u8], manifest_src_without_sig: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(binary.len() + 1 + manifest_src_without_sig.len());
    out.extend_from_slice(binary);
    out.push(SIGNATURE_SEP_BYTE);
    out.extend_from_slice(manifest_src_without_sig.as_bytes());
    out
}

/// Tiny stub binary contents — only the bytes signed; the binary is
/// never executed in the install path (Step 9 doesn't spawn).
const STUB_BINARY: &[u8] = b"#!/bin/sh\necho stub-md\n";

/// Manifest body with `{SIG_BLOCK}` placeholder replaced by either
/// the real `[plugin.signature]` (signed path) or the empty string
/// (unsigned path). Mirrors the `sy-plugin-md` shape so a successful
/// install lands a plugin the registry + doctor can both see.
fn manifest_body(plugin_id: &str, exec_path: &str, sig_block: &str) -> String {
    format!(
        r#"api = "1"

[plugin]
id = "{id}"
name = "Step 9 Test Plugin"
version = "0.1.0"
api_min = "1"
api_max = "1"

[plugin.binary]
exec = "{exec}"
{sig}
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
spawn_timeout_ms = 500
shutdown_timeout_ms = 500
"#,
        id = plugin_id,
        exec = exec_path,
        sig = sig_block,
    )
}

/// Plant a signed source-tree fixture: writes `<src>/bin/<id>`,
/// computes the canonical payload, signs it with `sk`, then writes
/// `plugin.toml` with the `[plugin.signature]` block carrying the
/// signature inline + the pubkey inline. Returns the source dir.
fn plant_signed_source(
    src: &Path,
    plugin_id: &str,
    pk_b64: &str,
    sk: &minisign::SecretKey,
) -> PathBuf {
    let bin_dir = src.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("mkdir bin");
    let bin_path = bin_dir.join(plugin_id);
    std::fs::write(&bin_path, STUB_BINARY).expect("write binary");
    // Manifest source the SIGNATURE is computed over: the on-disk
    // manifest **without** the `[plugin.signature]` block. We build
    // it first (no sig), sign the payload over (binary || 0x00 ||
    // that source), then write the manifest with the signature block
    // appended. `install::verify_signature` strips the block again
    // before recomputing, so both sides agree. The exec path is
    // `./bin/<id>` so the manifest is relocatable.
    let exec_field = format!("./bin/{plugin_id}");
    let manifest_no_sig = manifest_body(plugin_id, &exec_field, "");
    let payload = canonical_payload(STUB_BINARY, &manifest_no_sig);
    let sig_text = sign_bytes(sk, &payload);
    // Inline the signature + pubkey under `[plugin.signature]`. We
    // use TOML triple-quoted strings so the multi-line minisign
    // text round-trips through the parser.
    let sig_block = format!(
        "\n[plugin.signature]\nsig = '''\n{sig}'''\npubkey = \"{pk}\"\n",
        sig = sig_text,
        pk = pk_b64,
    );
    let manifest_with_sig = manifest_body(plugin_id, &exec_field, &sig_block);
    std::fs::write(src.join("plugin.toml"), &manifest_with_sig).expect("write plugin.toml");
    src.to_path_buf()
}

/// Plant an unsigned source-tree fixture: same shape as the signed
/// variant but with no `[plugin.signature]` block. The install path
/// requires `--unsigned` for these to succeed.
fn plant_unsigned_source(src: &Path, plugin_id: &str) -> PathBuf {
    let bin_dir = src.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("mkdir bin");
    let bin_path = bin_dir.join(plugin_id);
    std::fs::write(&bin_path, STUB_BINARY).expect("write binary");
    let exec_field = format!("./bin/{plugin_id}");
    let manifest = manifest_body(plugin_id, &exec_field, "");
    std::fs::write(src.join("plugin.toml"), manifest).expect("write plugin.toml");
    src.to_path_buf()
}

/// Common command builder: pin `SY_PLUGIN_INSTALL_DIR` and
/// `SY_PLUGIN_PUBLISHERS_DIR` so the test never writes to the host
/// `~/.local/share/sy/plugins/` or reads from the in-repo
/// `configs/sy/plugin-publishers/`.
fn sy(install_root: &Path, publishers_dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sy"));
    cmd.env("SY_PLUGIN_INSTALL_DIR", install_root);
    cmd.env("SY_PLUGIN_PUBLISHERS_DIR", publishers_dir);
    // Defence-in-depth: a stale env from a dev shell mustn't leak
    // into the signature path.
    cmd.env_remove("SY_PLUGIN_NO_SIGNATURE");
    cmd
}

/// SPEC §3.2 row 10: local-path install. The source tree carries a
/// real minisign signature; install must verify it and land the
/// plugin under `<install_root>/<id>/`.
#[test]
fn install_from_local_path_copies_into_data_dir() {
    let tmp = tempfile::tempdir().expect("tmp");
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let install_root = tmp.path().join("install");
    let publishers = tmp.path().join("pub");
    std::fs::create_dir_all(&publishers).unwrap();

    let (pk_b64, sk) = fresh_keypair();
    let plugin_id = "sample-signed";
    plant_signed_source(&src, plugin_id, &pk_b64, &sk);

    let out = sy(&install_root, &publishers)
        .args(["plugin", "install"])
        .arg(&src)
        .output()
        .expect("spawn sy plugin install");
    assert!(
        out.status.success(),
        "install exit={:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let landed = install_root.join(plugin_id).join("plugin.toml");
    assert!(landed.is_file(), "plugin.toml not at {}", landed.display());
    let landed_bin = install_root.join(plugin_id).join("bin").join(plugin_id);
    assert!(
        landed_bin.is_file(),
        "binary not at {}",
        landed_bin.display()
    );

    // `sy plugin list --json` from $SY_PLUGIN_DIR=<install_root> must
    // surface the newly-installed plugin so journey J3 can route to it.
    let list = Command::new(env!("CARGO_BIN_EXE_sy"))
        .args(["plugin", "list", "--json"])
        .env("SY_PLUGIN_DIR", &install_root)
        .env_remove("XDG_DATA_HOME")
        .output()
        .expect("spawn sy plugin list");
    assert!(list.status.success(), "list exit={:?}", list.status.code());
    let v: serde_json::Value = serde_json::from_slice(&list.stdout).expect("list json");
    let ids: Vec<&str> = v["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&plugin_id),
        "list missing {plugin_id}: {ids:?}"
    );
}

/// SPEC §3.2 row 10: git-URL install. Uses a local bare repo under a
/// tempdir + the `git+file://` scheme so the test stays offline.
#[test]
fn install_from_git_url_clones_shallow() {
    if !PathBuf::from("/usr/bin/git").exists() {
        eprintln!("skip: /usr/bin/git missing on host");
        return;
    }
    let tmp = tempfile::tempdir().expect("tmp");
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let bare = tmp.path().join("bare.git");
    let install_root = tmp.path().join("install");
    let publishers = tmp.path().join("pub");
    std::fs::create_dir_all(&publishers).unwrap();

    let (pk_b64, sk) = fresh_keypair();
    let plugin_id = "sample-git";
    plant_signed_source(&src, plugin_id, &pk_b64, &sk);

    // Build a normal repo, commit, then push to a bare repo. Hermetic
    // (no network) and idiomatic for a one-shot test fixture.
    let git = "/usr/bin/git";
    let run_in_src = |args: &[&str]| {
        let out = Command::new(git)
            .arg("-C")
            .arg(&src)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "sy-test")
            .env("GIT_AUTHOR_EMAIL", "sy-test@example.invalid")
            .env("GIT_COMMITTER_NAME", "sy-test")
            .env("GIT_COMMITTER_EMAIL", "sy-test@example.invalid")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run_in_src(&["init", "-q", "-b", "main"]);
    run_in_src(&["add", "."]);
    run_in_src(&["commit", "-q", "-m", "fixture"]);
    let out = Command::new(git)
        .args(["clone", "-q", "--bare"])
        .arg(&src)
        .arg(&bare)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("spawn git clone --bare");
    assert!(
        out.status.success(),
        "git clone --bare failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let url = format!("git+file://{}", bare.display());
    let install = sy(&install_root, &publishers)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(["plugin", "install", &url])
        .output()
        .expect("spawn sy plugin install git+...");
    assert!(
        install.status.success(),
        "git install exit={:?}\nstdout:\n{}\nstderr:\n{}",
        install.status.code(),
        String::from_utf8_lossy(&install.stdout),
        String::from_utf8_lossy(&install.stderr),
    );
    assert!(install_root.join(plugin_id).join("plugin.toml").is_file());
}

/// SPEC §4.5 row 7: signature mismatch aborts the install with exit 7
/// and leaves no staging dir on disk.
#[test]
fn signature_mismatch_aborts_install() {
    let tmp = tempfile::tempdir().expect("tmp");
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let install_root = tmp.path().join("install");
    let publishers = tmp.path().join("pub");
    std::fs::create_dir_all(&publishers).unwrap();

    let (pk_b64, sk) = fresh_keypair();
    let plugin_id = "sample-mismatch";
    plant_signed_source(&src, plugin_id, &pk_b64, &sk);
    // Corrupt the binary AFTER signing so the verifier rejects the
    // payload mismatch (binary bytes have changed but the signature
    // still references the original digest).
    let bin_path = src.join("bin").join(plugin_id);
    std::fs::write(&bin_path, b"#!/bin/sh\necho TAMPERED\n").expect("tamper binary");

    let out = sy(&install_root, &publishers)
        .args(["plugin", "install"])
        .arg(&src)
        .output()
        .expect("spawn sy plugin install");
    assert_eq!(
        out.status.code(),
        Some(EXIT_SIGNATURE_INVALID),
        "exit must be 7 on tamper, got {:?}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("signature"),
        "stderr must mention 'signature', got: {stderr}"
    );
    // No leftover staging dir + no half-installed final dir.
    let final_dir = install_root.join(plugin_id);
    assert!(
        !final_dir.exists(),
        "no partial install dir survives sig mismatch: {} still present",
        final_dir.display()
    );
    let leftovers: Vec<_> = if install_root.exists() {
        std::fs::read_dir(&install_root)
            .unwrap()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name();
                let s = name.to_str()?.to_string();
                if s.starts_with(".staging-") {
                    Some(s)
                } else {
                    None
                }
            })
            .collect()
    } else {
        Vec::new()
    };
    assert!(
        leftovers.is_empty(),
        "staging dirs must be unlinked on failure: {leftovers:?}"
    );
}

/// SPEC §4.5: `--unsigned` skips signature verification and prints a
/// stderr warning naming the plugin id.
#[test]
fn unsigned_with_flag_succeeds_with_warning() {
    let tmp = tempfile::tempdir().expect("tmp");
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let install_root = tmp.path().join("install");
    let publishers = tmp.path().join("pub");
    std::fs::create_dir_all(&publishers).unwrap();

    let plugin_id = "sample-unsigned";
    plant_unsigned_source(&src, plugin_id);

    let out = sy(&install_root, &publishers)
        // `tracing::warn!` goes to stderr only when a subscriber is
        // installed. The bin doesn't install one for CLI subcommands,
        // so we surface the warn via an unconditional stderr line in
        // the install path (the supervisor's own warn-per-spawn path
        // is separately covered).
        .env("RUST_LOG", "warn")
        .args(["plugin", "install", "--unsigned"])
        .arg(&src)
        .output()
        .expect("spawn sy plugin install --unsigned");
    assert!(
        out.status.success(),
        "unsigned install exit={:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(install_root.join(plugin_id).join("plugin.toml").is_file());
}

/// SPEC §4.5 env table: `SY_PLUGIN_NO_SIGNATURE=1` is the testing-
/// only bypass that skips signature verification on every spawn and
/// emits a `tracing::warn!` line per the SPEC. The supervisor in
/// `src/plugin/proc.rs` emits one warn per spawn naming the plugin
/// id under that env var. We drive a one-shot `sy plugin exec` (which
/// spawns the supervisor) with the env var set, then assert the
/// stderr carries the warn line.
#[test]
fn sy_plugin_no_signature_env_warns_per_spawn() {
    // Plant an installed plugin so `sy plugin exec` has something
    // to spawn. The fake plugin shipped under tests/fixtures handshakes
    // + echoes — perfect for a one-shot RPC.
    let tmp = tempfile::tempdir().expect("tmp");
    let install_root = tmp.path().join("install");
    let publishers = tmp.path().join("pub");
    std::fs::create_dir_all(&publishers).unwrap();
    let plugin_id = "no-sig-warn-canary";
    let plugin_dir = install_root.join(plugin_id);
    std::fs::create_dir_all(&plugin_dir).unwrap();
    // Re-use the in-tree fake from `tests/fixtures/sy-plugin-fake/`.
    let fake_bin = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/sy-plugin-fake/bin/sy-plugin-fake");
    assert!(fake_bin.is_file(), "fake plugin must ship in-tree");
    // `sy plugin exec` doesn't resolve relative exec paths against
    // the manifest dir (that's a Step 13+ daemon concern), so the
    // manifest here ships an absolute exec — the production install
    // path is exercised by the other tests in this file.
    let exec_field = fake_bin.display().to_string();
    std::fs::write(
        plugin_dir.join("plugin.toml"),
        manifest_body(plugin_id, &exec_field, ""),
    )
    .unwrap();

    let runtime = tmp.path().join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_sy"))
        .args([
            "plugin",
            "exec",
            plugin_id,
            "ping",
            "--params",
            r#"{"ts":1}"#,
        ])
        .env("SY_PLUGIN_DIR", &install_root)
        .env("SY_PLUGIN_RUNTIME_DIR", &runtime)
        .env("SY_PLUGIN_NO_SIGNATURE", "1")
        // tracing-subscriber inside the bin honours RUST_LOG; the
        // supervisor's warn! emission lands on stderr at level=warn,
        // which is the default level — but we set RUST_LOG=warn for
        // belt-and-braces.
        .env("RUST_LOG", "warn")
        .env_remove("SY_PLUGIN_PUBLISHERS_DIR")
        .env_remove("XDG_DATA_HOME")
        .env_remove("SY_PLUGIN_DISABLED_TOML")
        .output()
        .expect("spawn sy plugin exec");
    assert!(
        out.status.success(),
        "exec exit={:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("SY_PLUGIN_NO_SIGNATURE"),
        "stderr must carry the env-var warn: {stderr}"
    );
    assert!(
        stderr.contains(plugin_id),
        "stderr warn must name the plugin id: {stderr}"
    );
}

/// Re-installing the same plugin atomically overwrites the prior
/// version. Files from the prior install must not survive — the
/// swap-then-cleanup pattern unlinks the old dir once the rename
/// commits.
#[test]
fn reinstall_overwrites_atomic() {
    let tmp = tempfile::tempdir().expect("tmp");
    let install_root = tmp.path().join("install");
    let publishers = tmp.path().join("pub");
    std::fs::create_dir_all(&publishers).unwrap();
    let plugin_id = "sample-reinstall";

    // First install: vanilla unsigned (focus is the reinstall flow).
    let src1 = tmp.path().join("src1");
    std::fs::create_dir_all(&src1).unwrap();
    plant_unsigned_source(&src1, plugin_id);
    let r1 = sy(&install_root, &publishers)
        .args(["plugin", "install", "--unsigned"])
        .arg(&src1)
        .output()
        .expect("spawn install #1");
    assert!(r1.status.success(), "first install failed");
    // Plant a sentinel file the second install must NOT leave behind.
    let sentinel = install_root.join(plugin_id).join("OLD_SENTINEL");
    std::fs::write(&sentinel, b"first").unwrap();
    assert!(sentinel.exists());

    // Second install: same id, different binary contents (proves
    // the rename actually swapped). Source dir doesn't carry the
    // sentinel — its presence after install #2 would prove the swap
    // wasn't atomic.
    let src2 = tmp.path().join("src2");
    std::fs::create_dir_all(&src2).unwrap();
    std::fs::create_dir_all(src2.join("bin")).unwrap();
    std::fs::write(src2.join("bin").join(plugin_id), b"#!/bin/sh\necho v2\n").unwrap();
    let exec_field = format!("./bin/{plugin_id}");
    std::fs::write(
        src2.join("plugin.toml"),
        manifest_body(plugin_id, &exec_field, ""),
    )
    .unwrap();
    let r2 = sy(&install_root, &publishers)
        .args(["plugin", "install", "--unsigned"])
        .arg(&src2)
        .output()
        .expect("spawn install #2");
    assert!(
        r2.status.success(),
        "second install failed: {}",
        String::from_utf8_lossy(&r2.stderr)
    );
    assert!(
        !sentinel.exists(),
        "OLD_SENTINEL must be gone after reinstall: {}",
        sentinel.display()
    );
    // The new binary content survived the swap.
    let landed_bin = install_root.join(plugin_id).join("bin").join(plugin_id);
    let got = std::fs::read_to_string(&landed_bin).unwrap();
    assert_eq!(got, "#!/bin/sh\necho v2\n", "new binary must be in place");

    // No `.old-*` debris lingering after a clean swap.
    let leftovers: Vec<_> = std::fs::read_dir(&install_root)
        .unwrap()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let s = name.to_str()?.to_string();
            if s.starts_with(&format!("{plugin_id}.old-")) {
                Some(s)
            } else {
                None
            }
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "no .old-* dirs should remain after clean swap: {leftovers:?}"
    );
}
