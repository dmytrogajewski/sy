//! ACP (Agent Client Protocol) wire layer.
//!
//! One ACP child = one stdio JSON-RPC 2.0 connection. We classify each
//! inbound line by which JSON-RPC fields are present:
//!   * has `id` + `result|error`            → response to one of our requests
//!   * has `id` + `method`                  → reverse request from agent
//!   * no `id`, has `method`                → notification
//!
//! ## Sandboxing the ACP child (SPEC §4.4 step 4)
//!
//! When `spec.sandbox_profile = Some(<name>)`, [`AcpChild::spawn`]
//! wraps the agent process in `systemd-run --user --scope` plus a
//! re-exec into `sy agt sandbox-exec`. The wrapped argv is built by
//! [`build_acp_command`]; the cgroup scope supplies `MemoryMax` /
//! `TasksMax` / `RuntimeMaxSec` and the re-exec target layers
//! Landlock + seccomp on top — see `sandbox/scope.rs` for the full
//! envelope. The wrapper inherits across `execve`, so every tool the
//! agent spawns from inside the cgroup stays bounded too.
//!
//! `--scope` mode doesn't intercept stdio, so the JSON-RPC pipes on
//! the outer `systemd-run` command flow through to the agent
//! verbatim — that's why ACP keeps working through the wrapper.
//!
//! **systemd-run unavailable fallback**: when `which("systemd-run")`
//! fails we emit `tracing::warn!` and spawn the agent directly. The
//! in-process Landlock + seccomp layers require a re-exec target,
//! which itself needs `systemd-run` to wire the cgroup; falling back
//! to a direct spawn keeps dev hosts without a user manager working
//! at the cost of running that one agent unsandboxed. A future
//! tightening could add a direct `sandbox-exec` invocation (no scope)
//! as a middle-ground fallback.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{mpsc, oneshot, Mutex},
};

use crate::agt::{
    policy::resolver::resolve_profile, registry::AgentSpec, sandbox::scope::build_systemd_run_argv,
};

#[derive(Debug)]
pub enum AcpInbound {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Closed,
}

/// Per-request reply channel kept in `AcpChild.pending`. `oneshot`
/// closes when the channel is dropped, which on the read side becomes
/// "child went away mid-request" — surfaced to the caller as an
/// error.
type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

pub struct AcpChild {
    write_tx: mpsc::Sender<Vec<u8>>,
    pending: PendingMap,
    next_id: AtomicU64,
    pub inbound: mpsc::Receiver<AcpInbound>,
    _child: Child,
}

/// Wrapper argv binary name — kept as a const so the test that
/// inspects the unwrapped path can negative-assert against it.
const SYSTEMD_RUN_BIN: &str = "systemd-run";

/// Build the program path + argv vector `AcpChild::spawn` will hand
/// to `Command::new`. When `spec.sandbox_profile` is `Some(...)` and
/// `systemd_run` is `Some(path)` (located via `which`), the result
/// wraps the agent in `systemd-run --user --scope … -- sy agt
/// sandbox-exec -- <agent args>`. Otherwise the result is a direct
/// invocation of `spec.command` with `spec.args`. The function is
/// pure: no I/O beyond what callers pass in, so unit tests can
/// inspect every branch without spawning real processes.
pub(crate) fn build_acp_command(
    spec: &AgentSpec,
    cwd: &Path,
    self_exe: &Path,
    systemd_run: Option<&Path>,
    profile_resolved: Option<&crate::agt::policy::schema::Profile>,
) -> (PathBuf, Vec<String>) {
    if let (Some(profile_name), Some(systemd), Some(profile)) = (
        spec.sandbox_profile.as_deref(),
        systemd_run,
        profile_resolved,
    ) {
        let mut inner_argv = Vec::with_capacity(spec.args.len());
        inner_argv.extend(spec.args.iter().cloned());
        let outer = build_systemd_run_argv(
            profile,
            profile_name,
            Path::new(&spec.command),
            &inner_argv,
            cwd,
            self_exe,
        );
        return (systemd.to_path_buf(), outer);
    }
    (PathBuf::from(&spec.command), spec.args.clone())
}

/// Look up the runtime inputs `build_acp_command` needs:
/// `systemd-run` path (via `which`) and the resolved sandbox
/// `Profile` (via the policy loader). When `spec.sandbox_profile`
/// is `Some` but `systemd-run` is missing, emit a single
/// `tracing::warn!` and fall back to a direct spawn — see the
/// module head comment for the rationale. The profile path is
/// resolved by [`crate::agt::policy::resolver::resolve_policy_root`] so this
/// daemon-side caller and the sandbox-exec re-exec agree on which
/// `policy/profiles/` directory is active.
fn resolve_spawn_command(spec: &AgentSpec, cwd: &Path) -> Result<(PathBuf, Vec<String>)> {
    let self_exe =
        std::env::current_exe().context("locate current sy binary for sandbox re-exec")?;
    let systemd_run = match which::which(SYSTEMD_RUN_BIN) {
        Ok(p) => Some(p),
        Err(_) => {
            if spec.sandbox_profile.is_some() {
                tracing::warn!(
                    agent = spec.name.as_str(),
                    "systemd-run unavailable; agent will run unsandboxed"
                );
            }
            None
        }
    };
    let profile = match (spec.sandbox_profile.as_deref(), systemd_run.as_ref()) {
        (Some(name), Some(_)) => {
            let policy_root =
                crate::agt::policy::resolver::resolve_policy_root(cwd).with_context(|| {
                    format!(
                        "locate `policy/profiles/{name}.toml` for agent {}",
                        spec.name
                    )
                })?;
            let tool_key = Path::new(&spec.command)
                .file_stem()
                .and_then(|s| s.to_str());
            Some(
                resolve_profile(&policy_root, name, tool_key, cwd).with_context(|| {
                    format!("resolve sandbox profile {name} for agent {}", spec.name)
                })?,
            )
        }
        _ => None,
    };
    Ok(build_acp_command(
        spec,
        cwd,
        &self_exe,
        systemd_run.as_deref(),
        profile.as_ref(),
    ))
}

impl AcpChild {
    pub async fn spawn(spec: &AgentSpec, cwd: &Path) -> Result<Self> {
        let (program, argv) = resolve_spawn_command(spec, cwd)?;
        let mut cmd = Command::new(&program);
        cmd.args(&argv);
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        cmd.current_dir(cwd);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn ACP child: {}", spec.command))?;

        let stdin = child.stdin.take().context("child stdin missing")?;
        let stdout = child.stdout.take().context("child stdout missing")?;
        let stderr = child.stderr.take().context("child stderr missing")?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (inbound_tx, inbound_rx) = mpsc::channel::<AcpInbound>(256);
        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(64);

        // Writer task: serializes stdin writes.
        let mut stdin = stdin;
        tokio::spawn(async move {
            while let Some(buf) = write_rx.recv().await {
                if stdin.write_all(&buf).await.is_err() {
                    break;
                }
                if stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        // Reader task: classifies each line.
        let pending_r = pending.clone();
        let inbound_r = inbound_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let v: Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("acp: malformed line: {e}: {trimmed}");
                        continue;
                    }
                };
                let id = v.get("id").cloned();
                let method = v.get("method").and_then(|m| m.as_str()).map(str::to_owned);
                let has_result = v.get("result").is_some();
                let err = v.get("error").cloned();

                match (id, method, has_result || err.is_some()) {
                    (Some(idv), None, true) => {
                        let key = idv.as_u64().unwrap_or(0);
                        let mut p = pending_r.lock().await;
                        if let Some(tx) = p.remove(&key) {
                            let payload = if let Some(e) = err {
                                Err(e.to_string())
                            } else {
                                Ok(v.get("result").cloned().unwrap_or(Value::Null))
                            };
                            let _ = tx.send(payload);
                        }
                    }
                    (Some(idv), Some(m), false) => {
                        let params = v.get("params").cloned().unwrap_or(Value::Null);
                        let _ = inbound_r
                            .send(AcpInbound::Request {
                                id: idv,
                                method: m,
                                params,
                            })
                            .await;
                    }
                    (None, Some(m), _) => {
                        let params = v.get("params").cloned().unwrap_or(Value::Null);
                        let _ = inbound_r
                            .send(AcpInbound::Notification { method: m, params })
                            .await;
                    }
                    _ => {}
                }
            }
            let _ = inbound_r.send(AcpInbound::Closed).await;
        });

        // Stderr passthrough task: log to our stderr prefixed with agent name.
        let label = spec.name.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[{label}] {line}");
            }
        });

        Ok(Self {
            write_tx,
            pending,
            next_id: AtomicU64::new(1),
            inbound: inbound_rx,
            _child: child,
        })
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send_line(&msg).await?;
        match rx.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(anyhow!("acp error: {e}")),
            Err(_) => Err(anyhow!("acp request {method}: receiver dropped")),
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.send_line(&msg).await
    }

    pub async fn respond(&self, id: Value, result: Result<Value>) -> Result<()> {
        let msg = match result {
            Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
            Err(e) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32000, "message": e.to_string()}
            }),
        };
        self.send_line(&msg).await
    }

    async fn send_line(&self, msg: &Value) -> Result<()> {
        let mut buf = serde_json::to_vec(msg)?;
        buf.push(b'\n');
        self.write_tx
            .send(buf)
            .await
            .map_err(|_| anyhow!("acp child stdin closed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agt::policy::schema::Profile;
    use std::collections::BTreeMap;

    const SELF_EXE: &str = "/usr/bin/sy";
    const SYSTEMD_RUN_FAKE: &str = "/usr/bin/systemd-run";
    const AGENT_BIN: &str = "/usr/bin/echo";
    const AGENT_ARG: &str = "hello";
    const PROFILE_NAME: &str = "normal";
    const NORMAL_MEMORY_MB: u64 = 1024;
    const NORMAL_MAX_PIDS: u64 = 256;
    const NORMAL_RUNTIME_SECS: u64 = 60;

    fn echo_spec(profile: Option<&str>) -> AgentSpec {
        AgentSpec {
            name: "acp-test".into(),
            command: AGENT_BIN.into(),
            args: vec![AGENT_ARG.into()],
            env: BTreeMap::new(),
            version_args: vec!["--version".into()],
            sandbox_profile: profile.map(str::to_owned),
        }
    }

    fn normal_profile() -> Profile {
        Profile {
            max_memory_mb: NORMAL_MEMORY_MB,
            max_pids: NORMAL_MAX_PIDS,
            max_runtime_seconds: NORMAL_RUNTIME_SECS,
            ..Profile::default()
        }
    }

    /// Wrapped path: `spec.sandbox_profile = Some("normal")` and
    /// `systemd-run` resolves — the argv must carry the scope flags,
    /// the cgroup caps from the profile, and the `sandbox-exec`
    /// re-exec target with the agent's bin + args trailing.
    #[test]
    fn spawn_wrapped_argv_for_normal_profile() {
        let spec = echo_spec(Some(PROFILE_NAME));
        let profile = normal_profile();
        let (program, argv) = build_acp_command(
            &spec,
            Path::new("/tmp"),
            Path::new(SELF_EXE),
            Some(Path::new(SYSTEMD_RUN_FAKE)),
            Some(&profile),
        );
        assert_eq!(program, PathBuf::from(SYSTEMD_RUN_FAKE));
        for expected in [
            "--user",
            "--scope",
            "--collect",
            "MemoryMax=1024M",
            "TasksMax=256",
            "RuntimeMaxSec=60",
            "sandbox-exec",
            "--profile",
            PROFILE_NAME,
            "--bin",
            AGENT_BIN,
        ] {
            assert!(
                argv.iter().any(|s| s == expected),
                "expected `{expected}` in argv: {argv:?}"
            );
        }
        // Agent's argv trails after the final `--`, in declaration order.
        let trailing = argv.iter().rposition(|s| s == "--").expect("trailing --");
        assert_eq!(argv.get(trailing + 1).map(String::as_str), Some(AGENT_ARG));
    }

    /// Direct path: `spec.sandbox_profile = None` (or `systemd-run`
    /// missing) — we spawn the agent verbatim with no wrapper.
    #[test]
    fn spawn_unwrapped_argv_when_no_profile() {
        let spec = echo_spec(None);
        let (program, argv) = build_acp_command(
            &spec,
            Path::new("/tmp"),
            Path::new(SELF_EXE),
            Some(Path::new(SYSTEMD_RUN_FAKE)),
            None,
        );
        assert_eq!(program, PathBuf::from(AGENT_BIN));
        assert_eq!(argv, vec![AGENT_ARG.to_string()]);
        assert!(
            !argv.iter().any(|s| s.contains(SYSTEMD_RUN_BIN)),
            "no systemd-run leakage in direct-spawn argv: {argv:?}"
        );
    }

    /// Fallback path: profile is `Some` but `systemd-run` is `None`
    /// (the at-runtime `which` lookup failed). We spawn unsandboxed
    /// rather than fail-closed — `tracing::warn!` (not asserted here)
    /// surfaces the downgrade. Module head comment documents the
    /// deliberate divergence from "no silent downgrade".
    #[test]
    fn spawn_falls_back_to_direct_when_systemd_run_missing() {
        let spec = echo_spec(Some(PROFILE_NAME));
        let (program, argv) = build_acp_command(
            &spec,
            Path::new("/tmp"),
            Path::new(SELF_EXE),
            None,
            Some(&normal_profile()),
        );
        assert_eq!(program, PathBuf::from(AGENT_BIN));
        assert_eq!(argv, vec![AGENT_ARG.to_string()]);
    }
}
