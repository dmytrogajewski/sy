use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[test]
fn upgrade_check_runs_embed_before_verifying_live_backend() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let venv = tmp.path().join("venv");
    let site = venv.join("lib/python3.12/site-packages");
    let capi = site.join("onnxruntime/capi");
    let voe = site.join("voe-4.0-linux_x86_64");
    let xrt = tmp.path().join("xrt");
    for dir in [&bin, &capi, &voe, &xrt] {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(tmp.path().join("accel0"), "").unwrap();
    fs::write(capi.join("libonnxruntime.so.1"), "").unwrap();
    fs::write(voe.join("vaip_config.json"), "{}").unwrap();
    let fake = bin.join("sy");
    fs::write(&fake, "#!/bin/sh\necho \"$2\" >> \"$SY_UPGRADE_CALLS\"\ncase \"$2\" in\nrun) echo '{\"output\":{\"vector\":[0.5]}}';;\nstatus) echo '{\"daemon_running\":true,\"embed_backend\":\"vitisai\"}';;\nesac\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let fake_rpm = bin.join("rpm");
    fs::write(
        &fake_rpm,
        "#!/bin/sh\nprintf 'amdxdna 2.0-1\\nxrt 2.20-1\\nunrelated 1-1\\n'\n",
    )
    .unwrap();
    fs::set_permissions(&fake_rpm, fs::Permissions::from_mode(0o755)).unwrap();
    let calls = tmp.path().join("calls");
    let output = Command::new("bash")
        .arg("scripts/check-npu-upgrade.sh")
        .env("SY_NPU_DEVICE_PATH", tmp.path().join("accel0"))
        .env("SY_RYZENAI_VENV_ROOT", &venv)
        .env("SY_XRT_LIB_DIR", &xrt)
        .env("SY_UPGRADE_CALLS", &calls)
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
    assert_eq!(fs::read_to_string(calls).unwrap(), "run\nstatus\n");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("amdxdna 2.0-1"), "{stdout}");
    assert!(stdout.contains("xrt 2.20-1"), "{stdout}");
    assert!(!stdout.contains("unrelated 1-1"), "{stdout}");
}
