#[path = "../src/sparkplane_bridge.rs"]
#[allow(dead_code)]
mod bridge;

#[test]
fn scoped_apply_never_requires_desktop_themes_or_updates_other_components() {
    let root = tempfile::tempdir().unwrap();
    let (pin, manifest, signature, binary) = fixture();
    bridge::install_verified(
        &root.path().join("data/sparkplane/client"),
        &pin,
        &manifest,
        &signature,
        &binary,
    )
    .unwrap();
    std::fs::write(
        root.path().join("sy.toml"),
        format!(
            "[integrations.sparkplane]\nenabled=true\n[integrations.sparkplane.release]\n{}",
            toml::to_string(&pin).unwrap()
        ),
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .args(["apply", "--only", "sparkplane", "--root"])
        .arg(root.path())
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.path().join("config").exists());
}

#[test]
fn scoped_dry_run_reports_json_without_installing_the_client() {
    let root = tempfile::tempdir().unwrap();
    let (pin, _, _, _) = fixture();
    std::fs::write(
        root.path().join("sy.toml"),
        format!(
            "[integrations.sparkplane]\nenabled=true\n[integrations.sparkplane.release]\n{}",
            toml::to_string(&pin).unwrap()
        ),
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .args(["apply", "--dry-run", "--json", "--root"])
        .arg(root.path())
        .env("SY_APPLY_ONLY", "sparkplane")
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report,
        serde_json::json!({"schema":"sy.integration-apply/v1","integration":"sparkplane","enabled":true,"dry_run":true})
    );
    assert!(!root.path().join("data").exists());
}

#[test]
fn protocol_probe_waits_for_a_transient_executable_writer_to_close() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let (pin, manifest, signature, binary) = fixture();
    bridge::install_verified(root.path(), &pin, &manifest, &signature, &binary).unwrap();
    let path = root.path().join("current/sparkplane");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o555)).unwrap();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            drop(writer);
        });
        bridge::prepare(root.path(), &[]).unwrap();
    });
}

#[test]
fn missing_managed_client_has_an_actionable_error() {
    let directory = tempfile::tempdir().unwrap();
    let error = bridge::prepare(directory.path(), &["dgx".into(), "status".into()])
        .unwrap_err()
        .to_string();
    assert!(error.contains("sy apply"), "{error}");
}

fn fixture() -> (bridge::ReleasePin, Vec<u8>, String, Vec<u8>) {
    let binary = b"#!/bin/sh\nif [ \"$1\" = --bridge-protocol ]; then echo sparkplane.bridge/v1; exit; fi\nprintf '%s\\n' \"$@\"\nexit 17\n".to_vec();
    signed_fixture(binary)
}

fn signed_fixture(binary: Vec<u8>) -> (bridge::ReleasePin, Vec<u8>, String, Vec<u8>) {
    use sha2::{Digest, Sha256};
    let keys = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let pin = bridge::ReleasePin {
        version: "0.1.0".into(),
        target: format!("{}-unknown-linux-gnu", std::env::consts::ARCH),
        sha256: format!("{:x}", Sha256::digest(&binary)),
        public_key: keys.pk.to_base64(),
    };
    let manifest = format!("{}  sparkplane-{}\n", pin.sha256, pin.target).into_bytes();
    let signature = minisign::sign(None, &keys.sk, std::io::Cursor::new(&manifest), None, None)
        .unwrap()
        .to_string();
    (pin, manifest, signature, binary)
}

#[test]
fn unsupported_protocol_is_not_activated() {
    let root = tempfile::tempdir().unwrap();
    let (pin, manifest, signature, binary) =
        signed_fixture(b"#!/bin/sh\necho incompatible\n".to_vec());
    assert!(bridge::install_verified(root.path(), &pin, &manifest, &signature, &binary).is_err());
    assert!(!root.path().join("current").exists());
}

#[test]
fn applying_an_installed_pin_is_offline_and_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let (pin, manifest, signature, binary) = fixture();
    bridge::install_verified(root.path(), &pin, &manifest, &signature, &binary).unwrap();
    let before = std::fs::symlink_metadata(root.path().join("current"))
        .unwrap()
        .modified()
        .unwrap();
    bridge::apply(
        Some(&bridge::Integration {
            enabled: true,
            release: Some(pin),
        }),
        root.path(),
        false,
    )
    .unwrap();
    assert_eq!(
        std::fs::symlink_metadata(root.path().join("current"))
            .unwrap()
            .modified()
            .unwrap(),
        before
    );
}

#[test]
fn signed_client_preserves_arguments_output_and_exit_code() {
    let root = tempfile::tempdir().unwrap();
    let (pin, manifest, signature, binary) = fixture();
    bridge::install_verified(root.path(), &pin, &manifest, &signature, &binary).unwrap();
    let output = bridge::prepare(
        root.path(),
        &["host".into(), "--".into(), "a b;$(false)".into()],
    )
    .unwrap()
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(17));
    assert_eq!(output.stdout, b"host\n--\na b;$(false)\n");
}

#[test]
fn corrupted_binary_is_rejected_before_activation() {
    let root = tempfile::tempdir().unwrap();
    let (pin, manifest, signature, _) = fixture();
    assert!(
        bridge::install_verified(root.path(), &pin, &manifest, &signature, b"corrupt").is_err()
    );
    assert!(!root.path().join("current").exists());
}

#[test]
fn wrong_signature_is_rejected_before_activation() {
    let root = tempfile::tempdir().unwrap();
    let (pin, manifest, _, binary) = fixture();
    let (_, _, other_signature, _) = fixture();
    assert!(
        bridge::install_verified(root.path(), &pin, &manifest, &other_signature, &binary).is_err()
    );
    assert!(!root.path().join("current").exists());
}

#[test]
fn disabled_integration_does_not_create_directories() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("absent");
    bridge::apply(None, &destination, false).unwrap();
    assert!(!destination.exists());
}

#[test]
fn dry_run_does_not_download_or_install() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("absent");
    let (pin, _, _, _) = fixture();
    let integration = bridge::Integration {
        enabled: true,
        release: Some(pin),
    };
    bridge::apply(Some(&integration), &destination, true).unwrap();
    assert!(!destination.exists());
}

#[test]
fn reinstall_rejects_corrupt_receipt_without_changing_activation() {
    let root = tempfile::tempdir().unwrap();
    let (pin, manifest, signature, binary) = fixture();
    bridge::install_verified(root.path(), &pin, &manifest, &signature, &binary).unwrap();
    let current = root.path().join("current");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        current.join("release.json"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    std::fs::write(current.join("release.json"), b"{}").unwrap();
    let before = std::fs::symlink_metadata(&current)
        .unwrap()
        .modified()
        .unwrap();
    assert!(bridge::install_verified(root.path(), &pin, &manifest, &signature, &binary).is_err());
    assert_eq!(
        std::fs::symlink_metadata(current)
            .unwrap()
            .modified()
            .unwrap(),
        before
    );
}

#[test]
fn sy_bridge_runs_without_repository_or_amd_runtime() {
    let root = tempfile::tempdir().unwrap();
    let (pin, manifest, signature, binary) = fixture();
    bridge::install_verified(
        &root.path().join("sparkplane/client"),
        &pin,
        &manifest,
        &signature,
        &binary,
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .args(["spark", "host", "status", "--json"])
        .current_dir(root.path())
        .env("XDG_DATA_HOME", root.path())
        .env("SY_ROOT", root.path().join("absent-repository"))
        .env("ORT_DYLIB_PATH", "/nonexistent/amd/runtime")
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(17),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"host\nstatus\n--json\n");
}

#[test]
fn canonical_environment_overrides_legacy_environment() {
    let root = tempfile::tempdir().unwrap();
    let (pin, manifest, signature, binary) = signed_fixture(b"#!/bin/sh\nif [ \"$1\" = --bridge-protocol ]; then echo sparkplane.bridge/v1; exit; fi\nprintf '%s\\n' \"$SPARKPLANE_MODEL\" \"$SPARKPLANE_HOST\"\n".to_vec());
    bridge::install_verified(
        &root.path().join("sparkplane/client"),
        &pin,
        &manifest,
        &signature,
        &binary,
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .args(["spark", "host", "status"])
        .env_clear()
        .env("XDG_DATA_HOME", root.path())
        .env("SY_SPARK_HOST", "legacy-host")
        .env("SY_SPARK_MODEL", "legacy-model")
        .env("SPARKPLANE_MODEL", "canonical")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"canonical\nlegacy-host\n");
}

#[test]
fn bridge_preserves_non_utf8_arguments() {
    use std::os::unix::ffi::OsStringExt;
    let root = tempfile::tempdir().unwrap();
    let (pin, manifest, signature, binary) = fixture();
    bridge::install_verified(
        &root.path().join("sparkplane/client"),
        &pin,
        &manifest,
        &signature,
        &binary,
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .args(["spark", "host"])
        .arg(std::ffi::OsString::from_vec(vec![0xff, 0xfe]))
        .env("XDG_DATA_HOME", root.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(17));
    assert_eq!(output.stdout, b"host\n\xff\xfe\n");
}

#[test]
fn bridge_preserves_signal_termination_without_a_wrapper_process() {
    use std::os::unix::process::ExitStatusExt;
    let root = tempfile::tempdir().unwrap();
    let (pin, manifest, signature, binary) = signed_fixture(b"#!/bin/sh\nif [ \"$1\" = --bridge-protocol ]; then echo sparkplane.bridge/v1; exit; fi\nkill -TERM $$\n".to_vec());
    bridge::install_verified(
        &root.path().join("sparkplane/client"),
        &pin,
        &manifest,
        &signature,
        &binary,
    )
    .unwrap();
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .args(["spark", "host"])
        .env_clear()
        .env("XDG_DATA_HOME", root.path())
        .status()
        .unwrap();
    assert_eq!(status.signal(), Some(libc::SIGTERM));
}

#[test]
fn bridge_inherits_all_terminal_descriptors_and_keeps_the_original_pid() {
    let root = tempfile::tempdir().unwrap();
    let (pin, manifest, signature, binary) = signed_fixture(b"#!/bin/sh\nif [ \"$1\" = --bridge-protocol ]; then echo sparkplane.bridge/v1; exit; fi\n[ -t 0 ] && [ -t 1 ] && [ -t 2 ] || exit 19\nprintf 'tty:%s\\n' \"$$\"\nexit 17\n".to_vec());
    bridge::install_verified(
        &root.path().join("sparkplane/client"),
        &pin,
        &manifest,
        &signature,
        &binary,
    )
    .unwrap();
    let output = std::process::Command::new("python3").args(["-c",
        "import os,pty,subprocess,sys; master,slave=pty.openpty(); p=subprocess.Popen([sys.argv[1],'spark','host'],stdin=slave,stdout=slave,stderr=slave); code=p.wait(timeout=10); os.close(slave); data=os.read(master,4096); os.close(master); assert code==17,(code,data); assert data==f'tty:{p.pid}\\r\\n'.encode(),data",
        env!("CARGO_BIN_EXE_sy")]).env_clear().env("XDG_DATA_HOME", root.path()).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
