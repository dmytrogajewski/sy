use std::{fs, path::PathBuf, process::Command};

use anyhow::{Context, Result, bail};

pub fn toggle(key: &str) -> Result<()> {
    let Some(id) = key.strip_prefix("agt:") else {
        bail!("unsupported agent popup key: {key}");
    };
    let id: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    if id.is_empty() {
        bail!("empty agent session id");
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR for agent popup")?;
    let pid_file = PathBuf::from(runtime).join(format!("sy-agt-{id}.pid"));
    if let Ok(pid) = fs::read_to_string(&pid_file)
        && let Ok(pid) = pid.trim().parse::<i32>()
        && pid > 0
    {
        let cmdline = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let args: Vec<&[u8]> = cmdline.split(|byte| *byte == 0).collect();
        let app_id = format!("sy-agt-{id}");
        if args
            .windows(2)
            .any(|pair| pair[0] == b"--app-id" && pair[1] == app_id.as_bytes())
        {
            let _ = Command::new("kill").arg(pid.to_string()).status();
            fs::remove_file(pid_file)?;
            return Ok(());
        }
    }
    let child = Command::new("foot")
        .args([
            "--app-id",
            &format!("sy-agt-{id}"),
            "-e",
            "sy",
            "agt",
            "inspect",
            &id,
        ])
        .spawn()
        .context("open agent inspector")?;
    fs::write(pid_file, child.id().to_string())?;
    Ok(())
}
