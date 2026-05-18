//! sy AGT — unified ACP-driven agent subsystem.

use std::{fmt, path::PathBuf};

use anyhow::{Context, Result};
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
pub mod audit;
pub mod client;
pub mod daemon;
pub mod inspector;
pub mod launcher;
pub mod menu;
pub mod permission;
pub mod policy;
pub mod proc_scan;
pub mod protocol;
pub mod registry;
pub mod sandbox;
pub mod session;
pub mod waybar;
pub mod wire;

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
    /// Sandbox a single binary invocation under the in-process layers
    /// (SPEC §4.4 step 3). Hidden — primarily the re-exec target the
    /// Step 4 `systemd-run --scope` wrapper invokes; also lets
    /// operators spot-check the sandbox manually
    /// (`sy agt sandbox-exec --profile strict --bin /bin/cat --
    /// /etc/shadow`).
    #[command(hide = true, name = "sandbox-exec")]
    SandboxExec {
        /// Policy profile name (strict | normal | trusted) under
        /// `<cwd>/configs/policy/profiles/`.
        #[arg(long, env = "SY_AGT_SANDBOX_PROFILE", default_value = "strict")]
        profile: String,
        /// Absolute path to the binary to execute.
        #[arg(long)]
        bin: PathBuf,
        /// Working directory used as `$REPO` for policy `$REPO` expansion.
        #[arg(long, env = "SY_AGT_CWD")]
        cwd: Option<PathBuf>,
        /// argv values to forward verbatim to the sandboxed binary.
        #[arg(trailing_var_arg = true)]
        argv: Vec<String>,
    },
    /// Sandbox a single binary invocation inside a `systemd-run --user
    /// --scope` transient cgroup that supplies `MemoryMax`,
    /// `TasksMax`, and `RuntimeMaxSec` (SPEC §4.4 step 4). Hidden —
    /// the outer entry point the daemon's `Decision::Allow` path will
    /// eventually call. The scope's child re-execs `sy agt
    /// sandbox-exec` so the in-process Landlock + seccomp layers
    /// stack on top of the cgroup caps; namespacing directives
    /// (`NoNewPrivileges`, `PrivateNetwork`, `ProtectSystem`) are
    /// rejected by `--user --scope` and handled by the Step 3 layers
    /// instead (see `sandbox/scope.rs` head comment).
    /// (`sy agt sandbox-run --profile normal --bin /usr/bin/rg -- --version`)
    #[command(hide = true, name = "sandbox-run")]
    SandboxRun {
        #[arg(long, env = "SY_AGT_SANDBOX_PROFILE", default_value = "strict")]
        profile: String,
        #[arg(long)]
        bin: PathBuf,
        #[arg(long, env = "SY_AGT_CWD")]
        cwd: Option<PathBuf>,
        #[arg(trailing_var_arg = true)]
        argv: Vec<String>,
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
        AgtCmd::SandboxExec {
            profile,
            bin,
            cwd,
            argv,
        } => sandbox_exec(&profile, &bin, cwd.as_deref(), &argv),
        AgtCmd::SandboxRun {
            profile,
            bin,
            cwd,
            argv,
        } => sandbox_run(&profile, &bin, cwd.as_deref(), &argv),
    }
}

/// Re-exec entry point for SPEC §4.4 step 3's layered in-process
/// sandbox. Loads the named profile rooted at `cwd` (falling back to
/// the current directory), then hands `(bin, argv)` to
/// [`sandbox::fork_and_exec`] which applies PR_SET_NO_NEW_PRIVS →
/// Landlock → seccomp → execve. The process exits with the
/// sandboxed child's exit code so a future `systemd-run --scope`
/// wrapper (Step 4) can propagate the status untouched.
fn sandbox_exec(
    profile_name: &str,
    bin: &std::path::Path,
    cwd: Option<&std::path::Path>,
    argv: &[String],
) -> Result<()> {
    let cwd_buf = match cwd {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().context("current dir")?,
    };
    let policy_root = policy::resolver::resolve_policy_root(&cwd_buf)
        .context("locate policy/profiles for sandbox-exec")?;
    let tool_key = bin.file_stem().and_then(|s| s.to_str());
    let resolver = policy::Resolver::load(&policy_root, profile_name, tool_key, &cwd_buf)
        .with_context(|| format!("load policy {profile_name} from {}", policy_root.display()))?;
    let policy_sha = resolver.fingerprint();
    // Surface the resolved policy fingerprint so audit consumers can
    // correlate this invocation with the exact policy bytes in effect
    // (SPEC §4.4 step 2). Step 5 lifts this onto the journald record.
    tracing::debug!(
        policy_sha = policy_sha.as_str(),
        profile = profile_name,
        "sandbox-exec policy loaded"
    );
    let mut profile = policy::resolver::resolve_profile(
        &policy_root,
        profile_name,
        tool_key,
        &cwd_buf,
    )
    .with_context(|| format!("resolve profile {profile_name}"))?;
    // Landlock blocks execve when the binary itself isn't in
    // read_paths — the kernel can't load the image. The profile
    // can't know agent install locations a priori (claude lives in
    // `~/.local/share/claude/versions/<X>/`, goose in
    // `~/.cargo/bin`, …), so we extend read_paths with the
    // canonical binary path's parent dir at sandbox-exec time. The
    // sandbox is fundamentally a sandbox AROUND this binary — we
    // must be able to read it.
    extend_read_paths_with_bin_dir(&mut profile, bin);

    // SPEC §4.4 "Audit log": stamp the Allow decision before fork
    // and the post-exec status after the child returns. `Allow` is
    // the only decision that reaches the in-process sandbox layer —
    // `Deny` short-circuits in `Resolver::decide` and never gets
    // here; `ConsentRequired` is audited under
    // `AuditDecision::Consent` at the permission-prompt call site.
    let audit_dir = audit::default_audit_dir();
    let tool = bin.display().to_string();
    // arch-observability Step 4: lift the active trace_id (if any)
    // onto the audit record so journald + JSONL correlate back to
    // the originating IPC envelope. `request_id` rides through the
    // `SY_AGT_REQUEST_ID` env var when the daemon's Decision::Allow
    // path eventually invokes `sandbox-exec` via `systemd-run`; a
    // bare CLI invocation (no IPC envelope as entry point) leaves
    // it `None`, which is the correct shape for hand-typed runs.
    let trace_id = sy_core::obs::current_trace_ctx().map(|c| c.trace_id.0);
    let request_id = std::env::var("SY_AGT_REQUEST_ID")
        .ok()
        .and_then(|s| ulid::Ulid::from_string(&s).ok());
    audit::emit(
        &audit::AuditRecord::now(
            tool.clone(),
            policy_sha.clone(),
            audit::AuditDecision::Allow,
            argv.to_vec(),
        )
        .with_trace_id(trace_id.clone())
        .with_request_id(request_id),
        &audit_dir,
    );

    let status = sandbox::fork_and_exec(&profile, bin, argv)?;

    // Post-exec record: same decision, with the exit code captured
    // in `reason` so operators can `jq 'select(.reason | startswith
    // ("exit="))'` the JSONL to find non-zero terminations.
    audit::emit(
        &audit::AuditRecord::now(tool, policy_sha, audit::AuditDecision::Allow, argv.to_vec())
            .with_reason(Some(format!("exit={}", status.code().unwrap_or(-1))))
            .with_trace_id(trace_id)
            .with_request_id(request_id),
        &audit_dir,
    );

    std::process::exit(status.code().unwrap_or(1));
}

/// Outer entry point for SPEC §4.4 step 4's `systemd-run --user
/// --scope` cgroup wrapper. Resolves the named profile to obtain the
/// `MemoryMax` / `TasksMax` / `RuntimeMaxSec` / `deny_network` caps,
/// then [`sandbox::scope::run_in_scope`] builds the `systemd-run`
/// argv and exits with the child's status. The scope's child is `sy
/// agt sandbox-exec`, which then layers in PR_SET_NO_NEW_PRIVS +
/// Landlock + seccomp from Step 3.
fn sandbox_run(
    profile_name: &str,
    bin: &std::path::Path,
    cwd: Option<&std::path::Path>,
    argv: &[String],
) -> Result<()> {
    let cwd_buf = match cwd {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().context("current dir")?,
    };
    let policy_root = policy::resolver::resolve_policy_root(&cwd_buf)
        .context("locate policy/profiles for sandbox-run")?;
    let tool_key = bin.file_stem().and_then(|s| s.to_str());
    let mut profile = policy::resolver::resolve_profile(
        &policy_root,
        profile_name,
        tool_key,
        &cwd_buf,
    )
    .with_context(|| format!("resolve profile {profile_name}"))?;
    extend_read_paths_with_bin_dir(&mut profile, bin);
    let status = sandbox::scope::run_in_scope(&profile, profile_name, bin, argv, &cwd_buf)?;
    std::process::exit(status.code().unwrap_or(1));
}

/// Extend `profile.read_paths` so Landlock allows execve of `bin` and
/// loading whatever lives in its install directory. Idempotent —
/// duplicates are silently skipped.
///
/// Two paths get added:
///   - The canonical (symlink-resolved) binary's parent directory —
///     the kernel can't load `claude` if Landlock blocks reading
///     `/home/.../.local/share/claude/versions/<X>/`.
///   - The path-as-given's parent dir — needed when `bin` is a
///     symlink under (e.g.) `~/.local/bin/` that the kernel resolves
///     at execve time. Both ends of the symlink chain have to be
///     reachable.
fn extend_read_paths_with_bin_dir(profile: &mut policy::schema::Profile, bin: &std::path::Path) {
    let mut add = |p: PathBuf| {
        if !profile.read_paths.iter().any(|existing| existing == &p) {
            profile.read_paths.push(p);
        }
    };
    if let Some(parent) = bin.parent() {
        if parent.as_os_str().is_empty() {
            // `bin` was a bare filename (no parent component); nothing
            // to add — execve will resolve it via $PATH which Landlock
            // can't restrict per-entry anyway.
        } else {
            add(parent.to_path_buf());
        }
    }
    if let Ok(canonical) = bin.canonicalize() {
        if let Some(parent) = canonical.parent() {
            add(parent.to_path_buf());
        }
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
            "{:<10} {:<8} {:<10} {:<25} SUMMARY",
            "ID", "AGENT", "STATUS", "CREATED"
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
    c.send_stream(&ClientReq::Tail {
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
        println!("{:<10} {:<6} VERSION", "AGENT", "OK");
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
