use std::{os::unix::fs::PermissionsExt, process::Command};

#[test]
fn newer_amd_layout_is_reported_as_incompatible() {
    let root = tempfile::tempdir().unwrap();
    let lib = root.path().join("onnxruntime/lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("libonnxruntime.so.1"), []).unwrap();
    let output = Command::new("bash")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/check-npu-upgrade.sh"
        ))
        .env("SY_RYZENAI_VENV_ROOT", root.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Ryzen AI 1.8 layout"));
}

#[test]
fn upgrade_check_works_with_only_the_aiplane_command() {
    let root = tempfile::tempdir().unwrap();
    let site = root.path().join("venv/lib/python3.14/site-packages");
    let capi = site.join("onnxruntime/capi");
    let voe = site.join("voe-4.0-linux_x86_64");
    let xrt = root.path().join("xrt/lib");
    let bin = root.path().join("bin");
    for dir in [&capi, &voe, &xrt, &bin] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(capi.join("libonnxruntime.so.1"), []).unwrap();
    std::fs::write(voe.join("vaip_config.json"), "{}").unwrap();
    std::fs::write(root.path().join("accel0"), []).unwrap();
    let sy = bin.join("sy");
    std::fs::write(&sy, "#!/bin/sh\ncase \"$1 $2\" in\n  'aiplane status') echo '{\"daemon_running\":true,\"embed_backend\":\"vitisai\"}';;\n  'aiplane run') echo '{\"output\":{\"vector\":[0.1,0.2]}}';;\n  *) exit 42;;\nesac\n").unwrap();
    std::fs::set_permissions(&sy, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new("bash")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/check-npu-upgrade.sh"
        ))
        .env("SY_RYZENAI_VENV_ROOT", root.path().join("venv"))
        .env("SY_NPU_DEVICE_PATH", root.path().join("accel0"))
        .env("SY_XRT_LIB_DIR", xrt)
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("2 dimensions"));
}
