//! `sy policy` subcommands — SPEC §4.4 step 2 operator surface.
//!
//! Five subcommands ship in this step:
//!
//! | Subcommand        | Purpose                                                |
//! |-------------------|--------------------------------------------------------|
//! | `show`            | Print the resolved profile + active overlays           |
//! | `lint`            | Static checks for risky settings (`trusted + network`) |
//! | `explain`         | Simulate the resolver for a (tool, argv) pair          |
//! | `trust --confirm` | Write `$XDG_STATE_HOME/sy/trusted.toml` sentinel (TTY)  |
//! | `grant`           | Persist a pre-issued TTL grant to `$XDG_RUNTIME_DIR/sy` |
//!
//! ## `sy policy show --json` schema
//!
//! ```json
//! {
//!   "profile": "<name>",
//!   "fingerprint": "<sha256-hex>",
//!   "read_paths": ["..."],
//!   "write_paths": ["..."],
//!   "exec_allowlist": [{"bin": "...", "argv": ["..."]}],
//!   "net_outbound_allowlist": [{"host": "...", "port": 443}],
//!   "env_passthrough_allowlist": ["PATH", "HOME"],
//!   "max_runtime_seconds": 60,
//!   "max_stdout_bytes": 16777216,
//!   "max_memory_mb": 1024,
//!   "max_pids": 256,
//!   "deny_network": false,
//!   "require_consent": "once_per_session",
//!   "overlay_for_tool": "<tool>|null"
//! }
//! ```
//!
//! ## `sy policy lint --json` schema
//!
//! ```json
//! {
//!   "checks": [{"name":"...", "severity":"fail|warn", "message":"..."}],
//!   "summary": {"fail": N, "warn": N, "pass": N}
//! }
//! ```
//!
//! ## `sy policy explain --json` schema
//!
//! ```json
//! {
//!   "profile": "<name>",
//!   "tool": "/usr/bin/rg",
//!   "argv": ["foo"],
//!   "decision": "Allow|Deny|ConsentRequired",
//!   "reason": "<human-readable, empty for Allow/Deny>"
//! }
//! ```

use std::{
    io::{IsTerminal, Read},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::agt::policy::{
    grant::{write_trust_sentinel, Grant},
    resolver::{Decision, Resolver},
    schema::Profile,
};

/// Exit codes for `sy policy` — CLIG-stable.
const EXIT_USAGE: i32 = 2;
/// Lint policy diff against the strict baseline raised a `fail` row.
const EXIT_LINT_FAIL: i32 = 3;

const DEFAULT_PROFILE: &str = "normal";
const TRUST_CONFIRMATION_STRING: &str = "TRUST THIS PROFILE";

#[derive(Debug, Subcommand)]
pub enum PolicyCmd {
    /// Print the resolved profile + active overlays.
    ///
    /// Examples:
    ///   sy policy show
    ///   sy policy show --profile strict --json
    ///   sy policy show --profile normal --tool rg --json
    Show {
        /// Profile name under `configs/policy/profiles/<name>.toml`.
        #[arg(long, default_value = DEFAULT_PROFILE)]
        profile: String,
        /// Layer the per-tool overlay under `configs/policy/tools/<tool>.toml`.
        #[arg(long)]
        tool: Option<String>,
        /// Machine-readable output (schema documented in `policy/cli.rs`).
        #[arg(long)]
        json: bool,
    },
    /// Static checks for risky policy settings.
    ///
    /// Example: sy policy lint --profile trusted --json
    Lint {
        #[arg(long, default_value = DEFAULT_PROFILE)]
        profile: String,
        #[arg(long)]
        json: bool,
    },
    /// Simulate the resolver for a (tool, argv) pair.
    ///
    /// Example:
    ///   sy policy explain --tool /usr/bin/rg --argv 'foo bar'
    Explain {
        #[arg(long)]
        tool: String,
        /// argv as a single string; split on whitespace.
        #[arg(long)]
        argv: String,
        #[arg(long, default_value = DEFAULT_PROFILE)]
        profile: String,
        #[arg(long)]
        json: bool,
    },
    /// Opt into the `trusted` profile. Requires a TTY on stdin
    /// (or `--yes` plus the confirmation string read from stdin).
    ///
    /// Example: sy policy trust --confirm
    Trust {
        /// Required — refuses to write the sentinel without it.
        #[arg(long)]
        confirm: bool,
        /// Non-interactive override: stdin must contain
        /// `TRUST THIS PROFILE\n` to proceed.
        #[arg(long)]
        yes: bool,
    },
    /// Issue a pre-approved TTL grant for `<tool>` under `<scope>`.
    ///
    /// Example: sy policy grant --tool rg --scope ~/sources/sy --ttl 15m
    Grant {
        #[arg(long)]
        tool: String,
        #[arg(long)]
        scope: PathBuf,
        /// TTL with unit: `200ms | 5s | 1m | 2h`.
        #[arg(long, value_parser = parse_ttl_ms)]
        ttl: u64,
        #[arg(long)]
        json: bool,
    },
}

/// Entry point invoked from `src/main.rs`.
pub fn dispatch(cmd: PolicyCmd) -> Result<()> {
    match cmd {
        PolicyCmd::Show {
            profile,
            tool,
            json,
        } => show(&profile, tool.as_deref(), json),
        PolicyCmd::Lint { profile, json } => lint(&profile, json),
        PolicyCmd::Explain {
            tool,
            argv,
            profile,
            json,
        } => explain(&profile, &tool, &argv, json),
        PolicyCmd::Trust { confirm, yes } => trust(confirm, yes),
        PolicyCmd::Grant {
            tool,
            scope,
            ttl,
            json,
        } => grant(&tool, &scope, Duration::from_millis(ttl), json),
    }
}

/// Locate `<repo>/configs/policy` by walking up from `cwd`. Mirrors
/// `main.rs::find_root` but anchored on the policy subdir so callers
/// can run `sy policy …` from anywhere inside the repo.
fn policy_root() -> Result<PathBuf> {
    let mut cur = std::env::current_dir().context("cwd")?;
    loop {
        let candidate = cur.join("configs").join("policy");
        if candidate.is_dir() {
            return Ok(candidate);
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => {
                return Err(anyhow!(
                    "could not find configs/policy/ — run `sy policy` from inside the sy repo or set SY_ROOT"
                ));
            }
        }
    }
}

fn repo_root_from_policy_root(policy_root: &Path) -> PathBuf {
    // policy_root = <repo>/configs/policy → repo = parent.parent.
    policy_root
        .parent()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn show(profile: &str, tool: Option<&str>, json_out: bool) -> Result<()> {
    let root = policy_root()?;
    let repo = repo_root_from_policy_root(&root);
    let resolver = Resolver::load(&root, profile, tool, &repo)
        .with_context(|| format!("load profile {profile}"))?;
    let raw = load_profile_value(&root, profile)?;
    let value = show_json_value(profile, tool, &resolver, &raw);
    if json_out {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        print_show_human(profile, tool, &resolver);
    }
    Ok(())
}

fn show_json_value(profile: &str, tool: Option<&str>, resolver: &Resolver, raw: &Profile) -> Value {
    json!({
        "profile": profile,
        "fingerprint": resolver.fingerprint(),
        "read_paths": raw.read_paths,
        "write_paths": raw.write_paths,
        "exec_allowlist": raw.exec_allowlist,
        "net_outbound_allowlist": raw.net_outbound_allowlist,
        "env_passthrough_allowlist": raw.env_passthrough_allowlist,
        "max_runtime_seconds": raw.max_runtime_seconds,
        "max_stdout_bytes": raw.max_stdout_bytes,
        "max_memory_mb": raw.max_memory_mb,
        "max_pids": raw.max_pids,
        "deny_network": raw.deny_network,
        "require_consent": raw.require_consent,
        "overlay_for_tool": tool,
    })
}

fn print_show_human(profile: &str, tool: Option<&str>, resolver: &Resolver) {
    println!("profile:     {profile}");
    if let Some(t) = tool {
        println!("overlay:     {t}");
    }
    println!("fingerprint: {}", resolver.fingerprint());
}

fn load_profile_value(policy_root: &Path, profile: &str) -> Result<Profile> {
    let p = policy_root.join("profiles").join(format!("{profile}.toml"));
    let text = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    toml::from_str(&text).with_context(|| format!("parse {}", p.display()))
}

fn lint(profile: &str, json_out: bool) -> Result<()> {
    let root = policy_root()?;
    let raw = load_profile_value(&root, profile)?;
    let report = lint_profile(&raw);
    if json_out {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_lint_human(profile, &report);
    }
    let fails = report
        .get("summary")
        .and_then(|s| s.get("fail"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if fails > 0 {
        std::process::exit(EXIT_LINT_FAIL);
    }
    Ok(())
}

const LINT_RUNTIME_MAX_SECS: u64 = 600;
const LINT_MEMORY_MAX_MB: u64 = 8192;

/// Static checks. Public for tests.
pub(crate) fn lint_profile(profile: &Profile) -> Value {
    let mut checks: Vec<Value> = Vec::new();
    let exec_is_star = profile
        .exec_allowlist
        .iter()
        .any(|entry| entry.bin.to_str() == Some("*"));
    if !profile.deny_network && exec_is_star {
        checks.push(json!({
            "name": "trusted_network_with_star_exec",
            "severity": "fail",
            "message": "deny_network=false combined with exec_allowlist=[\"*\"] yields a trust-everything profile; requires `sy policy trust --confirm`",
        }));
    }
    if profile.max_runtime_seconds > LINT_RUNTIME_MAX_SECS {
        checks.push(json!({
            "name": "runtime_cap_high",
            "severity": "warn",
            "message": format!(
                "max_runtime_seconds={} exceeds the {}s soft cap; long-running calls hold the consent slot",
                profile.max_runtime_seconds, LINT_RUNTIME_MAX_SECS
            ),
        }));
    }
    if profile.max_memory_mb > LINT_MEMORY_MAX_MB {
        checks.push(json!({
            "name": "memory_cap_high",
            "severity": "warn",
            "message": format!(
                "max_memory_mb={} exceeds the {}M soft cap",
                profile.max_memory_mb, LINT_MEMORY_MAX_MB
            ),
        }));
    }
    let consent_never = matches!(
        profile.require_consent,
        crate::agt::policy::schema::ConsentMode::Never
    );
    if consent_never && !profile.exec_allowlist.is_empty() {
        checks.push(json!({
            "name": "consent_never_with_allowlist",
            "severity": "warn",
            "message": "require_consent=never bypasses operator approval even for allowlisted tools",
        }));
    }
    let fail = checks
        .iter()
        .filter(|c| c.get("severity").and_then(Value::as_str) == Some("fail"))
        .count();
    let warn = checks
        .iter()
        .filter(|c| c.get("severity").and_then(Value::as_str) == Some("warn"))
        .count();
    let pass = if checks.is_empty() { 1 } else { 0 };
    json!({
        "checks": checks,
        "summary": {"fail": fail, "warn": warn, "pass": pass},
    })
}

fn print_lint_human(profile: &str, report: &Value) {
    println!("profile: {profile}");
    if let Some(checks) = report.get("checks").and_then(Value::as_array) {
        if checks.is_empty() {
            println!("  ok (no checks tripped)");
            return;
        }
        for check in checks {
            let sev = check.get("severity").and_then(Value::as_str).unwrap_or("?");
            let name = check.get("name").and_then(Value::as_str).unwrap_or("?");
            let msg = check.get("message").and_then(Value::as_str).unwrap_or("");
            println!("  [{sev}] {name}: {msg}");
        }
    }
}

fn explain(profile: &str, tool: &str, argv: &str, json_out: bool) -> Result<()> {
    let root = policy_root()?;
    let repo = repo_root_from_policy_root(&root);
    let resolver = Resolver::load(&root, profile, Some(strip_tool_stem(tool).as_str()), &repo)
        .with_context(|| format!("load profile {profile}"))?;
    let argv_vec: Vec<String> = argv.split_whitespace().map(|s| s.to_string()).collect();
    let decision = resolver.decide(tool, &argv_vec);
    let (label, reason) = match &decision {
        Decision::Allow => ("Allow", String::new()),
        Decision::Deny => (
            "Deny",
            "exec_allowlist is empty (strict default)".to_string(),
        ),
        Decision::ConsentRequired { reason } => ("ConsentRequired", reason.clone()),
    };
    let value = json!({
        "profile": profile,
        "tool": tool,
        "argv": argv_vec,
        "decision": label,
        "reason": reason,
    });
    if json_out {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("decision: {label}");
        if !reason.is_empty() {
            println!("reason:   {reason}");
        }
    }
    Ok(())
}

fn strip_tool_stem(tool: &str) -> String {
    Path::new(tool)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(tool)
        .to_string()
}

fn trust(confirm: bool, yes: bool) -> Result<()> {
    if !confirm {
        eprintln!(
            "error: `sy policy trust` refuses to run without `--confirm`; opting into the trusted profile requires an explicit acknowledgement"
        );
        std::process::exit(EXIT_USAGE);
    }
    let stdin = std::io::stdin();
    let interactive = stdin.is_terminal();
    if !interactive {
        if !yes {
            eprintln!(
                "error: stdin is not a TTY; re-run on a real terminal or pair `--confirm --yes` and pipe `TRUST THIS PROFILE` on stdin"
            );
            std::process::exit(EXIT_USAGE);
        }
        let mut buf = String::new();
        stdin
            .lock()
            .read_to_string(&mut buf)
            .context("read trust confirmation from stdin")?;
        if buf.trim() != TRUST_CONFIRMATION_STRING {
            eprintln!(
                "error: stdin payload must equal {TRUST_CONFIRMATION_STRING:?} (got {:?})",
                buf.trim()
            );
            std::process::exit(EXIT_USAGE);
        }
    }
    let state_dir = state_root()?;
    let pid = std::process::id();
    let path = write_trust_sentinel(&state_dir, pid)?;
    println!("trusted profile unlocked: {}", path.display());
    Ok(())
}

/// Outcome of the TTY pre-flight for `sy approve <token>`. Split from
/// the IPC-dispatching wrapper so unit tests can verify the policy
/// without spawning the daemon — the e2e behaviour is covered by
/// `approve_refuses_outside_tty`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ApprovePreflight {
    /// stdin is a TTY (or `--yes --token-from-stdin` overrode it
    /// with the operator's blessing): proceed to dispatch `agt.approve`.
    Proceed(String),
    /// stdin is not a TTY and the override flags weren't paired:
    /// caller exits with [`EXIT_USAGE`] (CLIG: stdin redirection is
    /// not a substitute for a real terminal).
    RefuseNoTty,
}

/// TTY pre-flight check for `sy approve <token>`. Returns
/// [`ApprovePreflight::Proceed`] with the validated token string
/// when the call is allowed to reach the daemon; otherwise
/// [`ApprovePreflight::RefuseNoTty`] so the caller exits 2.
///
/// The pre-flight isolates side-effect-free policy so the
/// `approve_refuses_outside_tty` unit test can drive it without
/// touching `/dev/tty` or the daemon socket.
pub(crate) fn approve_preflight(
    token: Option<&str>,
    yes: bool,
    token_from_stdin: bool,
    is_tty: bool,
    stdin_payload: Option<&str>,
) -> ApprovePreflight {
    if is_tty {
        // Interactive TTY: the operator typed the UUID themselves.
        // Reject empty arguments so an accidental `sy approve ""`
        // doesn't slip through.
        match token.map(str::trim).filter(|t| !t.is_empty()) {
            Some(t) => ApprovePreflight::Proceed(t.to_string()),
            None => ApprovePreflight::RefuseNoTty,
        }
    } else if yes && token_from_stdin {
        // Non-interactive override: caller piped the token on stdin
        // AND added `--yes` to acknowledge the bypass.
        match stdin_payload.map(str::trim).filter(|t| !t.is_empty()) {
            Some(t) => ApprovePreflight::Proceed(t.to_string()),
            None => ApprovePreflight::RefuseNoTty,
        }
    } else {
        // Non-interactive without override: CLIG default is to refuse.
        ApprovePreflight::RefuseNoTty
    }
}

/// `sy approve <token>` — SPEC §4.4 "Consent UX" step 2 (a). Refuses to
/// run when stdin is not a TTY unless `--yes --token-from-stdin` is
/// paired (matches `sy policy trust --confirm`'s override pattern).
/// On TTY, dispatches `agt.approve {token}` over IPC v1.
pub fn approve(
    token: Option<String>,
    yes: bool,
    token_from_stdin: bool,
    json_out: bool,
) -> Result<()> {
    let stdin = std::io::stdin();
    let is_tty = stdin.is_terminal();
    let mut piped = String::new();
    if !is_tty && token_from_stdin {
        stdin
            .lock()
            .read_to_string(&mut piped)
            .context("read token from stdin")?;
    }
    let preflight = approve_preflight(
        token.as_deref(),
        yes,
        token_from_stdin,
        is_tty,
        if piped.is_empty() { None } else { Some(&piped) },
    );
    let token_str = match preflight {
        ApprovePreflight::Proceed(t) => t,
        ApprovePreflight::RefuseNoTty => {
            eprintln!(
                "error: `sy approve` requires a TTY (CLIG: non-interactive default refuses); re-run on a real terminal or pair `--token-from-stdin --yes` and pipe the UUID on stdin"
            );
            std::process::exit(EXIT_USAGE);
        }
    };
    let uuid = uuid::Uuid::parse_str(&token_str)
        .map_err(|e| anyhow!("token {token_str:?} is not a valid UUID: {e}"))?;
    dispatch_approve(uuid, json_out)
}

/// Sends `agt.approve {token}` over the daemon socket and prints
/// the structured response. Split from [`approve`] so the pre-flight
/// can be unit-tested without a live daemon.
fn dispatch_approve(token: uuid::Uuid, json_out: bool) -> Result<()> {
    use crate::agt::client::Client;
    let mut client = Client::connect()?;
    let response = client.call_raw(
        crate::agt::wire::METHOD_APPROVE,
        json!({ "token": token.to_string() }),
    )?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
        println!("approved: {token}");
    }
    Ok(())
}

fn grant(tool: &str, scope: &Path, ttl: Duration, json_out: bool) -> Result<()> {
    let stdin = std::io::stdin();
    let tty = if stdin.is_terminal() {
        std::env::var("TTY").ok().or_else(|| Some("stdin".into()))
    } else {
        None
    };
    let grant = Grant::new(
        tool.to_string(),
        scope.to_path_buf(),
        ttl,
        std::process::id(),
        tty,
    );
    let dir = runtime_root()?.join("grants");
    let path = grant.persist(&dir)?;
    // Sanity probe: `is_active(now)` must hold immediately after
    // issuance, otherwise we wrote a grant that's dead on arrival.
    // Step 6 reads `is_active` for every tool call; surfacing the
    // check here gives the field its first production caller and
    // makes a clock-skew bug at issuance visible from `--json`.
    let active_now = grant.is_active(std::time::SystemTime::now());
    if json_out {
        let value = json!({
            "grant": grant,
            "path": path,
            "active_now": active_now,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("granted: {} (active_now={active_now})", path.display());
    }
    Ok(())
}

fn runtime_root() -> Result<PathBuf> {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .map_err(|_| anyhow!("XDG_RUNTIME_DIR not set; cannot persist grant"))?;
    Ok(base.join("sy"))
}

fn state_root() -> Result<PathBuf> {
    if let Ok(v) = std::env::var("XDG_STATE_HOME") {
        if !v.is_empty() {
            return Ok(PathBuf::from(v).join("sy"));
        }
    }
    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME not set"))?;
    Ok(PathBuf::from(home).join(".local/state/sy"))
}

fn parse_ttl_ms(raw: &str) -> std::result::Result<u64, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("empty TTL".into());
    }
    let (num_part, unit_factor_ms) = if let Some(rest) = s.strip_suffix("ms") {
        (rest, 1u64)
    } else if let Some(rest) = s.strip_suffix('s') {
        (rest, 1000)
    } else if let Some(rest) = s.strip_suffix('m') {
        (rest, 60 * 1000)
    } else if let Some(rest) = s.strip_suffix('h') {
        (rest, 60 * 60 * 1000)
    } else {
        return Err(format!(
            "ttl {raw:?} needs a unit (`ms`, `s`, `m`, `h`); bare numbers are rejected"
        ));
    };
    let n: u64 = num_part
        .trim()
        .parse()
        .map_err(|e| format!("ttl {raw:?}: bad number {num_part:?}: {e}"))?;
    n.checked_mul(unit_factor_ms)
        .ok_or_else(|| format!("ttl {raw:?} overflows u64 milliseconds"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agt::policy::schema::{ConsentMode, ExecAllow};

    const NORMAL: &str = "normal";

    fn policy_root_for_tests() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("configs")
            .join("policy")
    }

    fn repo_root_for_tests() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
    }

    fn load_normal() -> Resolver {
        Resolver::load(
            &policy_root_for_tests(),
            NORMAL,
            None,
            &repo_root_for_tests(),
        )
        .expect("load normal")
    }

    #[test]
    fn lint_flags_trusted_with_network_on() {
        let profile = Profile {
            exec_allowlist: vec![ExecAllow {
                bin: PathBuf::from("*"),
                argv: vec!["*".to_string()],
            }],
            deny_network: false,
            require_consent: ConsentMode::Never,
            ..Default::default()
        };
        let report = lint_profile(&profile);
        let checks = report
            .get("checks")
            .and_then(Value::as_array)
            .expect("checks array");
        let fails: Vec<&Value> = checks
            .iter()
            .filter(|c| c.get("severity").and_then(Value::as_str) == Some("fail"))
            .collect();
        assert_eq!(
            fails.len(),
            1,
            "expected exactly one fail entry, got {checks:?}"
        );
        assert_eq!(
            fails[0].get("name").and_then(Value::as_str),
            Some("trusted_network_with_star_exec")
        );
    }

    #[test]
    fn explain_normal_rg_returns_allow() {
        let resolver = load_normal();
        let argv = vec!["foo".to_string()];
        let decision = resolver.decide("/usr/bin/rg", &argv);
        assert!(matches!(decision, Decision::Allow));
    }

    #[test]
    fn explain_normal_curl_returns_consent() {
        let resolver = load_normal();
        let argv = vec!["https://x".to_string()];
        let decision = resolver.decide("/usr/bin/curl", &argv);
        assert!(matches!(decision, Decision::ConsentRequired { .. }));
    }

    #[test]
    fn show_normal_emits_stable_json() {
        let resolver = load_normal();
        let raw =
            load_profile_value(&policy_root_for_tests(), NORMAL).expect("load normal profile");
        let value = show_json_value(NORMAL, None, &resolver, &raw);
        // Profile name + fingerprint round-trip.
        assert_eq!(value.get("profile").and_then(Value::as_str), Some(NORMAL));
        assert!(
            value
                .get("fingerprint")
                .and_then(Value::as_str)
                .map(|s| s.len() == 64)
                .unwrap_or(false),
            "fingerprint must be 64 hex chars, got {:?}",
            value.get("fingerprint")
        );
        // Schema shape: every documented top-level key is present.
        for key in [
            "profile",
            "fingerprint",
            "read_paths",
            "write_paths",
            "exec_allowlist",
            "net_outbound_allowlist",
            "env_passthrough_allowlist",
            "max_runtime_seconds",
            "max_stdout_bytes",
            "max_memory_mb",
            "max_pids",
            "deny_network",
            "require_consent",
            "overlay_for_tool",
        ] {
            assert!(value.get(key).is_some(), "missing key {key} in {value:?}");
        }
        // normal profile invariants per configs/policy/profiles/normal.toml.
        assert_eq!(
            value.get("deny_network").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            value.get("require_consent").and_then(Value::as_str),
            Some("once_per_session")
        );
        // overlay_for_tool is null when no `--tool` was passed.
        assert!(value
            .get("overlay_for_tool")
            .map(Value::is_null)
            .unwrap_or(false));
    }

    #[test]
    fn parse_ttl_ms_accepts_units() {
        assert_eq!(parse_ttl_ms("10ms"), Ok(10));
        assert_eq!(parse_ttl_ms("2s"), Ok(2000));
        assert_eq!(parse_ttl_ms("1m"), Ok(60_000));
        assert!(parse_ttl_ms("10").is_err());
    }

    /// SPEC §4.12 CLIG check: `sy approve` must refuse a piped
    /// `/dev/null`-style stdin. The pre-flight covers the
    /// `--token-from-stdin --yes` override too — pass either flag
    /// alone and the call still refuses.
    #[test]
    fn approve_refuses_outside_tty() {
        // Bare non-TTY: refuse.
        let bare = approve_preflight(Some("a-uuid"), false, false, false, None);
        assert_eq!(bare, ApprovePreflight::RefuseNoTty);

        // `--token-from-stdin` alone (no `--yes`): refuse — explicit
        // bypass requires the operator to acknowledge with `--yes`.
        let stdin_only = approve_preflight(None, false, true, false, Some("abc"));
        assert_eq!(stdin_only, ApprovePreflight::RefuseNoTty);

        // `--yes` alone (no `--token-from-stdin`): refuse.
        let yes_only = approve_preflight(Some("abc"), true, false, false, None);
        assert_eq!(yes_only, ApprovePreflight::RefuseNoTty);

        // Override path: both flags set and stdin payload present.
        let allowed = approve_preflight(None, true, true, false, Some("abc\n"));
        assert_eq!(allowed, ApprovePreflight::Proceed("abc".to_string()));

        // TTY: bare token accepted.
        let tty = approve_preflight(Some(" my-token "), false, false, true, None);
        assert_eq!(tty, ApprovePreflight::Proceed("my-token".to_string()));

        // TTY + empty token: still refuse so `sy approve ""` doesn't
        // silently surface a "token not found" from the daemon.
        let tty_empty = approve_preflight(Some(""), false, false, true, None);
        assert_eq!(tty_empty, ApprovePreflight::RefuseNoTty);
    }
}
