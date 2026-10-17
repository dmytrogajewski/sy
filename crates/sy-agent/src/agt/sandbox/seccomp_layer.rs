//! Seccomp-bpf layer — SPEC §4.4 step 3 layer 3.
//!
//! Default action: `Allow`. We explicitly `Errno(EPERM)` a curated set
//! of high-risk syscalls that bypass or undermine the Landlock /
//! `PR_SET_NO_NEW_PRIVS` layers: `ptrace`, `bpf`, `mount`, `umount2`,
//! `pivot_root`, `unshare`, `setns`, `kexec_load`, `kexec_file_load`,
//! `init_module`, `finit_module`, `delete_module`, `swapon`, `swapoff`,
//! `reboot`, `clock_settime`, `settimeofday`, `process_vm_readv`,
//! `process_vm_writev`, `keyctl`, `add_key`, `request_key`.
//!
//! Argument matching for high-risk syscalls (`execveat`, `unlinkat`,
//! `mount`) is intentionally OUT of this iteration — the deny is a
//! plain syscall-number block. The roadmap revisits per-arg gates
//! once the curated allowlist style stabilises.

use anyhow::{Context, Result};
use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, SeccompRule, TargetArch};
use std::collections::BTreeMap;

/// Syscalls explicitly denied with EPERM. Listed by name; resolved
/// against `libc` at compile time. Order is alphabetical for review
/// friction — clippy doesn't enforce, humans do.
const DENY_SYSCALLS: &[i64] = &[
    libc::SYS_add_key,
    libc::SYS_bpf,
    libc::SYS_clock_settime,
    libc::SYS_delete_module,
    libc::SYS_finit_module,
    libc::SYS_init_module,
    libc::SYS_kexec_file_load,
    libc::SYS_kexec_load,
    libc::SYS_keyctl,
    libc::SYS_mount,
    libc::SYS_pivot_root,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_ptrace,
    libc::SYS_reboot,
    libc::SYS_request_key,
    libc::SYS_setns,
    libc::SYS_settimeofday,
    libc::SYS_swapoff,
    libc::SYS_swapon,
    libc::SYS_umount2,
    libc::SYS_unshare,
];

/// EPERM error number used by the deny action. Picked over `EACCES`
/// because `setuid` callers see `EPERM` from the kernel when
/// `PR_SET_NO_NEW_PRIVS` blocks them — using the same number keeps
/// strace output consistent across layers.
const DENY_ERRNO: u32 = libc::EPERM as u32;

/// Compile the deny filter into a BPF program ready for
/// `seccompiler::apply_filter`. Separates compilation (allocates,
/// can fail) from application (signal-safe, applied inside the child
/// `pre_exec`).
pub fn compile() -> Result<BpfProgram> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for syscall in DENY_SYSCALLS {
        // Empty rule vec means "match every invocation of this syscall"
        // — i.e. the deny applies regardless of argument values.
        rules.insert(*syscall, Vec::new());
    }
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(DENY_ERRNO),
        target_arch(),
    )
    .context("seccompiler::SeccompFilter::new")?;
    let program: BpfProgram = filter.try_into().context("compile seccomp BPF")?;
    Ok(program)
}

/// Apply the precompiled filter to the calling thread. Called from
/// the child's `pre_exec` closure after Landlock has been activated.
/// `seccompiler::apply_filter` is signal-safe per the crate's docs.
pub fn apply(program: &BpfProgram) -> Result<()> {
    seccompiler::apply_filter(program).context("seccompiler::apply_filter")
}

#[cfg(target_arch = "x86_64")]
fn target_arch() -> TargetArch {
    TargetArch::x86_64
}
#[cfg(target_arch = "aarch64")]
fn target_arch() -> TargetArch {
    TargetArch::aarch64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_constructs() {
        // The compile step must succeed (no syscall numbers out of
        // range, no architecture mismatch). We don't `apply` because
        // that would sandbox the test process.
        let program = compile().expect("compile filter");
        assert!(!program.is_empty(), "BPF program is non-empty");
    }
}
