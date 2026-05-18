//! Landlock LSM layer — SPEC §4.4 step 3 layer 2.
//!
//! Builds a `landlock::Ruleset` from the profile's `read_paths`,
//! `write_paths`, and `net_outbound_allowlist`. Network rules require
//! ABI v4 (Linux 6.7+); on older kernels we refuse to install a
//! ruleset that demands network restrictions rather than silently
//! no-op (SPEC §6 risk row 4 mitigation).
//!
//! The build function produces a ready-to-restrict `RulesetCreated`
//! handle but does **not** call `restrict_self()` on the running
//! process — that's the caller's job inside the `pre_exec` closure,
//! after `fork(2)` and after PR_SET_NO_NEW_PRIVS. The split lets
//! unit tests exercise the build path without sandboxing the test
//! harness.

use anyhow::{anyhow, Context, Result};
use landlock::{
    AccessFs, AccessNet, CompatLevel, Compatible, NetPort, PathBeneath, PathFd, Ruleset,
    RulesetAttr, RulesetCreated, RulesetCreatedAttr, RulesetStatus, ABI,
};

use crate::agt::policy::schema::Profile;

/// Landlock ABI v4 — Linux 6.7+; adds `LANDLOCK_ACCESS_NET_CONNECT_TCP`.
/// We pin to v4 for the FS-and-net feature set the SPEC asks for; the
/// crate transparently downgrades on older kernels under
/// `CompatLevel::BestEffort`. For the network gate we flip to
/// `HardRequirement` so an old kernel hard-errors instead of silently
/// dropping the TCP rules.
const TARGET_ABI: ABI = ABI::V4;

/// Build a Landlock ruleset for `profile`, opening the FS rule fds
/// in the parent so the child's `pre_exec` only needs to call
/// `restrict_self()`. Returns the created ruleset handle.
///
/// Errors:
/// - The profile asks for `net_outbound_allowlist` entries but the
///   running kernel is older than 6.7 (ABI < v4). We refuse rather
///   than silently dropping network rules.
/// - A `read_paths` / `write_paths` entry can't be opened (typically
///   ENOENT).
pub fn build(profile: &Profile) -> Result<RulesetCreated> {
    let fs_read = AccessFs::from_read(TARGET_ABI);
    let fs_write = AccessFs::from_write(TARGET_ABI);
    let needs_net = !profile.net_outbound_allowlist.is_empty();

    let mut builder = Ruleset::default()
        .handle_access(fs_read)
        .context("handle AccessFs read")?;
    {
        let builder_ref = &mut builder;
        builder_ref
            .handle_access(fs_write)
            .context("handle AccessFs write")?;
        if needs_net {
            // HardRequirement: the kernel MUST support ConnectTcp or
            // we refuse to install the ruleset. SPEC §6 risk row 4.
            builder_ref.set_compatibility(CompatLevel::HardRequirement);
            builder_ref
                .handle_access(AccessNet::ConnectTcp)
                .map_err(|e| {
                    anyhow!("kernel rejects AccessNet::ConnectTcp (need Linux 6.7+): {e}")
                })?;
            builder_ref.set_compatibility(CompatLevel::BestEffort);
        }
    }

    let mut ruleset = builder.create().context("create landlock ruleset")?;

    for path in &profile.read_paths {
        let fd = PathFd::new(path)
            .with_context(|| format!("open read_path {} for landlock", path.display()))?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, fs_read))
            .with_context(|| format!("add read rule {}", path.display()))?;
    }
    for path in &profile.write_paths {
        let fd = PathFd::new(path)
            .with_context(|| format!("open write_path {} for landlock", path.display()))?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, fs_read | fs_write))
            .with_context(|| format!("add write rule {}", path.display()))?;
    }
    for entry in &profile.net_outbound_allowlist {
        ruleset = ruleset
            .add_rule(NetPort::new(entry.port, AccessNet::ConnectTcp))
            .with_context(|| format!("add net rule {}:{}", entry.host, entry.port))?;
    }

    Ok(ruleset)
}

/// Apply the ruleset to the current task. Called from the child's
/// `pre_exec` closure. Returns the kernel's `RulesetStatus` so the
/// caller can decide whether to abort if enforcement was downgraded.
pub fn restrict(ruleset: RulesetCreated) -> Result<RulesetStatus> {
    let restrict = ruleset
        .restrict_self()
        .context("landlock restrict_self failed")?;
    Ok(restrict.ruleset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agt::policy::schema::Profile;

    #[test]
    fn ruleset_builds() {
        // Coverage: the construction shape works on a synthetic
        // profile rooted at a tempdir. We do NOT call `restrict_self`
        // — that would sandbox the test process and break later
        // tests.
        let tmp = tempfile::tempdir().expect("tempdir");
        let profile = Profile {
            read_paths: vec![tmp.path().to_path_buf()],
            ..Profile::default()
        };

        let _ruleset = build(&profile).expect("build ruleset");
    }
}
