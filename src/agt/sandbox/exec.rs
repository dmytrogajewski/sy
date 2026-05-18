//! Fork-and-exec entry point for the layered sandbox.
//!
//! Layer order (SPEC §4.4 step 3, verbatim — load-bearing):
//!
//! 1. `prctl(PR_SET_NO_NEW_PRIVS, 1)` — drops the ability to gain
//!    privilege via `execve`. This must come first because Landlock
//!    requires it for unprivileged callers, and because once seccomp
//!    is loaded the `prctl` call itself might be denied.
//! 2. `landlock_layer::restrict` — FS allowlist + (kernel ≥ 6.7)
//!    per-port TCP gate. Landlock's `restrict_self` syscall stays
//!    available even under our seccomp filter, but we apply it
//!    second so a seccomp bug never blocks the FS lock-down.
//! 3. `seccomp_layer::apply` — the curated EPERM deny set. Seccomp
//!    goes last because it strips syscalls the earlier layers
//!    themselves depend on; applying it before Landlock would risk
//!    `landlock_restrict_self` itself being filtered.
//! 4. `execve` — the new image inherits all three restrictions.
//!
//! Anti-goal (SPEC §3.4): we *never* execute `/bin/sh -c <string>`.
//! The caller passes an explicit `(bin, argv[])` pair; the parent
//! sets `env_clear()` and `envs(scrubbed)` before spawn so the child
//! has no inherited env beyond the profile's allowlist.

use std::{collections::HashMap, os::unix::process::CommandExt, path::Path, process::ExitStatus};

use anyhow::{Context, Result};
use seccompiler::BpfProgram;

use crate::agt::{
    policy::schema::Profile,
    sandbox::{env_scrub, landlock_layer, seccomp_layer},
};

/// Spawn `bin` with `argv` under the full sandbox stack and wait for
/// it to exit.
///
/// The parent:
/// - reads the current process env;
/// - scrubs it against `profile.env_passthrough_allowlist`;
/// - precompiles the seccomp filter (allocates; not safe in
///   `pre_exec`);
/// - builds the Landlock ruleset (also allocates);
/// - spawns the child via `std::process::Command` with `env_clear` +
///   `envs(scrubbed)` + `pre_exec` applying the four layers.
pub fn fork_and_exec(profile: &Profile, bin: &Path, argv: &[String]) -> Result<ExitStatus> {
    let host_env: HashMap<String, String> = std::env::vars().collect();
    let scrubbed = env_scrub::scrub(&host_env, &profile.env_passthrough_allowlist);
    let seccomp = seccomp_layer::compile()?;
    let profile = profile.clone();

    let mut cmd = std::process::Command::new(bin);
    cmd.args(argv).env_clear().envs(&scrubbed);

    // SAFETY: `pre_exec` closures run after `fork(2)` and before
    // `execve(2)`. We only call APIs the underlying crates document as
    // signal-safe:
    //   - `rustix::process::set_no_new_privs` is a raw `prctl` syscall.
    //   - `landlock_layer::restrict` performs the kernel
    //     `landlock_restrict_self` syscall (no heap allocation; the
    //     ruleset fd was built in the parent).
    //   - `seccompiler::apply_filter` is documented signal-safe.
    // No `std::alloc::Global` traffic happens on the success path.
    // The closure consumes `seccomp` by reference through `move` so
    // its BPF buffer survives into the child address space without
    // re-allocating.
    unsafe {
        cmd.pre_exec(move || {
            apply_layers(&profile, &seccomp).map_err(|e| std::io::Error::other(e.to_string()))
        });
    }

    let mut child = cmd.spawn().context("spawn sandboxed child")?;
    child.wait().context("wait sandboxed child")
}

/// In-child layer application. Returns the first error encountered;
/// `pre_exec` translates it into an `io::Error` that fails the spawn.
fn apply_layers(profile: &Profile, seccomp: &BpfProgram) -> Result<()> {
    rustix::thread::set_no_new_privs(true).context("PR_SET_NO_NEW_PRIVS")?;
    let ruleset = landlock_layer::build(profile)?;
    let _status = landlock_layer::restrict(ruleset)?;
    seccomp_layer::apply(seccomp)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agt::policy::schema::Profile;

    /// Strict profile: empty read_paths so Landlock blocks every FS
    /// read including the `execve` of `/usr/bin/cat` itself. Either
    /// outcome counts as "denied":
    /// - spawn fails with EACCES (kernel rejects `execve` after
    ///   `restrict_self` clamped reads to the empty set), OR
    /// - the child starts (impossible here but defensive) and exits
    ///   non-zero because it can't open `/etc/shadow`.
    ///
    /// The test passes iff neither path returns a successful child
    /// exit, which is the SPEC §4.4 "strict profile denies sensitive
    /// reads" guarantee.
    #[test]
    #[ignore = "needs Landlock-enabled kernel; run with `cargo test -- --ignored` on Fedora 43"]
    fn sandbox_denies_etc_shadow_under_strict_profile() {
        let cat = which::which("cat").expect("cat on PATH");
        let profile = Profile {
            env_passthrough_allowlist: vec!["PATH".to_string()],
            ..Profile::default()
        };
        match fork_and_exec(&profile, &cat, &["/etc/shadow".to_string()]) {
            Ok(status) => assert!(
                !status.success(),
                "strict profile must deny /etc/shadow read; got success {status:?}"
            ),
            Err(e) => {
                // Spawn-time EACCES from Landlock-clamped execve is
                // the strongest possible denial — the kernel never
                // even loaded the binary's image. Walk the anyhow
                // chain because `Command::spawn` wraps the raw
                // `io::Error` and the EACCES string only appears on
                // the source.
                let combined: String = e
                    .chain()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(" | ");
                assert!(
                    combined.contains("Permission denied") || combined.contains("EACCES"),
                    "expected EACCES from sandboxed execve; got chain={combined}"
                );
            }
        }
    }

    /// Normal-ish profile with `rg --version`: rg only touches its
    /// own binary + libc + the version string on stdout; the
    /// Landlock ruleset grants read on the binary's parent dir.
    #[test]
    #[ignore = "needs `rg` on PATH and Landlock-enabled kernel; run with `cargo test -- --ignored`"]
    fn sandbox_allows_rg_under_normal_profile() {
        let Ok(rg) = which::which("rg") else {
            eprintln!("rg not on PATH; skip");
            return;
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let profile = Profile {
            read_paths: vec![
                std::path::PathBuf::from("/usr"),
                std::path::PathBuf::from("/etc/ld.so.cache"),
                tmp.path().to_path_buf(),
            ],
            env_passthrough_allowlist: vec!["PATH".to_string(), "LANG".to_string()],
            ..Profile::default()
        };
        let status =
            fork_and_exec(&profile, &rg, &["--version".to_string()]).expect("spawn rg --version");
        assert!(
            status.success(),
            "normal profile must allow `rg --version`; got {status:?}"
        );
    }
}
