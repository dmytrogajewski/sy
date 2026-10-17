//! In-process sandbox layers — SPEC §4.4 step 3 (arch-agent-sandbox
//! Step 3). Applies the four layers between `fork(2)` and `execve(2)`
//! in the order documented in [`exec`]:
//!
//! 1. `prctl(PR_SET_NO_NEW_PRIVS, 1)` — strips `setuid`/file-cap
//!    escalation so the rest of the layers can't be undone.
//! 2. `landlock_layer::install` — FS read/write allowlist and (kernel
//!    ≥ 6.7, ABI v4) per-host:port outbound TCP gate.
//! 3. `seccomp_layer::install` — curated EPERM deny set for ptrace,
//!    bpf, mount, kexec, … syscalls that bypass the FS/network gates.
//! 4. `execve` — the new image inherits all three restrictions.
//!
//! The `env_scrub` module is a pure helper used by [`exec`] before
//! spawn to drop everything outside the profile's allowlist
//! (`PATH`, `HOME`, `LANG`, `TERM`, …).

pub mod env_scrub;
pub mod exec;
pub mod landlock_layer;
pub mod scope;
pub mod seccomp_layer;

pub use exec::fork_and_exec;
// `scope::run_in_scope` is reached as `sandbox::scope::run_in_scope` —
// the re-export is deferred until a daemon-side call site lands (see
// `arch-agent-sandbox` Step 4's daemon-swap follow-up). Re-exporting
// without an in-tree consumer trips clippy's `unused_imports` lint
// under `-D warnings`.
