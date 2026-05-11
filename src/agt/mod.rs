//! sy AGT — unified ACP-driven agent subsystem.

use std::{fmt, path::PathBuf};

use anyhow::Result;
use clap::Subcommand;

/// CLI-level error carrying a stable exit code (per CLIG).
/// `main.rs` downcasts the anyhow error to map it to `process::exit(code)`.
#[derive(Debug)]
pub struct AgtError {
    pub code: i32,
    pub msg: String,
}

impl fmt::Display for AgtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for AgtError {}

pub mod acp;
pub mod client;
pub mod daemon;
pub mod inspector;
pub mod launcher;
pub mod menu;
pub mod permission;
pub mod proc_scan;
pub mod protocol;
pub mod registry;
pub mod session;
pub mod waybar;

use crate::agt::{
    client::{stream_events, Client},
    protocol::{exit, ClientReply, ClientReq},
};

/// Map a daemon `Error` reply to an AgtError. The daemon uses `code: 2`
/// for not-found and `4` for agent-failed; we surface them through stable
/// process-exit codes per CLAUDE.md's CLIG section.
fn daemon_error(message: String, code: u16) -> anyhow::Error {
    let exit_code = match code {
        2 => exit::NO_SESSION,
        _ => exit::DAEMON_UNAVAILABLE,
    };
    AgtError {
        code: exit_code,
        msg: message,
    }
    .into()
}

fn unexpected_reply() -> anyhow::Error {
    AgtError {
        code: exit::DAEMON_UNAVAILABLE,
        msg: "unexpected daemon reply".into(),
    }
    .into()
}

#[derive(Subcommand)]
pub enum AgtCmd {
    /// Run the long-lived daemon (foreground; spawned by niri at startup)
    Daemon,
    /// Start a new agent session (Super+A entry point)
    Run {
        /// Working directory the agent runs in. Defaults to focused niri window's cwd.
        #[arg(long, env = "SY_AGT_CWD")]
        cwd: Option<PathBuf>,
        /// Agent name from agents.toml. Skips the picker if set.
        #[arg(long, env = "SY_AGT_AGENT")]
        agent: Option<String>,
        /// Initial prompt; if omitted, fuzzel asks for it.
        prompt: Option<String>,
        /// Read prompt from $EDITOR instead of fuzzel.
        #[arg(long)]
        editor: bool,
    },
    /// List managed sessions
    List {
        /// JSON output for machine consumption
        #[arg(long)]
        json: bool,
    },
    /// Send a follow-up prompt to a running session
    Prompt { session_id: String, text: String },
    /// Stop and remove a session
    Stop { session_id: String },
    /// Stream the transcript of a session
    Tail {
        session_id: String,
        #[arg(short, long)]
        follow: bool,
        #[arg(long)]
        no_replay: bool,
    },
    /// Fuzzel session picker (waybar AGT left-click)
    Menu,
    /// Waybar JSON output
    Waybar,
    /// Inspector TUI — runs inside the foot popup
    Inspect { session_id: String },
    /// Diagnostics: print registry + ping each agent's --version
    Diag {
        #[arg(long)]
        json: bool,
    },
}

pub fn dispatch(cmd: AgtCmd) -> Result<()> {
    match cmd {
        AgtCmd::Daemon => daemon::run_blocking(),
        AgtCmd::Run {
            cwd,
            agent,
            prompt,
            editor,
        } => launcher::run(launcher::RunOpts {
            cwd,
            agent,
            prompt,
            editor,
        }),
        AgtCmd::List { json } => list(json),
        AgtCmd::Prompt { session_id, text } => prompt_session(&session_id, &text),
        AgtCmd::Stop { session_id } => stop_session(&session_id),
        AgtCmd::Tail {
            session_id,
            follow,
            no_replay,
        } => tail(&session_id, follow, !no_replay),
        AgtCmd::Menu => menu::run(),
        AgtCmd::Waybar => waybar::run(),
        AgtCmd::Inspect { session_id } => inspector::run(&session_id),
        AgtCmd::Diag { json } => diag(json),
    }
}

pub fn socket_path() -> PathBuf {
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d).join("sy-agentd.sock");
        }
    }
    let uid = unsafe { libc_getuid() };
    PathBuf::from(format!("/run/user/{uid}/sy-agentd.sock"))
}

extern "C" {
    fn getuid() -> u32;
}
unsafe fn libc_getuid() -> u32 {
    getuid()
}

fn list(json: bool) -> Result<()> {
    let mut c = Client::connect()?;
    let reply = c.round_trip(&ClientReq::List)?;
    let sessions = match reply {
        ClientReply::ListReply { sessions } => sessions,
        ClientReply::Error { message, code } => return Err(daemon_error(message, code)),
        _ => return Err(unexpected_reply()),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
    } else if sessions.is_empty() {
        println!("(no managed sessions)");
    } else {
        println!(
            "{:<10} {:<8} {:<10} {:<25} {}",
            "ID", "AGENT", "STATUS", "CREATED", "SUMMARY"
        );
        for s in sessions {
            println!(
                "{:<10} {:<8} {:<10} {:<25} {}",
                s.id,
                s.agent,
                s.status.label(),
                s.created_at,
                s.summary
            );
        }
    }
    Ok(())
}

fn prompt_session(session_id: &str, text: &str) -> Result<()> {
    let mut c = Client::connect()?;
    match c.round_trip(&ClientReq::Prompt {
        session_id: session_id.to_string(),
        text: text.to_string(),
    })? {
        ClientReply::Ack => Ok(()),
        ClientReply::Error { message, code } => Err(daemon_error(message, code)),
        _ => Err(unexpected_reply()),
    }
}

fn stop_session(session_id: &str) -> Result<()> {
    let mut c = Client::connect()?;
    match c.round_trip(&ClientReq::Stop {
        session_id: session_id.to_string(),
    })? {
        ClientReply::Ack => Ok(()),
        ClientReply::Error { message, code } => Err(daemon_error(message, code)),
        _ => Err(unexpected_reply()),
    }
}

fn tail(session_id: &str, follow: bool, replay: bool) -> Result<()> {
    let mut c = Client::connect()?;
    c.send(&ClientReq::Tail {
        session_id: session_id.to_string(),
        follow,
        replay,
    })?;
    let mut err: Option<anyhow::Error> = None;
    stream_events(&mut c, |reply| match reply {
        ClientReply::Event { event: e } => {
            match serde_json::to_string(&e) {
                Ok(s) => println!("{s}"),
                Err(e) => {
                    err = Some(e.into());
                    return false;
                }
            }
            true
        }
        ClientReply::Error { message, code } => {
            err = Some(daemon_error(message, code));
            false
        }
        _ => true,
    })?;
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn diag(json: bool) -> Result<()> {
    // Local probe: works even without the daemon.
    let agents = registry::load()?;
    let mut entries: Vec<protocol::DiagEntry> = Vec::new();
    for a in &agents {
        let r = std::process::Command::new(&a.command)
            .args(&a.version_args)
            .stderr(std::process::Stdio::null())
            .output();
        entries.push(protocol::DiagEntry {
            name: a.name.clone(),
            command: a.command.clone(),
            found: r.as_ref().map(|o| o.status.success()).unwrap_or(false),
            version: r
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_default(),
        });
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        println!("{:<10} {:<6} {}", "AGENT", "OK", "VERSION");
        for e in entries {
            println!(
                "{:<10} {:<6} {}",
                e.name,
                if e.found { "ok" } else { "miss" },
                e.version
            );
        }
        let sock = socket_path();
        if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
            println!("\ndaemon: running ({})", sock.display());
        } else {
            println!("\ndaemon: not running ({})", sock.display());
        }
    }
    Ok(())
}
