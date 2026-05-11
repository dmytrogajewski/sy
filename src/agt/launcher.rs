//! `sy agt run` — Super+A entry point.
//! 1. Detect cwd from focused niri window (fallback $HOME).
//! 2. Pick agent via fuzzel (or --agent).
//! 3. Read prompt via fuzzel single-line, or $EDITOR if --editor / `:e`.
//! 4. Dispatch Run to sy-agentd.

use std::{
    env, fs,
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
};

use anyhow::{anyhow, Result};

use crate::{
    agt::{
        client::Client,
        protocol::{ClientReply, ClientReq},
        registry,
    },
    wifi,
};

pub struct RunOpts {
    pub cwd: Option<PathBuf>,
    pub agent: Option<String>,
    pub prompt: Option<String>,
    pub editor: bool,
}

pub fn run(opts: RunOpts) -> Result<()> {
    let cwd = opts
        .cwd
        .or_else(detect_cwd_from_niri)
        .or_else(home_dir)
        .ok_or_else(|| anyhow!("could not determine cwd"))?;

    let agent = match opts.agent {
        Some(a) => a,
        None => pick_agent()?,
    };
    if agent.is_empty() {
        return Ok(());
    }

    let prompt = match (opts.prompt, opts.editor) {
        (Some(p), false) => p,
        (Some(p), true) => p,
        (None, true) => prompt_via_editor()?,
        (None, false) => match fuzzel_prompt()? {
            Some(p) if p.trim() == ":e" => prompt_via_editor()?,
            Some(p) => p,
            None => return Ok(()),
        },
    };
    if prompt.trim().is_empty() {
        return Ok(());
    }

    let mut client = Client::connect()?;
    let reply = client.round_trip(&ClientReq::Run {
        agent: agent.clone(),
        cwd,
        prompt,
    })?;
    match reply {
        ClientReply::RunReply { session_id } => {
            // Cache last-used agent so the launcher surfaces it first next time.
            if let Some(cache) = last_agent_path() {
                if let Some(p) = cache.parent() {
                    let _ = fs::create_dir_all(p);
                }
                let _ = fs::write(&cache, &agent);
            }
            wifi::notify("agents", &format!("{agent}: {session_id}"));
            let _ = Command::new("sh")
                .arg("-c")
                .arg("pkill -RTMIN+9 waybar 2>/dev/null")
                .status();
            Ok(())
        }
        ClientReply::Error { message, .. } => Err(anyhow!("daemon: {message}")),
        other => Err(anyhow!("unexpected reply: {other:?}")),
    }
}

fn pick_agent() -> Result<String> {
    let mut names: Vec<String> = registry::load()?.into_iter().map(|s| s.name).collect();
    if names.is_empty() {
        return Err(anyhow!("no agents in registry"));
    }
    if let Some(last) = last_agent() {
        if let Some(pos) = names.iter().position(|n| n == &last) {
            names.swap(0, pos);
        }
    }
    let input = names.join("\n");
    let choice = wifi::run_fuzzel(&input, "agent » ", false)?;
    Ok(choice.trim().to_string())
}

fn fuzzel_prompt() -> Result<Option<String>> {
    let s = wifi::run_fuzzel("", "prompt » ", false)?;
    let s = s.trim().to_string();
    if s.is_empty() {
        Ok(None)
    } else {
        Ok(Some(s))
    }
}

fn prompt_via_editor() -> Result<String> {
    let editor = env::var("EDITOR").unwrap_or_else(|_| "nano".into());
    let tmp = tempfile_path("sy-agt-prompt", "md");
    fs::write(&tmp, "# Type your prompt below this line; save & exit when done.\n\n")?;
    // Spawn editor inside a foot popup so it works under wayland regardless of
    // where Super+A was triggered from.
    let st = Command::new("foot")
        .args([
            "--app-id=sy-agt-launcher-editor",
            "-e",
            &editor,
            tmp.to_str().unwrap(),
        ])
        .status()?;
    if !st.success() {
        return Err(anyhow!("editor exited with {st}"));
    }
    let mut content = String::new();
    fs::File::open(&tmp)?.read_to_string(&mut content)?;
    let _ = fs::remove_file(&tmp);
    let cleaned: String = content
        .lines()
        .filter(|l| !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(cleaned.trim().to_string())
}

fn tempfile_path(stem: &str, ext: &str) -> PathBuf {
    let dir = env::var("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/tmp"));
    dir.join(format!(
        "{stem}-{}.{ext}",
        uuid::Uuid::new_v4().simple()
    ))
}

pub fn detect_cwd_from_niri() -> Option<PathBuf> {
    let out = Command::new("niri")
        .args(["msg", "--json", "focused-window"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let pid = v.get("pid").and_then(|p| p.as_u64())?;
    fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

fn home_dir() -> Option<PathBuf> {
    env::var("HOME").ok().map(PathBuf::from)
}

fn last_agent_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".cache/sy/agt/last-agent"))
}

fn last_agent() -> Option<String> {
    last_agent_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
}
