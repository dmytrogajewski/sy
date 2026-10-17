use std::{
    io::Write,
    process::{Command, Stdio},
};

use anyhow::{Context, Result};

pub fn run_fuzzel(input: &str, prompt: &str, password: bool) -> Result<String> {
    let mut cmd = Command::new("fuzzel");
    cmd.arg("--dmenu");
    if input.is_empty() {
        cmd.arg(format!("--prompt-only={prompt}"));
    } else {
        cmd.arg(format!("--prompt={prompt}"));
    }
    if password {
        cmd.arg("--password");
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("spawn fuzzel")?;
    if let Some(mut stdin) = child.stdin.take()
        && !input.is_empty()
    {
        stdin.write_all(input.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn notify(summary: &str, body: &str) {
    let _ = Command::new("notify-send")
        .args(["-a", "sy", summary, body])
        .status();
}
