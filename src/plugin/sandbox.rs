//! Sandbox envelope for `sy file` plugin processes.
//!
//! Implements the spawn-time enforcement ladder from [plugin SPEC
//! §4.3](../../../specs/research/sy-file-manager-plugins/SPEC.md#43-sandbox-enforcement).
//! Given a parsed [`Manifest`] and a per-plugin working directory the
//! caller has already provisioned, [`build_command`] returns a
//! [`tokio::process::Command`] that — when spawned — runs inside the
//! exact resource / environment / SELinux envelope every later journey
//! beat depends on:
//!
//! 1. `RLIMIT_AS`  ← `manifest.limits.memory_mb * 1 MiB`
//! 2. `RLIMIT_CPU` ← `manifest.limits.cpu_seconds`
//! 3. `RLIMIT_NOFILE` ← `manifest.limits.nofile`
//! 4. `setpriority(PRIO_PROCESS, 0, +5)` (nice +5)
//! 5. close fds except 0/1/2
//! 6. `env_clear()` + `envs(manifest.env)` + PATH carve-out
//! 7. `cwd = $SY_PLUGIN_RUNTIME_DIR/<plugin-id>/` (test override) or
//!    `$XDG_RUNTIME_DIR/sy-plugins/<plugin-id>/`
//! 8. `runcon -t sy_plugin_t -- <argv>` wrap when `/usr/bin/runcon` is
//!    on `PATH` *and* `getenforce` reports `Enforcing`.
//!
//! Each rlimit corresponds 1:1 to a manifest field declared in
//! [`crate::plugin::manifest::Limits`]; no silent defaults are applied
//! at the sandbox layer (the manifest parser supplies the canonical
//! defaults at parse time so the rlimit values are always explicit by
//! the time they reach this module).
//!
//! The PATH carve-out is the one documented deviation from a strict
//! allowlist: `/bin/sh` and other interpreters resolve dynamic linker
//! paths through `PATH`, so without it `execve(/bin/sh, ...)` from a
//! wrapped `runcon -- /bin/sh -c '...'` would `ENOENT`. Plugin
//! manifests are free to override `PATH` via `[env]` (the canary
//! `sy-plugin-md` pins it to `/usr/bin`). When neither the manifest
//! nor the host environment supplies `PATH`, the sandbox falls back to
//! `/usr/bin:/bin` so the child can still resolve coreutils.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use nix::sys::resource::{setrlimit, Resource};
use tokio::process::Command;

use crate::plugin::manifest::Manifest;

/// SELinux type the plugin domain transitions to under the
/// productivised `sy_plugin.te` module (SPEC §4.3). Used as `runcon
/// -t <label>` rather than the full `system_u:system_r:sy_plugin_t:s0`
/// triple so callers without the `system_r` role still benefit from
/// the type-only transition.
pub const SY_PLUGIN_SELINUX_TYPE: &str = "sy_plugin_t";

/// Env-var name the host reads to override the per-plugin cwd root.
/// Tests point this at a `tempfile::TempDir`; production leaves it
/// unset and the sandbox picks `$XDG_RUNTIME_DIR/sy-plugins/`.
pub const RUNTIME_DIR_ENV: &str = "SY_PLUGIN_RUNTIME_DIR";

/// Fallback runtime-dir root when neither `SY_PLUGIN_RUNTIME_DIR` nor
/// `XDG_RUNTIME_DIR` is set. `/tmp` is the SPEC-permitted last resort
/// (SPEC §4.3 step 7 "tmpfs slot") so the sandbox layer never has to
/// `mkdir -p $HOME/...` on a malformed host.
const FALLBACK_RUNTIME_ROOT: &str = "/tmp";

/// Resolve the per-plugin cwd slot the supervisor (Step 4) creates
/// before [`build_command`] is called.
///
/// Precedence (highest first):
/// 1. `SY_PLUGIN_RUNTIME_DIR` (the [`RUNTIME_DIR_ENV`] override;
///    tests point this at a `tempfile::TempDir`).
/// 2. `$XDG_RUNTIME_DIR/sy-plugins/<plugin_id>/` (production path
///    per SPEC §4.3 step 7).
/// 3. `/tmp/sy-plugins/<plugin_id>/` (degraded fallback — emits a
///    warning so the operator can fix the host).
pub fn runtime_dir_for(plugin_id: &str) -> PathBuf {
    if let Ok(root) = std::env::var(RUNTIME_DIR_ENV) {
        return PathBuf::from(root).join(plugin_id);
    }
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(xdg).join("sy-plugins").join(plugin_id);
    }
    tracing::warn!(
        target = "sy::plugin::sandbox",
        plugin_id,
        "no XDG_RUNTIME_DIR; falling back to /tmp/sy-plugins/<id> (SPEC §4.3 step 7 degraded)"
    );
    PathBuf::from(FALLBACK_RUNTIME_ROOT)
        .join("sy-plugins")
        .join(plugin_id)
}

/// Fallback `PATH` when the manifest doesn't set one and the host
/// process has no `PATH` either (rare; `cargo test` strips it under
/// some sandboxes). Keeps `/bin/sh` resolvable inside the child.
const FALLBACK_PATH: &str = "/usr/bin:/bin";

/// Build a [`tokio::process::Command`] wrapped in the SPEC §4.3
/// sandbox envelope.
///
/// The returned command is *not* yet spawned; callers may attach
/// stdin/stdout pipes (Step 4 supervisor) or override argv (tests
/// that probe the envelope via `/bin/sh -c '…'`) before calling
/// [`tokio::process::Command::spawn`].
///
/// `workdir` is the per-plugin runtime slot the host has already
/// created. The sandbox does not `mkdir` here — Step 4 owns slot
/// provisioning so this layer stays pure / synchronous.
pub fn build_command(manifest: &Manifest, workdir: &Path) -> Result<Command> {
    if !workdir.is_absolute() {
        return Err(anyhow!(
            "sandbox: workdir must be absolute, got {:?}",
            workdir
        ));
    }
    let argv = build_argv(manifest)?;
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| anyhow!("sandbox: empty argv (manifest.binary.exec must be non-empty)"))?;

    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.current_dir(workdir);
    apply_env(&mut cmd, manifest);
    // Stdin/stdout are piped so the Step 4 supervisor can drive the
    // JSON-RPC framed transport; stderr stays inherited so tracing's
    // `plugin.<id>` span sees the plugin's log lines.
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::inherit());

    let limits = manifest.limits.clone();
    // SAFETY: `pre_exec` runs after `fork(2)` and before `execve(2)` in
    // the child. We only call APIs documented as async-signal-safe:
    //   * `nix::sys::resource::setrlimit` is a thin libc setrlimit wrapper;
    //   * `libc::setpriority` is POSIX-async-signal-safe;
    //   * `libc::close` is async-signal-safe.
    // No heap allocations occur on the success path (the closure
    // captures `limits` by move; the fd-close loop reads `/proc/self/fd`
    // *before* the closure runs, in the parent).
    let max_fd = scan_open_fds().unwrap_or(DEFAULT_MAX_FD);
    unsafe {
        cmd.pre_exec(move || {
            apply_rlimits(&limits).map_err(std::io::Error::other)?;
            apply_nice().map_err(std::io::Error::other)?;
            close_fds_above_stderr(max_fd);
            Ok(())
        });
    }
    Ok(cmd)
}

/// Read `/proc/self/fd` in the parent so the child's `pre_exec` knows
/// which fds to close without doing a heap-allocating directory walk
/// inside the signal-handler context. Falls back to a conservative
/// ceiling on error.
fn scan_open_fds() -> Result<i32> {
    let entries = std::fs::read_dir("/proc/self/fd").context("sandbox: read /proc/self/fd")?;
    let mut max = 2;
    for ent in entries.flatten() {
        if let Some(name) = ent.file_name().to_str() {
            if let Ok(fd) = name.parse::<i32>() {
                if fd > max {
                    max = fd;
                }
            }
        }
    }
    Ok(max)
}

/// Conservative ceiling when `/proc/self/fd` is unreadable (e.g. a
/// stripped-down test sandbox). 1024 is the historical default soft
/// NOFILE on Linux; closing up to that range is cheap and matches the
/// SPEC §4.3 step 5 "close fds except (0, 1, 2)" intent.
const DEFAULT_MAX_FD: i32 = 1024;

fn apply_rlimits(l: &crate::plugin::manifest::Limits) -> Result<()> {
    // RLIMIT_AS in bytes per the SPEC §4.3 step 1 formula.
    let memory_bytes: u64 = u64::from(l.memory_mb)
        .checked_mul(1024 * 1024)
        .ok_or_else(|| anyhow!("sandbox: memory_mb overflow"))?;
    setrlimit(Resource::RLIMIT_AS, memory_bytes, memory_bytes)
        .context("sandbox: setrlimit RLIMIT_AS")?;
    setrlimit(
        Resource::RLIMIT_CPU,
        u64::from(l.cpu_seconds),
        u64::from(l.cpu_seconds),
    )
    .context("sandbox: setrlimit RLIMIT_CPU")?;
    setrlimit(
        Resource::RLIMIT_NOFILE,
        u64::from(l.nofile),
        u64::from(l.nofile),
    )
    .context("sandbox: setrlimit RLIMIT_NOFILE")?;
    Ok(())
}

/// Nice the child to +5 (SPEC §4.3 step 4). `setpriority(2)` with
/// `who = 0` targets the current process.
fn apply_nice() -> Result<()> {
    // SAFETY: `setpriority(2)` is async-signal-safe and takes plain
    // integers — no shared state, no heap traffic. PRIO_PROCESS=0 +
    // who=0 means "the current pid" (the freshly-forked child).
    let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 5) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error();
        // EACCES on hardened hosts (PAM-limits forbidding renice) is
        // expected; we surface it so the supervisor can decide, but
        // the SPEC §4.3 step 4 nice is best-effort, not a hard fail.
        return Err(anyhow!("sandbox: setpriority(+5) failed: {errno}"));
    }
    Ok(())
}

/// Close every fd in `(stderr, max_fd]`. Plain `libc::close` so the
/// closure stays async-signal-safe; ignores errors because the fd may
/// simply not be open.
fn close_fds_above_stderr(max_fd: i32) {
    let mut fd = 3;
    while fd <= max_fd {
        // SAFETY: `close(2)` is async-signal-safe; rc is discarded
        // because EBADF / EINTR on an already-closed fd is harmless.
        unsafe {
            libc::close(fd);
        }
        fd += 1;
    }
}

/// Build the argv for the wrapped child.
///
/// Adds the `runcon -t sy_plugin_t --` prefix when SELinux is enforcing
/// AND `runcon` is reachable on the host `PATH`; otherwise logs the
/// SPEC §4.3 fallback warning and returns the bare argv unchanged.
fn build_argv(manifest: &Manifest) -> Result<Vec<String>> {
    let bin = manifest.plugin.binary.exec.clone();
    if bin.is_empty() {
        return Err(anyhow!("sandbox: manifest.plugin.binary.exec is empty"));
    }
    let mut argv = vec![bin];
    if let Some(runcon) = locate_runcon_when_enforcing() {
        let mut wrapped = vec![
            runcon.to_string_lossy().into_owned(),
            "-t".to_string(),
            SY_PLUGIN_SELINUX_TYPE.to_string(),
            "--".to_string(),
        ];
        wrapped.append(&mut argv);
        return Ok(wrapped);
    }
    Ok(argv)
}

/// `Some(/usr/bin/runcon)` iff `runcon` is on the host `PATH` AND
/// `getenforce` reports `Enforcing` AND the host's loaded SELinux
/// policy actually defines the `sy_plugin_t` type. Anything else (no
/// runcon binary, no `getenforce`, `Permissive`, `Disabled`, or the
/// policy module not installed) returns `None` after emitting the
/// SPEC §4.3 fallback warning the journey's "SELinux denial on plugin
/// spawn" edge case mandates (J3 §"SELinux denial on plugin spawn"
/// — `runcon` returns ENOENT or `setexeccon` returns EPERM; we
/// degrade to spawning without a context).
fn locate_runcon_when_enforcing() -> Option<PathBuf> {
    let Ok(runcon) = which::which("runcon") else {
        tracing::warn!(
            target = "sy::plugin::sandbox",
            "runcon not on PATH; spawning plugin without SELinux transition (SPEC §4.3 fallback)"
        );
        return None;
    };
    match selinux_mode() {
        Some(SelinuxMode::Enforcing) => {}
        other => {
            tracing::warn!(
                target = "sy::plugin::sandbox",
                mode = ?other,
                "selinux not enforcing; spawning plugin without runcon wrap (SPEC §4.3 fallback)"
            );
            return None;
        }
    }
    if !plugin_label_known(&runcon) {
        tracing::warn!(
            target = "sy::plugin::sandbox",
            label = SY_PLUGIN_SELINUX_TYPE,
            "selinux module sy_plugin missing; spawning plugin without runcon wrap (SPEC §4.3 fallback)"
        );
        return None;
    }
    Some(runcon)
}

/// Probe whether the loaded SELinux policy defines `sy_plugin_t` by
/// running `runcon -t sy_plugin_t -- /bin/true`. A non-zero exit (or
/// the spawn itself failing because the type is unknown) signals that
/// the `sy_plugin` policy module isn't installed on this host — the
/// J3 fallback case. We use `/bin/true` because it's a coreutils
/// binary that always succeeds when the transition *does* work, so a
/// failure is unambiguously a label-resolution problem.
fn plugin_label_known(runcon: &Path) -> bool {
    std::process::Command::new(runcon)
        .args(["-t", SY_PLUGIN_SELINUX_TYPE, "--", "/bin/true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// SELinux enforcement modes returned by `/usr/sbin/getenforce` /
/// `/usr/bin/getenforce`. Anything other than `Enforcing` (including
/// the `getenforce` binary being absent) drops the runcon wrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelinuxMode {
    Enforcing,
    Permissive,
    Disabled,
}

fn selinux_mode() -> Option<SelinuxMode> {
    let out = std::process::Command::new("getenforce").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    match s.trim() {
        "Enforcing" => Some(SelinuxMode::Enforcing),
        "Permissive" => Some(SelinuxMode::Permissive),
        "Disabled" => Some(SelinuxMode::Disabled),
        _ => None,
    }
}

/// Wipe inherited env then re-populate from the manifest's `[env]`
/// allowlist (SPEC §4.3 step 6). The PATH carve-out lets interpreters
/// like `/bin/sh` resolve their linker paths inside the child; see the
/// module-level rationale.
fn apply_env(cmd: &mut Command, manifest: &Manifest) {
    cmd.env_clear();
    for (k, v) in &manifest.env {
        cmd.env(k, v);
    }
    if !manifest.env.contains_key("PATH") {
        let path = std::env::var("PATH").unwrap_or_else(|_| FALLBACK_PATH.to_string());
        cmd.env("PATH", path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::manifest::{load, Manifest};
    use std::collections::BTreeMap;
    use std::process::Stdio as StdStdio;
    use tempfile::TempDir;

    /// Canonical manifest fixture for the sandbox tests. Binary path
    /// is `/bin/sh` so each test can append `["-c", "<probe>"]` to
    /// read effective rlimits / environ back via the spawned shell.
    /// The `[limits]` block uses values that are (a) distinct from
    /// each other so a swapped argument is easy to spot in failure
    /// output, and (b) large enough that the shell itself can boot
    /// without tripping its own startup cost.
    const SH_PROBE_MANIFEST: &str = r#"
api = "1"

[plugin]
id = "sy-plugin-sandbox-probe"
name = "Sandbox Probe"
version = "0.0.0"
api_min = "1"
api_max = "1"

[plugin.binary]
exec = "/bin/sh"

[[capability]]
kind = "previewer"
url = "*.probe"

[needs]
fs_read = []
fs_write = []
preview = []
knowledge = []
network = []
exec = []

[limits]
memory_mb = 256
cpu_seconds = 17
nofile = 123
spawn_timeout_ms = 500
shutdown_timeout_ms = 1000

[env]
SY_PROBE_SENTINEL = "ok"
PATH = "/usr/bin:/bin"
"#;

    fn probe_manifest() -> Manifest {
        load(SH_PROBE_MANIFEST).expect("sandbox probe manifest parses")
    }

    fn workdir() -> TempDir {
        tempfile::tempdir().expect("tempdir for plugin cwd")
    }

    /// Spawn the configured Command (already wrapped in the SPEC §4.3
    /// envelope) with `sh -c <probe>` and return captured stdout.
    fn run_probe(manifest: &Manifest, cwd: &Path, probe: &str) -> String {
        // `build_command` returns a tokio Command. For these
        // probe-shaped synchronous tests we convert it back to its
        // std::process::Command counterpart by re-building an
        // equivalent invocation; that's how `tokio::process::Command::
        // as_std`-style sync access is done across the tokio 1.x line.
        let cmd = build_command(manifest, cwd).expect("build sandbox cmd");
        // Use blocking spawn via the std-flavoured equivalent: tokio's
        // Command derefs to std::process::Command for configuration
        // but spawn is async. We block on it via a current-thread
        // runtime so we can keep the test body synchronous.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .expect("rt");
        rt.block_on(async move {
            // Re-attach the probe via `.arg("-c").arg(probe)`. The
            // sandbox wrap puts argv at positions `[runcon -t … --]
            // /bin/sh`; appending `-c <probe>` lands as the shell's
            // own args, which is exactly how runcon's `--` semantics
            // work.
            let mut cmd = cmd;
            cmd.arg("-c").arg(probe);
            cmd.stdin(StdStdio::null());
            cmd.stdout(StdStdio::piped());
            cmd.stderr(StdStdio::piped());
            let out = cmd.output().await.expect("probe spawn");
            assert!(
                out.status.success(),
                "probe exited {:?}: stderr={}",
                out.status,
                String::from_utf8_lossy(&out.stderr),
            );
            String::from_utf8(out.stdout).expect("utf-8 probe stdout")
        })
    }

    /// SPEC §4.3 step 1 — `RLIMIT_AS` reflects `memory_mb * 1024 * 1024`.
    /// `ulimit -v` prints the soft limit in *kilobytes*, so a manifest
    /// `memory_mb = 256` must surface as `256 * 1024` kB.
    #[test]
    fn sets_rlimit_as_from_manifest() {
        let m = probe_manifest();
        let tmp = workdir();
        let out = run_probe(&m, tmp.path(), "ulimit -v");
        let kb: u64 = out.trim().parse().expect("kb integer");
        assert_eq!(
            kb,
            u64::from(m.limits.memory_mb) * 1024,
            "RLIMIT_AS (kB) must match memory_mb*1024"
        );
    }

    /// SPEC §4.3 step 2 — `RLIMIT_CPU` reflects `cpu_seconds`.
    #[test]
    fn sets_cpu_seconds() {
        let m = probe_manifest();
        let tmp = workdir();
        let out = run_probe(&m, tmp.path(), "ulimit -t");
        let secs: u64 = out.trim().parse().expect("seconds integer");
        assert_eq!(secs, u64::from(m.limits.cpu_seconds));
    }

    /// SPEC §4.3 step 3 — `RLIMIT_NOFILE` reflects `nofile`.
    #[test]
    fn sets_nofile() {
        let m = probe_manifest();
        let tmp = workdir();
        let out = run_probe(&m, tmp.path(), "ulimit -n");
        let n: u64 = out.trim().parse().expect("nofile integer");
        assert_eq!(n, u64::from(m.limits.nofile));
    }

    /// SPEC §4.3 step 6 — environ scrubbed to the manifest allowlist
    /// (plus the documented PATH carve-out). No host secrets survive.
    #[test]
    fn scrubs_environ_keeps_manifest_env() {
        // Plant a sentinel in the host env that must NOT appear in the
        // child's environ.
        // SAFETY: `set_var` is safe in single-threaded test context.
        unsafe {
            std::env::set_var("SY_HOST_SECRET", "leaked");
        }
        let m = probe_manifest();
        let tmp = workdir();
        let out = run_probe(&m, tmp.path(), "printenv | sort");
        // `printenv` prints `KEY=VALUE` lines; build a set of keys.
        let keys: std::collections::BTreeSet<&str> = out
            .lines()
            .filter_map(|l| l.split_once('=').map(|(k, _)| k))
            .collect();
        assert!(
            keys.contains("SY_PROBE_SENTINEL"),
            "manifest env must survive scrub, got keys={keys:?}"
        );
        assert!(keys.contains("PATH"), "PATH carve-out missing");
        assert!(
            !keys.contains("SY_HOST_SECRET"),
            "host secret must not survive scrub, got keys={keys:?}"
        );
    }

    /// SPEC §4.3 step 7 — cwd is the per-plugin runtime slot. We point
    /// it at a `tempfile::TempDir` (the production fallback is
    /// `$XDG_RUNTIME_DIR/sy-plugins/<plugin-id>/`).
    #[test]
    fn cwd_is_xdg_runtime_subdir() {
        let m = probe_manifest();
        let tmp = workdir();
        let out = run_probe(&m, tmp.path(), "pwd");
        let pwd = out.trim();
        // `/tmp` on Fedora is sometimes a symlink to `/var/tmp`; resolve
        // both sides via canonicalize so the equality test stays stable.
        let want = std::fs::canonicalize(tmp.path()).expect("canonical workdir");
        let got = std::fs::canonicalize(Path::new(pwd)).expect("canonical pwd");
        assert_eq!(got, want, "cwd must be the per-plugin runtime slot");
    }

    /// SPEC §4.3 step 8 — when `runcon` is on PATH *and* SELinux is
    /// `Enforcing`, the built command wraps argv with
    /// `runcon -t sy_plugin_t -- <orig>`. Otherwise (no runcon, or
    /// Permissive/Disabled), the original argv is preserved and a
    /// fallback warning is logged.
    #[test]
    fn runcon_used_when_label_present() {
        let m = probe_manifest();
        let tmp = workdir();
        let cmd = build_command(&m, tmp.path()).expect("build cmd");
        // tokio's Command exposes `as_std()` for inspection-only.
        let std_cmd = cmd.as_std();
        let program = std_cmd.get_program().to_string_lossy().into_owned();
        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        let runcon_path = which::which("runcon").ok();
        let enforcing = selinux_mode() == Some(SelinuxMode::Enforcing);
        let label_known = runcon_path.as_deref().is_some_and(plugin_label_known);
        if runcon_path.is_some() && enforcing && label_known {
            assert!(
                program.ends_with("runcon"),
                "program must be runcon when enforcing + label known, got {program}"
            );
            assert_eq!(
                args[..3],
                [
                    "-t".to_string(),
                    SY_PLUGIN_SELINUX_TYPE.to_string(),
                    "--".to_string()
                ],
                "argv must start with -t sy_plugin_t -- ; got {args:?}"
            );
            assert_eq!(args[3], "/bin/sh", "original argv must follow the --");
        } else {
            // SPEC §4.3 fallback path: no runcon, or SELinux not
            // enforcing, or `sy_plugin_t` not in the loaded policy.
            // Each branch is a journey-J3 "SELinux denial on plugin
            // spawn" tributary; all three degrade to argv unchanged
            // plus a tracing warning.
            assert_eq!(
                program, "/bin/sh",
                "no runcon wrap when SELinux denial path active"
            );
        }
    }

    /// Workdir must be absolute — the sandbox refuses relative paths
    /// because the cwd ladder would otherwise depend on the host
    /// process's own cwd, which is not under sandbox control.
    #[test]
    fn rejects_relative_workdir() {
        let m = probe_manifest();
        let rel = Path::new("relative/path");
        let err = build_command(&m, rel).expect_err("relative workdir rejected");
        assert!(format!("{err:#}").contains("absolute"), "got: {err}");
    }

    /// [`runtime_dir_for`] precedence: env override beats
    /// `XDG_RUNTIME_DIR` which beats the `/tmp` fallback. The test
    /// sets + unsets vars under the single-threaded `cargo test`
    /// harness; concurrent var mutation would race, but the sandbox
    /// module's tests share no env-sensitive state.
    #[test]
    fn runtime_dir_precedence_env_xdg_fallback() {
        // SAFETY: single-threaded test context; no other thread reads
        // these vars during this body.
        unsafe {
            std::env::set_var(RUNTIME_DIR_ENV, "/tmp/override-root");
            std::env::set_var("XDG_RUNTIME_DIR", "/run/user/9999");
        }
        let p = runtime_dir_for("sy-plugin-md");
        assert_eq!(p, Path::new("/tmp/override-root/sy-plugin-md"));

        unsafe {
            std::env::remove_var(RUNTIME_DIR_ENV);
        }
        let p = runtime_dir_for("sy-plugin-md");
        assert_eq!(p, Path::new("/run/user/9999/sy-plugins/sy-plugin-md"));

        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        let p = runtime_dir_for("sy-plugin-md");
        // Fallback path under FALLBACK_RUNTIME_ROOT.
        assert_eq!(p, Path::new("/tmp/sy-plugins/sy-plugin-md"));
    }

    /// Empty manifest binary path is a misconfiguration the sandbox
    /// catches before `execve` rather than letting it surface as an
    /// opaque ENOENT later.
    #[test]
    fn rejects_empty_exec() {
        let mut m = probe_manifest();
        m.plugin.binary.exec = String::new();
        // env keys borrow lifetime — clone to drop the ref.
        let _ = BTreeMap::<String, String>::new();
        let tmp = workdir();
        let err = build_command(&m, tmp.path()).expect_err("empty exec rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("exec") || msg.contains("empty"), "got: {msg}");
    }
}
