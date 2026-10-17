//! Dispatch independently packaged feature commands.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

pub fn power(action: Option<&str>, dry_run: bool, yes: bool, json: bool) -> Result<()> {
    let exe = power_exe();
    if !exe.is_file() {
        bail!(
            "sy power requires the sy-system-power package ({})",
            exe.display()
        );
    }
    let mut command = Command::new(&exe);
    if let Some(action) = action {
        command.arg(action);
    }
    if dry_run {
        command.arg("--dry-run");
    }
    if yes {
        command.arg("--yes");
    }
    if json {
        command.arg("--json");
    }
    Err(command.exec()).with_context(|| format!("run {}", exe.display()))
}

fn power_exe() -> PathBuf {
    if let Some(dir) = std::env::var_os("SY_FEATURE_BIN_DIR") {
        return PathBuf::from(dir).join("sy-system-power");
    }
    let system = PathBuf::from("/usr/libexec/sy/sy-system-power");
    let current = std::env::current_exe().ok();
    if current
        .as_ref()
        .is_some_and(|path| path.starts_with("/usr/libexec/sy"))
    {
        return system;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let local = PathBuf::from(home).join(".local/bin/sy-system-power");
        if local.is_file() {
            return local;
        }
    }
    if let Some(sibling) = current.map(|path| path.with_file_name("sy-system-power"))
        && sibling.is_file()
    {
        return sibling;
    }
    system
}
