//! BUG-20260525-2350 guard: the shipped tmpfiles.d drop-in MUST cover
//! every `cpufreq/policy*/energy_performance_preference` leaf the
//! kernel exposes — otherwise the EPP actuator can't flip half the
//! CPU's policies and the daemon's reward signal gets phantom drift
//! from the half-applied lever.
//!
//! Two acceptable shapes:
//! 1. A single glob entry: `z /sys/.../cpufreq/policy*/energy_performance_preference`.
//!    Survives kernel upgrades that add/remove policies. Preferred.
//! 2. An explicit enumeration whose count is at least
//!    `std::thread::available_parallelism()` — i.e. the build host's
//!    online CPU count. Acceptable but brittle; the next CPU upgrade
//!    will silently regress this back to the BUG-20260525-2350 state
//!    until someone manually grows the list.
//!
//! Either shape passes; both shapes failing is the regression we're
//! pinning here.

const TMPFILES_CONF: &str = include_str!("../configs/systemd/tmpfiles.d/sy-power.conf");

/// The literal leaf name we expect to find chmoded. Mirrors the
/// `EPP_LEAF` const in `src/power/apply/epp.rs` — drift here means
/// the kernel renamed the file, in which case the actuator also needs
/// an update.
const EPP_LEAF: &str = "energy_performance_preference";

/// The literal glob shape the actuator-resilience fix prefers — one
/// line that covers every present and future `policy<N>` directory.
const GLOB_TOKEN: &str = "policy*/energy_performance_preference";

#[test]
fn tmpfiles_covers_every_cpufreq_policy_epp_leaf() {
    // Strip comments and blank lines so the line scan only sees
    // actual `z` directives.
    let directives: Vec<&str> = TMPFILES_CONF
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    // Shape 1: a single glob line covering every policy.
    let has_glob = directives.iter().any(|l| l.contains(GLOB_TOKEN));

    // Shape 2: explicit enumeration with ≥ available_parallelism()
    // entries. We DON'T match on `policy0`/`policy1`/etc. by index;
    // we just count distinct `z` directives that target the EPP leaf
    // under `cpufreq/policy`.
    let explicit_count = directives
        .iter()
        .filter(|l| l.starts_with("z "))
        .filter(|l| l.contains("/cpufreq/policy"))
        .filter(|l| l.contains(EPP_LEAF))
        .count();

    let online_cpus = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);

    assert!(
        has_glob || explicit_count >= online_cpus,
        "tmpfiles.d must EITHER use the `{GLOB_TOKEN}` glob OR \
         enumerate ≥ {online_cpus} explicit policy EPP entries \
         (found glob={has_glob}, explicit_count={explicit_count}). \
         Without coverage, sy-powerd loses write access on the \
         uncovered policies — see BUG-20260525-2350.",
    );
}
