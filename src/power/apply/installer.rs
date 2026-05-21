//! Installer core for `sy power apply` (R1 cut).
//!
//! See `apply/mod.rs` for scope. This module owns three operations:
//!
//! 1. **Telemetry dir.** `mkdir -p <state_root>/power/`.
//! 2. **User systemd unit.** Drop
//!    `configs/systemd/user/sy-powerd.service` into
//!    `<user_unit_root>/sy-powerd.service`. Diff against any
//!    existing file so a re-apply that finds an identical payload
//!    is a no-op.
//! 3. **Polkit rule.** Drop `configs/policy/10-sy-power.rules` into
//!    `<polkit_root>/10-sy-power.rules`. The production polkit root
//!    (`/etc/polkit-1/rules.d/`) is root-owned; we *warn* instead of
//!    failing when the destination is unwritable so an unprivileged
//!    `sy power apply` still completes the user-scoped work. Tests
//!    redirect to a tempdir via `InstallOpts.polkit_root`.
//!
//! On top of that: detect a running `power-profiles-daemon` and emit
//! a `ChangeRecord::Warning`. R1 leaves PPD alone — the PPD shim
//! lands in Step 37.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// Embedded source of truth for the systemd unit. Using `include_str!`
/// keeps `sy` self-contained — the installed binary doesn't need the
/// repo checked out alongside it to reproduce the unit file.
const SY_POWERD_UNIT: &str = include_str!("../../../configs/systemd/user/sy-powerd.service");

/// Embedded source of truth for the polkit rule. Same rationale as
/// the unit file: `sy power apply` from a release binary must produce
/// byte-identical artifacts regardless of where it's invoked.
const POLKIT_RULE: &str = include_str!("../../../configs/policy/10-sy-power.rules");

/// Embedded source of truth for the grub drop-in. Step 27 lever:
/// `amd_dynamic_epp=disable` so the EPP actuator's per-policy writes
/// land in `/sys/.../cpufreq/policy*/energy_performance_preference`
/// instead of being silently no-op'd by the kernel.
const GRUB_DROPIN: &str = include_str!("../../../configs/grub/10-sy-power.cfg");

/// Step H5: embedded D-Bus system policy granting the `wheel` group
/// the right to `own` `net.hadess.PowerProfiles`. Fedora 43 ships a
/// vendor `net.hadess.PowerProfiles.conf` that restricts ownership to
/// `user="root"`, blocking the user-mode `sy-powerd` PPD shim. Our
/// drop-in's `99-` prefix loads after the vendor file (alphabetical
/// order) so the `wheel` allowance wins.
const DBUS_POLICY: &str = include_str!("../../../configs/dbus-1/system.d/99-sy-power.conf");

/// Step H6: embedded `systemd-tmpfiles.d(5)` drop-in flipping sysfs
/// knob ownership (`platform_profile`, per-policy EPP files,
/// `power_dpm_force_performance_level`) to `root:wheel 0664` at boot.
/// The HX 370 / kernel 7.0.6 host ships these as `root:root rw-r--r--`,
/// blocking userland writes from the wheel-group `sy-powerd` daemon.
/// On a successful install we shell out to `systemd-tmpfiles --create`
/// so the perms apply immediately without rebooting.
const TMPFILES_CONF: &str = include_str!("../../../configs/systemd/tmpfiles.d/sy-power.conf");

/// Step P3-2: embedded source of truth for the system-mode oneshot that
/// sets `amd-pstate=active` + `scaling_governor=powersave` at every
/// boot. Without it the next reboot reverts the runtime flip and EPP
/// writes go back to silently no-op-or-EBUSY. The unit lands under
/// `/etc/systemd/system/`; on install we shell out to
/// `systemctl daemon-reload && systemctl enable --now
/// sy-power-cpufreq.service` so the writes happen immediately AND
/// persist across reboot.
const CPUFREQ_ONESHOT_UNIT: &str =
    include_str!("../../../configs/systemd/system/sy-power-cpufreq.service");

/// Step P3-4: embedded source of truth for the udev rule that owns DRM
/// iGPU `power_dpm_force_performance_level` permission grants. The Step
/// H3 tmpfiles.d entries hard-coded card0/1/2 — the kernel renumbers
/// drm cards across reboots based on probe order, so the literal paths
/// don't survive a card-index change. This rule matches by AMD vendor
/// ID (`ATTR{vendor}=="0x1002"`) and applies the perm overrides to
/// whichever `card<N>` the kernel assigns. Lands under
/// `/etc/udev/rules.d/99-sy-power.rules`; on install we shell out to
/// `udevadm control --reload-rules` + `udevadm trigger
/// --subsystem-match=drm` so the rule takes effect without reboot.
const UDEV_RULE: &str = include_str!("../../../configs/udev/rules.d/99-sy-power.rules");

/// Basename of the user systemd unit we install. The full source
/// lives under `configs/systemd/user/`; we only need the leaf when
/// computing the destination path.
const UNIT_BASENAME: &str = "sy-powerd.service";

/// Basename of the polkit rule we install. Numeric prefix follows
/// the polkit convention (`10-` ⇒ sy's rules apply before any
/// distro-shipped `50-default.rules`).
const POLKIT_BASENAME: &str = "10-sy-power.rules";

/// Basename of the grub drop-in we install under `<grub_root>/`. The
/// `10-` prefix mirrors the polkit convention so distro-shipped
/// fragments (typically `50-…`) override sy if both are present.
const GRUB_DROPIN_BASENAME: &str = "10-sy-power.cfg";

/// Step H5: basename of the D-Bus policy drop-in we install under
/// `<dbus_root>/`. The `99-` prefix is load-bearing: D-Bus reads
/// `system.d/` in alphabetical order with later files overriding
/// earlier ones, so `99-sy-power.conf` MUST sort after the vendor
/// `net.hadess.PowerProfiles.conf` for the `wheel` allowance to win.
const DBUS_POLICY_BASENAME: &str = "99-sy-power.conf";

/// Step H6: basename of the tmpfiles.d drop-in. No numeric prefix — the
/// file is a single artifact whose contents (sysfs `z` lines) don't
/// depend on load order relative to other tmpfiles.d fragments.
const TMPFILES_BASENAME: &str = "sy-power.conf";

/// Step P3-2: basename of the system-mode cpufreq oneshot. Lands under
/// `<system_unit_root>/sy-power-cpufreq.service`. `systemctl enable
/// --now` is invoked against this basename so the unit starts
/// immediately AND persists across reboot.
const CPUFREQ_ONESHOT_BASENAME: &str = "sy-power-cpufreq.service";

/// Step P3-4: basename of the udev rule. The `99-` prefix ensures the
/// rule loads after distro defaults — udev applies rules in
/// alphabetical order with later rules' RUN+= actions appended, so
/// our perm overrides aren't clobbered by an earlier-loaded vendor
/// rule on the same matching device.
const UDEV_RULE_BASENAME: &str = "99-sy-power.rules";

/// `GRUB_CMDLINE_LINUX` variable name we scan for in `grub_cfg_file`.
/// Used both by the conflict detector ("already enables
/// amd_dynamic_epp") and (indirectly) by the drop-in template.
const GRUB_CMDLINE_VAR: &str = "GRUB_CMDLINE_LINUX";

/// Token the conflict detector flags as the inverse of our drop-in.
/// Two spellings exist in the wild (`enable` and `enabled`); we treat
/// either as a hard conflict and refuse to silently override.
const AMD_DYNAMIC_EPP_ENABLE: &str = "amd_dynamic_epp=enable";
const AMD_DYNAMIC_EPP_ENABLED: &str = "amd_dynamic_epp=enabled";

/// Output path `grub2-mkconfig -o <path>` writes the generated config
/// to. Fedora 43's well-known path; other distros use `update-grub`,
/// which already knows where to write.
const GRUB_CFG_OUTPUT: &str = "/boot/grub2/grub.cfg";

/// Telemetry subdirectory rooted at `state_root` (typically
/// `~/.local/state/sy`). Matches `power::power_state_dir_for_daemon`
/// so the daemon's NDJSON log lands where `sy power log` reads it.
const TELEMETRY_SUBDIR: &str = "power";

/// Step 37: basename of the `power-profiles-daemon` user unit. The
/// `systemctl --user mask` invocation creates a symlink at
/// `<user_unit_root>/<PPD_UNIT_BASENAME>` → `/dev/null`; the installer
/// detects that symlink to keep re-apply idempotent (second call
/// surfaces `AlreadyMatches` and does NOT re-invoke systemctl).
const PPD_UNIT_BASENAME: &str = "power-profiles-daemon.service";

/// Step 37: canonical filesystem locations of a vendor-installed
/// power-profiles-daemon unit. Production callers pass this slice
/// verbatim through [`InstallOpts::ppd_unit_paths`]; tests inject
/// tempdir paths so the detection branch is hermetic.
pub fn default_ppd_unit_paths() -> Vec<PathBuf> {
    [
        "/usr/lib/systemd/system/power-profiles-daemon.service",
        "/lib/systemd/system/power-profiles-daemon.service",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

/// Mechanism the installer used to push the `amd_dynamic_epp=disable`
/// kernel cmdline parameter at apply time. Stable for stdout / JSON
/// rendering and for tests that need to assert which branch fired.
///
/// - `Grubby` — Fedora's canonical path: invoked
///   `grubby --update-kernel=ALL --args="amd_dynamic_epp=disable"` so
///   BLS entries + `/etc/kernel/cmdline` are updated. This is the
///   default on hosts that ship `/usr/bin/grubby`.
/// - `GrubD` — Debian / Arch fallback: wrote
///   `<grub_root>/10-sy-power.cfg` and regenerated `grub.cfg` via
///   `grub2-mkconfig` / `update-grub`. The drop-in is shipped under
///   `configs/grub/` in tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrubbyOrDropIn {
    Grubby,
    GrubD,
}

/// Production grubby-detection: `which::which("grubby").is_ok()`. The
/// installer accepts this via [`InstallOpts::grubby_detect`] as an
/// injectable predicate so tests can force either branch without
/// touching the host's `$PATH`.
pub fn default_grubby_detect() -> Box<dyn Fn() -> bool + Send + Sync> {
    Box::new(|| which::which("grubby").is_ok())
}

/// One change the installer made (or, under `dry_run`, would make).
///
/// Stable for stdout pretty-print and (later, Step 35) JSON. Variants
/// describe outcomes, not actions — `Updated` means the destination
/// existed with a different payload and was rewritten; `AlreadyMatches`
/// is the idempotent-re-apply marker that lets a caller diff two
/// runs and confirm zero divergence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeRecord {
    /// Destination didn't exist; we wrote it (or would in dry-run).
    Created(PathBuf),
    /// Destination existed with different content; we rewrote it.
    Updated(PathBuf),
    /// Destination already byte-identical; no write performed.
    AlreadyMatches(PathBuf),
    /// `mkdir -p`-style directory creation. Idempotent: emitted only
    /// when the dir genuinely didn't exist beforehand.
    DirectoryCreated(PathBuf),
    /// `systemctl --user daemon-reload` ran (skipped in dry-run /
    /// when `run_daemon_reload` is false).
    SystemdReload,
    /// Step H6: `systemd-tmpfiles --create <dest>` ran successfully so
    /// the freshly-written drop-in's sysfs ownership/mode overrides
    /// land without waiting for the next boot. Only emitted on a real
    /// write — `AlreadyMatches` skips the shell-out.
    TmpfilesApplied,
    /// Step P3-1: the installer pushed `amd_dynamic_epp=disable` into
    /// the kernel command line via the mechanism named in `method`.
    /// Emitted only on a real write — a re-apply against an already-
    /// installed host stays silent (`AlreadyMatches` for the grub.d
    /// drop-in; grubby's idempotent nature means a re-apply with grubby
    /// detected still re-invokes the binary, so the record fires both
    /// times on that branch).
    KernelCmdlineUpdated { method: GrubbyOrDropIn },
    /// Soft diagnostic — the install completed but the operator
    /// should know something. Examples: PPD detected, polkit
    /// destination unwritable.
    Warning(String),
}

/// Abstract shell-out for the grub-regeneration step. Mirrors
/// `apply::npu::CommandRunner` (Step 16) so tests can assert that
/// `grub2-mkconfig` ran exactly once across a first-then-reapply
/// pair, without ever spawning the real binary. Distinct trait from
/// the NPU one because the failure-shape differs (no `NpuError`
/// here — generic anyhow is fine for grub).
pub trait CommandRunner: Send + Sync {
    /// Run `cmd args…`. Returns `Ok(())` on exit code 0; any other
    /// outcome (non-zero exit, missing binary, spawn failure) maps to
    /// `Err`. Callers downstream interpret an `Err` as "binary not
    /// present" and fall through to the next candidate.
    fn run(&self, cmd: &str, args: &[&str]) -> Result<()>;
}

/// Production [`CommandRunner`] — spawns via `std::process::Command`.
/// Non-zero exit codes and spawn failures both surface as `Err` so
/// the fallback chain (`grub2-mkconfig` → `update-grub` → warn) can
/// treat them uniformly.
#[derive(Debug, Default)]
pub struct SystemRunner;

impl SystemRunner {
    pub fn new() -> Self {
        Self
    }
}

impl CommandRunner for SystemRunner {
    fn run(&self, cmd: &str, args: &[&str]) -> Result<()> {
        let status = Command::new(cmd)
            .args(args)
            .status()
            .with_context(|| format!("spawn {cmd}"))?;
        if !status.success() {
            return Err(anyhow::anyhow!("{cmd} exited with {status}"));
        }
        Ok(())
    }
}

/// Caller-supplied roots for the install. Tests inject tempdirs;
/// production injects the real `$HOME`-derived paths so a single
/// `install` call drives the full apply.
pub struct InstallOpts {
    /// Print the plan; do not touch the filesystem and do not run
    /// `daemon-reload`.
    pub dry_run: bool,
    /// `~/.local/state/sy` in production; the telemetry subdir is
    /// `<state_root>/power/`.
    pub state_root: PathBuf,
    /// `~/.config/systemd/user` in production. The unit lands at
    /// `<user_unit_root>/sy-powerd.service`.
    pub user_unit_root: PathBuf,
    /// `/etc/polkit-1/rules.d/` in production. Unwritable on
    /// unprivileged installs ⇒ we warn instead of erroring out.
    pub polkit_root: PathBuf,
    /// `/etc/default/grub.d` in production — where the
    /// `10-sy-power.cfg` drop-in lands. Tests redirect to a tempdir.
    pub grub_root: PathBuf,
    /// Step H5: `/etc/dbus-1/system.d` in production — where the
    /// `99-sy-power.conf` D-Bus policy drop-in lands. Tests redirect
    /// to a tempdir.
    pub dbus_root: PathBuf,
    /// Step H6: `/etc/tmpfiles.d` in production — where the
    /// `sy-power.conf` tmpfiles.d drop-in lands. Tests redirect to a
    /// tempdir. On successful (re)write the installer also shells out
    /// to `systemd-tmpfiles --create <dest>` so the sysfs ownership/
    /// mode overrides apply immediately.
    pub tmpfiles_root: PathBuf,
    /// Step P3-2: `/etc/systemd/system` in production — where the
    /// system-mode `sy-power-cpufreq.service` oneshot lands. Tests
    /// redirect to a tempdir. On a real write the installer shells out
    /// to `systemctl daemon-reload` + `systemctl enable --now
    /// sy-power-cpufreq.service` so the amd-pstate=active +
    /// scaling_governor=powersave writes happen immediately AND
    /// persist across reboot.
    pub system_unit_root: PathBuf,
    /// Step P3-4: `/etc/udev/rules.d` in production — where the
    /// `99-sy-power.rules` udev rule lands. Tests redirect to a
    /// tempdir. On a real write the installer shells out to
    /// `udevadm control --reload-rules` + `udevadm trigger
    /// --subsystem-match=drm` so the rule takes effect immediately;
    /// the rule itself supersedes the Step H3 tmpfiles.d card-indexed
    /// entries which can't survive kernel card renumbering.
    pub udev_rules_root: PathBuf,
    /// `/etc/default/grub` in production — the parent file the grub
    /// generator includes drop-ins from. We *read-only* scan this for
    /// a pre-existing `amd_dynamic_epp=enable` conflict; we never
    /// rewrite it.
    pub grub_cfg_file: PathBuf,
    /// Shell-out indirection for `grub2-mkconfig` / `update-grub`.
    /// Production uses [`SystemRunner`]; tests inject a `MockRunner`
    /// so `make test` never invokes the real generator.
    pub command_runner: Box<dyn CommandRunner>,
    /// Whether to invoke `systemctl --user daemon-reload`. False in
    /// tests so `make test` stays hermetic; true in production.
    pub run_daemon_reload: bool,
    /// Step 37: the operator passed `--yes` to `sy power apply`. Required
    /// to take destructive actions — currently only the PPD-mask path
    /// gated on `--yes && !with_ppd && ppd_detected`.
    pub yes: bool,
    /// Step 37: the operator passed `--with-ppd`. Keeps
    /// `power-profiles-daemon` running; the PPD shim does NOT bind
    /// `net.hadess.PowerProfiles` (PPD keeps the name).
    pub with_ppd: bool,
    /// Step 37: where to look for an installed PPD systemd unit.
    /// Production passes [`DEFAULT_PPD_UNIT_PATHS`]; tests inject
    /// tempdir paths so the detection branch is deterministic.
    pub ppd_unit_paths: Vec<PathBuf>,
    /// Step P3-1: injectable predicate the installer uses to decide
    /// whether to push the kernel cmdline via `grubby` (Fedora) or via
    /// the Debian-style `<grub_root>/10-sy-power.cfg` drop-in.
    /// Production passes [`default_grubby_detect`] (which shells out
    /// to the `which` crate); tests pass a closure returning a fixed
    /// `bool` so the branch is deterministic regardless of host `$PATH`.
    pub grubby_detect: Box<dyn Fn() -> bool + Send + Sync>,
}

/// Drive the install. Idempotent: a second call against the same
/// roots returns only [`ChangeRecord::AlreadyMatches`] /
/// [`ChangeRecord::Warning`] entries (or fewer — directories that
/// already exist emit nothing). Never panics; filesystem errors
/// bubble up via `anyhow`.
pub fn install(opts: &InstallOpts) -> Result<Vec<ChangeRecord>> {
    let mut out: Vec<ChangeRecord> = Vec::new();
    install_telemetry_dir(opts, &mut out)?;
    install_user_unit(opts, &mut out)?;
    install_polkit_rule(opts, &mut out)?;
    install_kernel_cmdline_param(opts, &mut out)?;
    install_dbus_policy(opts, &mut out)?;
    install_tmpfiles_dropin(opts, &mut out)?;
    install_cpufreq_oneshot(opts, &mut out)?;
    install_udev_rule(opts, &mut out)?;
    handle_ppd_conflict(opts, &mut out)?;
    if !opts.dry_run && opts.run_daemon_reload && touched_filesystem(&out) {
        run_daemon_reload(&mut out)?;
    }
    Ok(out)
}

/// True iff the records so far include a write or directory create.
/// We skip `daemon-reload` on a clean re-apply so the second `sy
/// power apply` is a true diff-equals-zero — matches the "0 changes
/// on re-apply" idempotency target in the Step 13 DoD.
fn touched_filesystem(out: &[ChangeRecord]) -> bool {
    out.iter().any(|r| {
        matches!(
            r,
            ChangeRecord::Created(_) | ChangeRecord::Updated(_) | ChangeRecord::DirectoryCreated(_)
        )
    })
}

/// Step 1: `~/.local/state/sy/power/`. We only emit the record when
/// the directory didn't exist before, so a re-apply is silent.
fn install_telemetry_dir(opts: &InstallOpts, out: &mut Vec<ChangeRecord>) -> Result<()> {
    let dir = opts.state_root.join(TELEMETRY_SUBDIR);
    if dir.is_dir() {
        return Ok(());
    }
    out.push(ChangeRecord::DirectoryCreated(dir.clone()));
    if opts.dry_run {
        return Ok(());
    }
    fs::create_dir_all(&dir).with_context(|| format!("mkdir -p {}", dir.display()))?;
    Ok(())
}

/// Step 2: write the embedded unit to `<user_unit_root>/sy-powerd.service`,
/// diffing first so a byte-identical payload is a `AlreadyMatches`.
fn install_user_unit(opts: &InstallOpts, out: &mut Vec<ChangeRecord>) -> Result<()> {
    let dest = opts.user_unit_root.join(UNIT_BASENAME);
    write_if_changed(&dest, SY_POWERD_UNIT, opts.dry_run, out)
}

/// Step 3: write the embedded polkit rule. The production polkit
/// root is root-owned, so the unprivileged path can't actually
/// write there — we degrade to a `Warning` so the rest of the apply
/// (which *is* user-scoped) still succeeds.
///
/// Step H4: when the write path fails AND the dest already exists
/// with matching content, surface `AlreadyMatches` instead of the
/// misleading "unwritable" Warning — re-applying after a privileged
/// install must not look like a half-broken install.
fn install_polkit_rule(opts: &InstallOpts, out: &mut Vec<ChangeRecord>) -> Result<()> {
    let dest = opts.polkit_root.join(POLKIT_BASENAME);
    let pre_len = out.len();
    if write_if_changed(&dest, POLKIT_RULE, opts.dry_run, out).is_ok() {
        return Ok(());
    }
    // Roll back any half-pushed record (e.g. Updated/Created emitted
    // before the write failed) so the records vec stays consistent.
    out.truncate(pre_len);
    if let Ok(existing) = fs::read_to_string(&dest) {
        if existing == POLKIT_RULE {
            out.push(ChangeRecord::AlreadyMatches(dest));
            return Ok(());
        }
    }
    out.push(ChangeRecord::Warning(format!(
        "polkit destination {} unwritable; re-run as root or copy {} manually",
        dest.display(),
        POLKIT_BASENAME,
    )));
    Ok(())
}

/// Step H5: write `99-sy-power.conf` into `<dbus_root>/`. Mirrors
/// the polkit path — the production root (`/etc/dbus-1/system.d`)
/// is root-owned, so the unprivileged path can't write; we degrade
/// to a `Warning`. When the dest is unwritable AND already exists
/// with matching content, surface `AlreadyMatches` (H4-style
/// fallback) so a re-apply after a privileged install isn't
/// misreported as half-broken.
fn install_dbus_policy(opts: &InstallOpts, out: &mut Vec<ChangeRecord>) -> Result<()> {
    let dest = opts.dbus_root.join(DBUS_POLICY_BASENAME);
    let pre_len = out.len();
    if write_if_changed(&dest, DBUS_POLICY, opts.dry_run, out).is_ok() {
        return Ok(());
    }
    out.truncate(pre_len);
    if let Ok(existing) = fs::read_to_string(&dest) {
        if existing == DBUS_POLICY {
            out.push(ChangeRecord::AlreadyMatches(dest));
            return Ok(());
        }
    }
    out.push(ChangeRecord::Warning(format!(
        "dbus policy destination {} unwritable; re-run as root or copy {} manually",
        dest.display(),
        DBUS_POLICY_BASENAME,
    )));
    Ok(())
}

/// Step H6: write `sy-power.conf` into `<tmpfiles_root>/`. Mirrors the
/// polkit / dbus paths — the production root (`/etc/tmpfiles.d`) is
/// root-owned, so the unprivileged path can't write; we degrade to a
/// `Warning`. When the dest is unwritable AND already exists with
/// matching content, surface `AlreadyMatches` (H4-style fallback) so a
/// re-apply after a privileged install isn't misreported as
/// half-broken.
///
/// On a real write (Created or Updated, dry-run excluded), shell out
/// to `systemd-tmpfiles --create <dest>` via the injected
/// [`CommandRunner`] so the sysfs ownership/mode overrides apply
/// without waiting for the next boot. `AlreadyMatches` skips the
/// shell-out — nothing changed on disk, no need to re-apply.
fn install_tmpfiles_dropin(opts: &InstallOpts, out: &mut Vec<ChangeRecord>) -> Result<()> {
    let dest = opts.tmpfiles_root.join(TMPFILES_BASENAME);
    let pre_len = out.len();
    if write_if_changed(&dest, TMPFILES_CONF, opts.dry_run, out).is_err() {
        out.truncate(pre_len);
        if let Ok(existing) = fs::read_to_string(&dest) {
            if existing == TMPFILES_CONF {
                out.push(ChangeRecord::AlreadyMatches(dest));
                return Ok(());
            }
        }
        out.push(ChangeRecord::Warning(format!(
            "tmpfiles destination {} unwritable; re-run as root or copy {} manually",
            dest.display(),
            TMPFILES_BASENAME,
        )));
        return Ok(());
    }
    let newly_written = out
        .iter()
        .skip(pre_len)
        .any(|r| matches!(r, ChangeRecord::Created(_) | ChangeRecord::Updated(_)));
    if newly_written && !opts.dry_run {
        let dest_str = dest.to_string_lossy().to_string();
        if opts
            .command_runner
            .run("systemd-tmpfiles", &["--create", dest_str.as_str()])
            .is_ok()
        {
            out.push(ChangeRecord::TmpfilesApplied);
        } else {
            out.push(ChangeRecord::Warning(format!(
                "tmpfiles drop-in installed at {} but `systemd-tmpfiles --create` failed; reboot or re-run as root",
                dest.display(),
            )));
        }
    }
    Ok(())
}

/// Step P3-2: write `sy-power-cpufreq.service` into `<system_unit_root>/`.
/// On a successful (re)write — i.e. `Created` or `Updated`, dry-run
/// excluded — shell out via the injected [`CommandRunner`]:
///
/// 1. `systemctl daemon-reload` so systemd picks up the new unit;
/// 2. `systemctl enable --now sy-power-cpufreq.service` so the unit
///    starts immediately AND links into `multi-user.target.wants/` so
///    it runs again on every boot.
///
/// Mirrors the H6 `install_tmpfiles_dropin` shape (write → shell out)
/// and the polkit / dbus content-diff fallback (H4-style): if the
/// production root is unwritable but the dest already exists with the
/// embedded payload, surface `AlreadyMatches` instead of the misleading
/// "unwritable" Warning. `AlreadyMatches` skips both shell-outs — the
/// unit is already installed and presumably enabled.
fn install_cpufreq_oneshot(opts: &InstallOpts, out: &mut Vec<ChangeRecord>) -> Result<()> {
    let dest = opts.system_unit_root.join(CPUFREQ_ONESHOT_BASENAME);
    let pre_len = out.len();
    if write_if_changed(&dest, CPUFREQ_ONESHOT_UNIT, opts.dry_run, out).is_err() {
        out.truncate(pre_len);
        if let Ok(existing) = fs::read_to_string(&dest) {
            if existing == CPUFREQ_ONESHOT_UNIT {
                out.push(ChangeRecord::AlreadyMatches(dest));
                return Ok(());
            }
        }
        out.push(ChangeRecord::Warning(format!(
            "cpufreq oneshot destination {} unwritable; re-run as root or copy {} manually",
            dest.display(),
            CPUFREQ_ONESHOT_BASENAME,
        )));
        return Ok(());
    }
    let newly_written = out
        .iter()
        .skip(pre_len)
        .any(|r| matches!(r, ChangeRecord::Created(_) | ChangeRecord::Updated(_)));
    if newly_written && !opts.dry_run {
        if let Err(e) = opts.command_runner.run("systemctl", &["daemon-reload"]) {
            out.push(ChangeRecord::Warning(format!(
                "cpufreq oneshot installed at {} but `systemctl daemon-reload` failed: {e}",
                dest.display(),
            )));
            return Ok(());
        }
        if let Err(e) = opts
            .command_runner
            .run("systemctl", &["enable", "--now", CPUFREQ_ONESHOT_BASENAME])
        {
            out.push(ChangeRecord::Warning(format!(
                "cpufreq oneshot installed at {} but `systemctl enable --now {}` failed: {e}",
                dest.display(),
                CPUFREQ_ONESHOT_BASENAME,
            )));
        }
    }
    Ok(())
}

/// Step P3-4: write `99-sy-power.rules` into `<udev_rules_root>/`.
/// Mirrors the H6 / P3-2 shape (write → shell out + H4-style content-
/// diff fallback). On a real write — `Created` or `Updated`, dry-run
/// excluded — shell out via the injected [`CommandRunner`]:
///
/// 1. `udevadm control --reload-rules` so udev re-reads its rule set;
/// 2. `udevadm trigger --subsystem-match=drm` so the rule fires against
///    every currently-present drm device, applying the perm overrides
///    immediately without waiting for a hot-plug / reboot.
///
/// If the production root (`/etc/udev/rules.d`) is unwritable but the
/// dest already exists with the embedded payload, surface
/// `AlreadyMatches` instead of the misleading "unwritable" Warning.
/// `AlreadyMatches` skips both shell-outs — the rule is already in
/// effect.
fn install_udev_rule(opts: &InstallOpts, out: &mut Vec<ChangeRecord>) -> Result<()> {
    let dest = opts.udev_rules_root.join(UDEV_RULE_BASENAME);
    let pre_len = out.len();
    if write_if_changed(&dest, UDEV_RULE, opts.dry_run, out).is_err() {
        out.truncate(pre_len);
        if let Ok(existing) = fs::read_to_string(&dest) {
            if existing == UDEV_RULE {
                out.push(ChangeRecord::AlreadyMatches(dest));
                return Ok(());
            }
        }
        out.push(ChangeRecord::Warning(format!(
            "udev rules destination {} unwritable; re-run as root or copy {} manually",
            dest.display(),
            UDEV_RULE_BASENAME,
        )));
        return Ok(());
    }
    let newly_written = out
        .iter()
        .skip(pre_len)
        .any(|r| matches!(r, ChangeRecord::Created(_) | ChangeRecord::Updated(_)));
    if newly_written && !opts.dry_run {
        if let Err(e) = opts
            .command_runner
            .run("udevadm", &["control", "--reload-rules"])
        {
            out.push(ChangeRecord::Warning(format!(
                "udev rule installed at {} but `udevadm control --reload-rules` failed: {e}",
                dest.display(),
            )));
            return Ok(());
        }
        if let Err(e) = opts
            .command_runner
            .run("udevadm", &["trigger", "--subsystem-match=drm"])
        {
            out.push(ChangeRecord::Warning(format!(
                "udev rule installed at {} but `udevadm trigger --subsystem-match=drm` failed: {e}",
                dest.display(),
            )));
        }
    }
    Ok(())
}

/// Step 27 + P3-1: push `amd_dynamic_epp=disable` into the kernel
/// command line. On Fedora (host with `/usr/bin/grubby`) we invoke
/// `grubby --update-kernel=ALL --args="amd_dynamic_epp=disable"` —
/// that's the canonical Fedora path; `grub2-mkconfig` on Fedora does
/// NOT source `/etc/default/grub.d/`, so the Debian-style drop-in
/// lands but takes no effect. Elsewhere (Debian / Arch) we keep
/// writing the drop-in + regenerating `grub.cfg`. Both paths bail
/// early with a Warning when `/etc/default/grub` already pins
/// `amd_dynamic_epp=enable` — we never silently override an explicit
/// operator opt-in.
fn install_kernel_cmdline_param(opts: &InstallOpts, out: &mut Vec<ChangeRecord>) -> Result<()> {
    if let Some(msg) = detect_grub_cmdline_conflict(&opts.grub_cfg_file)? {
        out.push(ChangeRecord::Warning(msg));
        return Ok(());
    }
    if (opts.grubby_detect)() {
        install_via_grubby(opts, out);
        return Ok(());
    }
    install_via_grub_dropin(opts, out)
}

/// Fedora path: shell out to `grubby` via the injected
/// [`CommandRunner`]. On success, emit
/// `KernelCmdlineUpdated { method: Grubby }` plus the reboot-required
/// advisory (which also reminds UKI hosts to rebuild). On failure,
/// surface a Warning so the operator knows the cmdline param did NOT
/// land. Dry-run skips the shell-out and emits only the planned-action
/// record.
fn install_via_grubby(opts: &InstallOpts, out: &mut Vec<ChangeRecord>) {
    if opts.dry_run {
        out.push(ChangeRecord::KernelCmdlineUpdated {
            method: GrubbyOrDropIn::Grubby,
        });
        return;
    }
    match opts.command_runner.run(
        "grubby",
        &["--update-kernel=ALL", "--args=amd_dynamic_epp=disable"],
    ) {
        Ok(()) => {
            out.push(ChangeRecord::KernelCmdlineUpdated {
                method: GrubbyOrDropIn::Grubby,
            });
            out.push(ChangeRecord::Warning(
                "reboot required for amd_dynamic_epp=disable to take effect; UKI hosts must also run `ukify build` or `dracut --uefi`".to_string(),
            ));
        }
        Err(e) => {
            out.push(ChangeRecord::Warning(format!(
                "grubby --update-kernel=ALL --args=amd_dynamic_epp=disable failed: {e}; re-run as root or update /etc/kernel/cmdline manually",
            )));
        }
    }
}

/// Debian / Arch path: write `<grub_root>/10-sy-power.cfg`, regenerate
/// `grub.cfg` via `grub2-mkconfig` / `update-grub`, and emit the
/// reboot-required warning. Idempotent — a re-apply with the drop-in
/// already on disk surfaces `AlreadyMatches` and does NOT re-run the
/// generator.
fn install_via_grub_dropin(opts: &InstallOpts, out: &mut Vec<ChangeRecord>) -> Result<()> {
    let dest = opts.grub_root.join(GRUB_DROPIN_BASENAME);
    let pre_len = out.len();
    write_if_changed(&dest, GRUB_DROPIN, opts.dry_run, out)?;
    let newly_written = out
        .iter()
        .skip(pre_len)
        .any(|r| matches!(r, ChangeRecord::Created(_) | ChangeRecord::Updated(_)));
    if newly_written && !opts.dry_run {
        regenerate_grub_cfg(opts.command_runner.as_ref(), out);
        out.push(ChangeRecord::KernelCmdlineUpdated {
            method: GrubbyOrDropIn::GrubD,
        });
        out.push(ChangeRecord::Warning(
            "reboot required for amd_dynamic_epp=disable to take effect".to_string(),
        ));
    }
    Ok(())
}

/// Read `grub_cfg_file` and surface a Warning *string* if the
/// `GRUB_CMDLINE_LINUX` line already enables `amd_dynamic_epp`. ENOENT
/// is fine (no grub config means no conflict). All other errors
/// propagate — a permission denied on the production file would
/// otherwise mask a real install failure.
fn detect_grub_cmdline_conflict(grub_cfg_file: &Path) -> Result<Option<String>> {
    let body = match fs::read_to_string(grub_cfg_file) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!(
                "read {} for conflict scan",
                grub_cfg_file.display()
            )));
        }
    };
    for line in body.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with(GRUB_CMDLINE_VAR) {
            continue;
        }
        if trimmed.contains(AMD_DYNAMIC_EPP_ENABLE) || trimmed.contains(AMD_DYNAMIC_EPP_ENABLED) {
            return Ok(Some(format!(
                "conflict: amd_dynamic_epp=enable already in {GRUB_CMDLINE_VAR} — remove it then re-run sy power apply",
            )));
        }
    }
    Ok(None)
}

/// Run `grub2-mkconfig -o <GRUB_CFG_OUTPUT>`; fall back to
/// `update-grub`; if neither is available, warn so the operator
/// regenerates the boot config manually. Best-effort — a failure here
/// must not abort the rest of the apply (the drop-in is already on
/// disk and will take effect on the next reboot once grub.cfg is
/// regenerated by some means).
fn regenerate_grub_cfg(runner: &dyn CommandRunner, out: &mut Vec<ChangeRecord>) {
    if runner
        .run("grub2-mkconfig", &["-o", GRUB_CFG_OUTPUT])
        .is_ok()
    {
        return;
    }
    if runner.run("update-grub", &[]).is_ok() {
        return;
    }
    out.push(ChangeRecord::Warning(
        "install drop-in landed but neither grub2-mkconfig nor update-grub found — regenerate grub.cfg manually"
            .to_string(),
    ));
}

/// Compute the right `ChangeRecord` for `body` at `dest`, then (unless
/// dry-run) perform the write. Creates the parent directory on demand
/// so the polkit / user-unit roots don't need to pre-exist in tests.
fn write_if_changed(
    dest: &Path,
    body: &str,
    dry_run: bool,
    out: &mut Vec<ChangeRecord>,
) -> Result<()> {
    let record = match fs::read(dest) {
        Ok(existing) if existing == body.as_bytes() => ChangeRecord::AlreadyMatches(dest.into()),
        Ok(_) => ChangeRecord::Updated(dest.into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ChangeRecord::Created(dest.into()),
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!("read {}", dest.display())));
        }
    };
    let needs_write = !dry_run && !matches!(record, ChangeRecord::AlreadyMatches(_));
    out.push(record);
    if needs_write {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("mkdir -p {}", parent.display()))?;
        }
        fs::write(dest, body).with_context(|| format!("write {}", dest.display()))?;
    }
    Ok(())
}

/// Step 37: resolve the PPD-conflict decision.
///
/// 1. PPD absent → no-op (the host runs no competing daemon).
/// 2. `with_ppd` set → emit a Warning explaining PPD is kept active
///    and the shim will NOT bind `net.hadess.PowerProfiles`. No mask,
///    no shell-out.
/// 3. `yes && !with_ppd && ppd_detected` → install the systemd user
///    mask (a symlink at `<user_unit_root>/power-profiles-daemon.service`
///    → `/dev/null`). Re-applying when the mask is already in place is
///    a no-op (`AlreadyMatches`, runner not invoked).
/// 4. `!yes && !with_ppd && ppd_detected` → advisory Warning instructing
///    the operator to re-run with `--yes` or `--with-ppd`.
fn handle_ppd_conflict(opts: &InstallOpts, out: &mut Vec<ChangeRecord>) -> Result<()> {
    if !ppd_present(&opts.ppd_unit_paths) {
        return Ok(());
    }
    if opts.with_ppd {
        out.push(ChangeRecord::Warning(
            "PPD kept active; sy power shim will NOT bind net.hadess.PowerProfiles to avoid the bus-name fight".to_string(),
        ));
        return Ok(());
    }
    if !opts.yes {
        out.push(ChangeRecord::Warning(
            "power-profiles-daemon detected. Re-run with --yes to mask it, or --with-ppd to keep both running.".to_string(),
        ));
        return Ok(());
    }
    install_ppd_mask(opts, out)
}

/// True iff at least one of the candidate PPD-unit paths exists.
fn ppd_present(candidates: &[PathBuf]) -> bool {
    candidates.iter().any(|p| p.exists())
}

/// `systemctl --user mask power-profiles-daemon.service` semantics:
/// the unit becomes a symlink to `/dev/null` under
/// `<user_unit_root>/`. We track the mask symlink to keep re-apply
/// idempotent — the symlink's presence + `/dev/null` target is the
/// "already masked" signal; we only shell out when the link is
/// missing.
fn install_ppd_mask(opts: &InstallOpts, out: &mut Vec<ChangeRecord>) -> Result<()> {
    let mask_link = opts.user_unit_root.join(PPD_UNIT_BASENAME);
    if is_ppd_masked(&mask_link) {
        out.push(ChangeRecord::AlreadyMatches(mask_link));
        return Ok(());
    }
    out.push(ChangeRecord::Updated(mask_link));
    if opts.dry_run {
        return Ok(());
    }
    opts.command_runner
        .run("systemctl", &["--user", "mask", PPD_UNIT_BASENAME])
        .with_context(|| {
            format!("systemctl --user mask {PPD_UNIT_BASENAME} (PPD-replacement install)")
        })?;
    Ok(())
}

/// True iff `mask_link` is a symlink whose target is `/dev/null` — the
/// well-known shape `systemctl --user mask` produces. Anything else
/// (regular file, broken link, missing link) means the mask isn't in
/// effect and we need to (re-)invoke systemctl.
fn is_ppd_masked(mask_link: &Path) -> bool {
    match fs::read_link(mask_link) {
        Ok(target) => target == Path::new("/dev/null"),
        Err(_) => false,
    }
}

/// Idempotent + cheap, but we still gate behind `run_daemon_reload`
/// so `make test` doesn't shell out to systemctl on CI / dev hosts
/// without a user manager.
fn run_daemon_reload(out: &mut Vec<ChangeRecord>) -> Result<()> {
    let status = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()
        .context("spawn systemctl --user daemon-reload")?;
    if !status.success() {
        return Err(anyhow::anyhow!(
            "systemctl --user daemon-reload exited with {status}"
        ));
    }
    out.push(ChangeRecord::SystemdReload);
    Ok(())
}

/// Human pretty-print for one record. The CLI handler maps the whole
/// `Vec<ChangeRecord>` through this so the dry-run and the
/// commit-with-output paths share formatting (CLIG: same shape
/// human-side regardless of mode).
pub fn format_record(rec: &ChangeRecord) -> String {
    match rec {
        ChangeRecord::Created(p) => format!("+ {}", p.display()),
        ChangeRecord::Updated(p) => format!("~ {}", p.display()),
        ChangeRecord::AlreadyMatches(p) => format!("= {}", p.display()),
        ChangeRecord::DirectoryCreated(p) => format!("d {}/", p.display()),
        ChangeRecord::SystemdReload => "  systemctl --user daemon-reload".to_string(),
        ChangeRecord::TmpfilesApplied => "  systemd-tmpfiles --create".to_string(),
        ChangeRecord::KernelCmdlineUpdated { method } => match method {
            GrubbyOrDropIn::Grubby => {
                "  grubby --update-kernel=ALL --args=amd_dynamic_epp=disable".to_string()
            }
            GrubbyOrDropIn::GrubD => {
                "  kernel cmdline updated via /etc/default/grub.d drop-in".to_string()
            }
        },
        ChangeRecord::Warning(msg) => format!("! {msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Test double for [`CommandRunner`]: records every shell-out so
    /// the idempotency tests can assert `grub2-mkconfig` ran exactly
    /// once across a first-then-reapply pair.
    #[derive(Default)]
    struct MockRunner {
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl MockRunner {
        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().map(|g| g.clone()).unwrap_or_default()
        }
    }

    impl CommandRunner for MockRunner {
        fn run(&self, cmd: &str, args: &[&str]) -> Result<()> {
            let mut v = vec![cmd.to_string()];
            v.extend(args.iter().map(|s| s.to_string()));
            self.calls
                .lock()
                .map_err(|e| anyhow::anyhow!("mock runner poisoned: {e}"))?
                .push(v);
            Ok(())
        }
    }

    /// Build an `InstallOpts` pointing every root at `td`'s subpaths.
    /// `daemon_reload=false` keeps `make test` hermetic; no systemctl
    /// invocation, no `/etc` writes. `command_runner` defaults to a
    /// no-op runner so the grub step doesn't shell out under test.
    fn opts_for(td: &TempDir, dry_run: bool) -> InstallOpts {
        InstallOpts {
            dry_run,
            state_root: td.path().join("state"),
            user_unit_root: td.path().join("config/systemd/user"),
            polkit_root: td.path().join("polkit"),
            grub_root: td.path().join("grub.d"),
            grub_cfg_file: td.path().join("default-grub"),
            dbus_root: td.path().join("dbus.d"),
            tmpfiles_root: td.path().join("tmpfiles.d"),
            system_unit_root: td.path().join("systemd/system"),
            udev_rules_root: td.path().join("udev/rules.d"),
            command_runner: Box::new(MockRunner::default()),
            run_daemon_reload: false,
            yes: false,
            with_ppd: false,
            ppd_unit_paths: Vec::new(),
            // Default tests to the drop-in branch — that's what every
            // existing pre-P3-1 assertion expects. P3-1-specific tests
            // override to `|| true` to exercise the grubby branch.
            grubby_detect: Box::new(|| false),
        }
    }

    /// Step 13 required test: `--dry-run` lists every planned action
    /// but writes nothing to disk. We check both the record shape
    /// (Created + DirectoryCreated, no AlreadyMatches because we
    /// started empty) and the empty filesystem after the call.
    #[test]
    fn dry_run_writes_nothing() {
        let td = TempDir::new().expect("tempdir");
        let opts = opts_for(&td, /* dry_run = */ true);

        let records = install(&opts).expect("dry-run install");

        assert!(
            records
                .iter()
                .any(|r| matches!(r, ChangeRecord::DirectoryCreated(p) if p.ends_with("power"))),
            "expected DirectoryCreated(<state>/power) in dry-run records, got {records:?}",
        );
        assert!(
            records.iter().any(|r| matches!(
                r,
                ChangeRecord::Created(p) if p.ends_with("sy-powerd.service")
            )),
            "expected Created(sy-powerd.service) in dry-run records, got {records:?}",
        );
        assert!(
            records.iter().any(|r| matches!(
                r,
                ChangeRecord::Created(p) if p.ends_with("10-sy-power.rules")
            )),
            "expected Created(10-sy-power.rules) in dry-run records, got {records:?}",
        );
        assert!(
            !records
                .iter()
                .any(|r| matches!(r, ChangeRecord::SystemdReload)),
            "dry-run must not run daemon-reload, got {records:?}",
        );

        // No files touched.
        assert!(!opts.state_root.join(TELEMETRY_SUBDIR).exists());
        assert!(!opts.user_unit_root.join(UNIT_BASENAME).exists());
        assert!(!opts.polkit_root.join(POLKIT_BASENAME).exists());
    }

    /// Step 13 required test: re-running `install(false)` against an
    /// already-installed tempdir is a no-op — every file record is
    /// `AlreadyMatches` (or a Warning, which is informational, not
    /// destructive).
    #[test]
    fn reapply_is_noop() {
        let td = TempDir::new().expect("tempdir");
        let opts = opts_for(&td, /* dry_run = */ false);

        let first = install(&opts).expect("first install");
        // The first run materialises everything.
        assert!(opts.state_root.join(TELEMETRY_SUBDIR).is_dir());
        assert!(opts.user_unit_root.join(UNIT_BASENAME).is_file());
        assert!(opts.polkit_root.join(POLKIT_BASENAME).is_file());
        assert!(first.iter().any(|r| matches!(r, ChangeRecord::Created(_))));

        let second = install(&opts).expect("second install");
        // No destructive variants — only AlreadyMatches and Warnings.
        for rec in &second {
            assert!(
                matches!(
                    rec,
                    ChangeRecord::AlreadyMatches(_) | ChangeRecord::Warning(_)
                ),
                "re-apply produced non-idempotent record {rec:?}",
            );
        }
    }

    /// Step 13 contract preserved through Step 37: with a PPD unit on
    /// disk + no `--yes`/`--with-ppd`, the installer emits an advisory
    /// Warning telling the operator how to opt in. No destructive
    /// records, no shell-outs. We use a synthetic PPD path so the test
    /// is deterministic regardless of the host.
    #[test]
    fn detects_existing_ppd() {
        let td = TempDir::new().expect("tempdir");
        let fake = seed_fake_ppd(&td, "lib/systemd/system/power-profiles-daemon.service");
        let runner = std::sync::Arc::new(MockRunner::default());
        let opts = InstallOpts {
            dry_run: false,
            state_root: td.path().join("state"),
            user_unit_root: td.path().join("config/systemd/user"),
            polkit_root: td.path().join("polkit"),
            grub_root: td.path().join("grub.d"),
            grub_cfg_file: td.path().join("default-grub"),
            dbus_root: td.path().join("dbus.d"),
            tmpfiles_root: td.path().join("tmpfiles.d"),
            system_unit_root: td.path().join("systemd/system"),
            udev_rules_root: td.path().join("udev/rules.d"),
            command_runner: Box::new(SharedRunner(runner.clone())),
            run_daemon_reload: false,
            yes: false,
            with_ppd: false,
            ppd_unit_paths: vec![fake],
            grubby_detect: Box::new(|| false),
        };

        let records = install(&opts).expect("install must not fail when PPD is just-detected");

        assert!(
            records.iter().any(|r| matches!(
                r,
                ChangeRecord::Warning(msg)
                    if msg.contains("power-profiles-daemon")
                        && msg.contains("--yes")
                        && msg.contains("--with-ppd")
            )),
            "expected advisory PPD Warning, got {records:?}",
        );
        let mask_link = opts.user_unit_root.join(PPD_UNIT_BASENAME);
        assert!(
            !records.iter().any(|r| matches!(
                r,
                ChangeRecord::Updated(p) | ChangeRecord::Created(p) if p == &mask_link
            )),
            "advisory path must not write the mask: {records:?}",
        );
        // P3-2: cpufreq install legitimately shells out to `systemctl
        // daemon-reload` + `systemctl enable --now …`. The advisory PPD
        // path must not shell out `systemctl mask` for PPD though.
        assert!(
            !runner
                .calls()
                .iter()
                .any(|call| call.iter().any(|a| a == "mask")),
            "advisory PPD path must not shell out `systemctl mask`: {:?}",
            runner.calls(),
        );
    }

    /// The PPD-advisory warning is the only string the CLI parses to
    /// route operators between `--yes` and `--with-ppd`. Lock its
    /// shape — both flag names must appear verbatim and the warning
    /// must render with the `! ` prefix used by `sy power apply` text
    /// output.
    #[test]
    fn ppd_warning_shape_is_stable() {
        let rec = ChangeRecord::Warning(
            "power-profiles-daemon detected. Re-run with --yes to mask it, or --with-ppd to keep both running.".to_string(),
        );
        let line = format_record(&rec);
        assert!(line.starts_with("! "), "warning line shape: {line:?}");
        assert!(line.contains("--yes"), "warning mentions --yes: {line:?}");
        assert!(
            line.contains("--with-ppd"),
            "warning mentions --with-ppd: {line:?}"
        );
    }

    /// `touched_filesystem` returns true only when at least one
    /// destructive variant is in the record set; without that gate
    /// the production `install` would shell out to `daemon-reload`
    /// on every re-apply, breaking the "second call is zero
    /// changes" idempotency DoD.
    #[test]
    fn touched_filesystem_separates_writes_from_noops() {
        let writes = vec![ChangeRecord::Created(PathBuf::from("/x"))];
        let noops = vec![
            ChangeRecord::AlreadyMatches(PathBuf::from("/x")),
            ChangeRecord::Warning("anything".into()),
        ];
        assert!(touched_filesystem(&writes));
        assert!(!touched_filesystem(&noops));
        assert!(!touched_filesystem(&[]));
    }

    /// Step H4: when the polkit dest is unwritable AND already exists
    /// with content matching `POLKIT_RULE`, the installer must emit
    /// `AlreadyMatches` (not the misleading `Warning` from Step 13).
    /// Simulates real-host shape: file mode 0644 inside a 0o555 dir —
    /// the dir blocks writes/creates but lets the user read the file.
    #[test]
    fn polkit_already_matches_when_content_equal_but_dest_unwritable() {
        let td = TempDir::new().expect("tempdir");
        let polkit_root = td.path().join("polkit");
        std::fs::create_dir_all(&polkit_root).expect("mkdir polkit dir");
        let dest = polkit_root.join(POLKIT_BASENAME);
        std::fs::write(&dest, POLKIT_RULE).expect("seed matching rule");
        // Read+execute only — blocks new writes/creates inside.
        let ro_perm = std::fs::Permissions::from_mode(0o555);
        std::fs::set_permissions(&polkit_root, ro_perm).expect("chmod 0555 polkit dir");

        let opts = InstallOpts {
            dry_run: false,
            state_root: td.path().join("state"),
            user_unit_root: td.path().join("config/systemd/user"),
            polkit_root: polkit_root.clone(),
            grub_root: td.path().join("grub.d"),
            grub_cfg_file: td.path().join("default-grub"),
            dbus_root: td.path().join("dbus.d"),
            tmpfiles_root: td.path().join("tmpfiles.d"),
            system_unit_root: td.path().join("systemd/system"),
            udev_rules_root: td.path().join("udev/rules.d"),
            command_runner: Box::new(MockRunner::default()),
            run_daemon_reload: false,
            yes: false,
            with_ppd: false,
            ppd_unit_paths: Vec::new(),
            grubby_detect: Box::new(|| false),
        };

        let records = install(&opts).expect("install must succeed when dest matches");

        // Restore write perm so TempDir::drop can clean up.
        let _ = std::fs::set_permissions(&polkit_root, std::fs::Permissions::from_mode(0o755));

        assert!(
            records.iter().any(|r| matches!(
                r,
                ChangeRecord::AlreadyMatches(p) if p == &dest
            )),
            "expected AlreadyMatches for content-equal polkit dest, got {records:?}",
        );
        assert!(
            !records.iter().any(|r| matches!(
                r,
                ChangeRecord::Warning(msg) if msg.contains("polkit destination")
            )),
            "must NOT emit polkit unwritable warning when content matches: {records:?}",
        );
    }

    /// Step H5: a clean install must drop `99-sy-power.conf` at
    /// `<dbus_root>/`. Mirrors `dry_run_writes_nothing` /
    /// `reapply_is_noop` shape — first install Created + file exists
    /// with the embedded payload.
    #[test]
    fn installs_dbus_policy_dropin() {
        let td = TempDir::new().expect("tempdir");
        let opts = opts_for(&td, /* dry_run = */ false);

        let records = install(&opts).expect("install");

        let dest = opts.dbus_root.join(DBUS_POLICY_BASENAME);
        assert!(
            records.iter().any(|r| matches!(
                r,
                ChangeRecord::Created(p) if p == &dest
            )),
            "expected Created(99-sy-power.conf), got {records:?}",
        );
        let written = std::fs::read_to_string(&dest).expect("read installed policy");
        assert_eq!(
            written, DBUS_POLICY,
            "installed dbus policy must match embedded include_str! payload",
        );
    }

    /// Step H5 (H4-style fallback): when the dbus dest is unwritable
    /// AND already exists with content matching `DBUS_POLICY`, surface
    /// `AlreadyMatches` instead of the misleading "unwritable" Warning.
    /// Real-host shape: file mode 0644 inside a 0o555 dir — the dir
    /// blocks writes/creates but lets the user read the file.
    #[test]
    fn dbus_policy_already_matches_when_dest_readonly_but_content_equal() {
        let td = TempDir::new().expect("tempdir");
        let dbus_root = td.path().join("dbus.d");
        std::fs::create_dir_all(&dbus_root).expect("mkdir dbus dir");
        let dest = dbus_root.join(DBUS_POLICY_BASENAME);
        std::fs::write(&dest, DBUS_POLICY).expect("seed matching policy");
        let ro_perm = std::fs::Permissions::from_mode(0o555);
        std::fs::set_permissions(&dbus_root, ro_perm).expect("chmod 0555 dbus dir");

        let opts = InstallOpts {
            dry_run: false,
            state_root: td.path().join("state"),
            user_unit_root: td.path().join("config/systemd/user"),
            polkit_root: td.path().join("polkit"),
            grub_root: td.path().join("grub.d"),
            grub_cfg_file: td.path().join("default-grub"),
            dbus_root: dbus_root.clone(),
            tmpfiles_root: td.path().join("tmpfiles.d"),
            system_unit_root: td.path().join("systemd/system"),
            udev_rules_root: td.path().join("udev/rules.d"),
            command_runner: Box::new(MockRunner::default()),
            run_daemon_reload: false,
            yes: false,
            with_ppd: false,
            ppd_unit_paths: Vec::new(),
            grubby_detect: Box::new(|| false),
        };

        let records = install(&opts).expect("install must succeed when dest matches");

        let _ = std::fs::set_permissions(&dbus_root, std::fs::Permissions::from_mode(0o755));

        assert!(
            records.iter().any(|r| matches!(
                r,
                ChangeRecord::AlreadyMatches(p) if p == &dest
            )),
            "expected AlreadyMatches for content-equal dbus dest, got {records:?}",
        );
        assert!(
            !records.iter().any(|r| matches!(
                r,
                ChangeRecord::Warning(msg) if msg.contains("dbus policy destination")
            )),
            "must NOT emit dbus unwritable warning when content matches: {records:?}",
        );
    }

    /// Step H6: a clean install must drop `sy-power.conf` at
    /// `<tmpfiles_root>/` AND invoke `systemd-tmpfiles --create
    /// <dest>` so the sysfs perm overrides land immediately without
    /// reboot. Mirrors the Step H5 dbus shape (Created record + file
    /// exists with the embedded payload) plus the additional shell-out
    /// assertion against a shared `MockRunner`.
    #[test]
    fn installs_tmpfiles_dropin() {
        let td = TempDir::new().expect("tempdir");
        let (opts, runner) = opts_with_shared_runner(&td);

        let records = install(&opts).expect("install");

        let dest = opts.tmpfiles_root.join(TMPFILES_BASENAME);
        assert!(
            records.iter().any(|r| matches!(
                r,
                ChangeRecord::Created(p) if p == &dest
            )),
            "expected Created(sy-power.conf), got {records:?}",
        );
        let written = std::fs::read_to_string(&dest).expect("read installed tmpfiles drop-in");
        assert_eq!(
            written, TMPFILES_CONF,
            "installed tmpfiles drop-in must match embedded include_str! payload",
        );
        assert!(
            records
                .iter()
                .any(|r| matches!(r, ChangeRecord::TmpfilesApplied)),
            "expected TmpfilesApplied record after successful write, got {records:?}",
        );
        assert!(
            runner
                .calls()
                .iter()
                .any(
                    |call| call.first().map(String::as_str) == Some("systemd-tmpfiles")
                        && call.iter().any(|a| a == "--create")
                        && call.iter().any(|a| a == dest.to_string_lossy().as_ref())
                ),
            "expected `systemd-tmpfiles --create <dest>`, got {:?}",
            runner.calls(),
        );
    }

    /// Step H6 (H4-style fallback): when the tmpfiles dest is unwritable
    /// AND already exists with content matching `TMPFILES_CONF`, surface
    /// `AlreadyMatches` instead of the misleading "unwritable" Warning.
    /// Mirrors the H5 dbus content-diff fallback verbatim. The runner
    /// must NOT be invoked — `systemd-tmpfiles --create` only fires on
    /// a freshly-written drop-in.
    #[test]
    fn tmpfiles_already_matches_when_dest_readonly_but_content_equal() {
        let td = TempDir::new().expect("tempdir");
        let tmpfiles_root = td.path().join("tmpfiles.d");
        std::fs::create_dir_all(&tmpfiles_root).expect("mkdir tmpfiles dir");
        let dest = tmpfiles_root.join(TMPFILES_BASENAME);
        std::fs::write(&dest, TMPFILES_CONF).expect("seed matching tmpfiles drop-in");
        let ro_perm = std::fs::Permissions::from_mode(0o555);
        std::fs::set_permissions(&tmpfiles_root, ro_perm).expect("chmod 0555 tmpfiles dir");

        let runner = std::sync::Arc::new(MockRunner::default());
        let opts = InstallOpts {
            dry_run: false,
            state_root: td.path().join("state"),
            user_unit_root: td.path().join("config/systemd/user"),
            polkit_root: td.path().join("polkit"),
            grub_root: td.path().join("grub.d"),
            grub_cfg_file: td.path().join("default-grub"),
            dbus_root: td.path().join("dbus.d"),
            tmpfiles_root: tmpfiles_root.clone(),
            system_unit_root: td.path().join("systemd/system"),
            udev_rules_root: td.path().join("udev/rules.d"),
            command_runner: Box::new(SharedRunner(runner.clone())),
            run_daemon_reload: false,
            yes: false,
            with_ppd: false,
            ppd_unit_paths: Vec::new(),
            grubby_detect: Box::new(|| false),
        };

        let records = install(&opts).expect("install must succeed when dest matches");

        let _ = std::fs::set_permissions(&tmpfiles_root, std::fs::Permissions::from_mode(0o755));

        assert!(
            records.iter().any(|r| matches!(
                r,
                ChangeRecord::AlreadyMatches(p) if p == &dest
            )),
            "expected AlreadyMatches for content-equal tmpfiles dest, got {records:?}",
        );
        assert!(
            !records.iter().any(|r| matches!(
                r,
                ChangeRecord::Warning(msg) if msg.contains("tmpfiles destination")
            )),
            "must NOT emit tmpfiles unwritable warning when content matches: {records:?}",
        );
        assert!(
            !runner
                .calls()
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("systemd-tmpfiles")),
            "must NOT run systemd-tmpfiles when nothing was written: {:?}",
            runner.calls(),
        );
    }

    /// Polkit fallback: when the polkit root isn't writable (e.g.
    /// the production `/etc/polkit-1/rules.d/` from an unprivileged
    /// shell) the installer warns instead of failing. We simulate
    /// "unwritable" by pointing `polkit_root` at a regular *file*
    /// (creating a child path then fails with ENOTDIR) — much more
    /// reliable than chmod tricks on a tempdir.
    #[test]
    fn polkit_unwritable_degrades_to_warning() {
        let td = TempDir::new().expect("tempdir");
        let blocker = td.path().join("polkit-is-a-file");
        std::fs::write(&blocker, "not a directory").expect("seed blocker file");
        let opts = InstallOpts {
            dry_run: false,
            state_root: td.path().join("state"),
            user_unit_root: td.path().join("config/systemd/user"),
            polkit_root: blocker,
            grub_root: td.path().join("grub.d"),
            grub_cfg_file: td.path().join("default-grub"),
            dbus_root: td.path().join("dbus.d"),
            tmpfiles_root: td.path().join("tmpfiles.d"),
            system_unit_root: td.path().join("systemd/system"),
            udev_rules_root: td.path().join("udev/rules.d"),
            command_runner: Box::new(MockRunner::default()),
            run_daemon_reload: false,
            yes: false,
            with_ppd: false,
            ppd_unit_paths: Vec::new(),
            grubby_detect: Box::new(|| false),
        };

        let records = install(&opts).expect("install must not fail on unwritable polkit root");
        assert!(
            records.iter().any(|r| matches!(
                r,
                ChangeRecord::Warning(msg) if msg.contains("polkit destination")
            )),
            "expected polkit unwritable warning, got {records:?}",
        );
    }

    /// Wrap a `MockRunner` in `Arc` so the test owns a handle to
    /// inspect call counts while the actuator owns the boxed trait
    /// object (mirrors the `apply::npu` pattern verbatim).
    struct SharedRunner(std::sync::Arc<MockRunner>);
    impl CommandRunner for SharedRunner {
        fn run(&self, cmd: &str, args: &[&str]) -> Result<()> {
            self.0.run(cmd, args)
        }
    }

    /// Build an `InstallOpts` whose grub `command_runner` is a shared
    /// mock. Returns both the opts and the runner handle so tests can
    /// assert exactly how many shell-outs the install drove.
    fn opts_with_shared_runner(td: &TempDir) -> (InstallOpts, std::sync::Arc<MockRunner>) {
        let runner = std::sync::Arc::new(MockRunner::default());
        let opts = InstallOpts {
            dry_run: false,
            state_root: td.path().join("state"),
            user_unit_root: td.path().join("config/systemd/user"),
            polkit_root: td.path().join("polkit"),
            grub_root: td.path().join("grub.d"),
            grub_cfg_file: td.path().join("default-grub"),
            dbus_root: td.path().join("dbus.d"),
            tmpfiles_root: td.path().join("tmpfiles.d"),
            system_unit_root: td.path().join("systemd/system"),
            udev_rules_root: td.path().join("udev/rules.d"),
            command_runner: Box::new(SharedRunner(runner.clone())),
            run_daemon_reload: false,
            yes: false,
            with_ppd: false,
            ppd_unit_paths: Vec::new(),
            grubby_detect: Box::new(|| false),
        };
        (opts, runner)
    }

    /// Step 27 required: second `sy power apply` against an already-
    /// installed tempdir surfaces the grub drop-in as `AlreadyMatches`
    /// and does NOT re-invoke `grub2-mkconfig` — the expensive
    /// generator only runs when the drop-in was newly written.
    #[test]
    fn grub_dropin_idempotent() {
        let td = TempDir::new().expect("tempdir");
        let (opts, runner) = opts_with_shared_runner(&td);
        let first = install(&opts).expect("first install");
        let dropin = opts.grub_root.join(GRUB_DROPIN_BASENAME);
        assert!(
            first.iter().any(|r| matches!(
                r,
                ChangeRecord::Created(p) if p == &dropin
            )),
            "first install must Create the drop-in, got {first:?}",
        );
        assert_eq!(
            grub_call_count(&runner),
            1,
            "first install must shell out exactly once to regenerate grub.cfg: {:?}",
            runner.calls(),
        );

        let second = install(&opts).expect("re-apply");
        assert!(
            second.iter().any(|r| matches!(
                r,
                ChangeRecord::AlreadyMatches(p) if p == &dropin
            )),
            "re-apply must surface AlreadyMatches for the drop-in, got {second:?}",
        );
        assert_eq!(
            grub_call_count(&runner),
            1,
            "re-apply must NOT re-run grub2-mkconfig: {:?}",
            runner.calls(),
        );
    }

    /// Count of `grub2-mkconfig` / `update-grub` shell-outs the runner
    /// has recorded so far. H6 introduced a `systemd-tmpfiles --create`
    /// invocation on every successful install, so tests that gate on
    /// "exactly one grub regeneration" must filter the call list by
    /// command name instead of counting all calls.
    fn grub_call_count(runner: &std::sync::Arc<MockRunner>) -> usize {
        runner
            .calls()
            .iter()
            .filter(|c| {
                matches!(
                    c.first().map(String::as_str),
                    Some("grub2-mkconfig") | Some("update-grub"),
                )
            })
            .count()
    }

    /// Seed a fake PPD unit at `td/<rel>` so `ppd_unit_paths` detection
    /// triggers without touching `/usr/lib`. Returns the absolute path
    /// the test must pass through `InstallOpts::ppd_unit_paths`.
    fn seed_fake_ppd(td: &TempDir, rel: &str) -> PathBuf {
        let path = td.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir fake-ppd parent");
        }
        std::fs::write(&path, "[Unit]\nDescription=fake PPD\n").expect("seed fake PPD unit");
        path
    }

    /// Step 37 required: with PPD detected + `yes=true` + no
    /// `--with-ppd`, the installer masks PPD via
    /// `systemctl --user mask power-profiles-daemon.service` and emits
    /// `Updated(<mask symlink path>)`. The mock runner records the
    /// invocation so we never shell out for real.
    #[test]
    fn masks_ppd_when_yes_set() {
        let td = TempDir::new().expect("tempdir");
        let fake = seed_fake_ppd(&td, "lib/systemd/system/power-profiles-daemon.service");
        let runner = std::sync::Arc::new(MockRunner::default());
        let opts = InstallOpts {
            dry_run: false,
            state_root: td.path().join("state"),
            user_unit_root: td.path().join("config/systemd/user"),
            polkit_root: td.path().join("polkit"),
            grub_root: td.path().join("grub.d"),
            grub_cfg_file: td.path().join("default-grub"),
            dbus_root: td.path().join("dbus.d"),
            tmpfiles_root: td.path().join("tmpfiles.d"),
            system_unit_root: td.path().join("systemd/system"),
            udev_rules_root: td.path().join("udev/rules.d"),
            command_runner: Box::new(SharedRunner(runner.clone())),
            run_daemon_reload: false,
            yes: true,
            with_ppd: false,
            ppd_unit_paths: vec![fake],
            grubby_detect: Box::new(|| false),
        };

        let records = install(&opts).expect("install with --yes + PPD detected");

        let mask_link = opts.user_unit_root.join(PPD_UNIT_BASENAME);
        assert!(
            records.iter().any(|r| matches!(
                r,
                ChangeRecord::Updated(p) if p == &mask_link
            )),
            "expected Updated(<mask symlink>), got {records:?}",
        );
        assert!(
            runner
                .calls()
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("systemctl")
                    && call.iter().any(|a| a == "mask")
                    && call.iter().any(|a| a == PPD_UNIT_BASENAME)),
            "expected `systemctl --user mask power-profiles-daemon.service`, got {:?}",
            runner.calls(),
        );
    }

    /// `CommandRunner` that mirrors what `systemctl --user mask` does
    /// on a real system — drop a symlink to `/dev/null` under
    /// `user_unit_root`. Lets the idempotency test exercise the
    /// already-masked branch without spawning systemctl.
    struct MaskingRunner {
        inner: std::sync::Arc<MockRunner>,
        user_unit_root: PathBuf,
    }

    impl CommandRunner for MaskingRunner {
        fn run(&self, cmd: &str, args: &[&str]) -> Result<()> {
            self.inner.run(cmd, args)?;
            if cmd == "systemctl" && args.contains(&"mask") {
                std::fs::create_dir_all(&self.user_unit_root)
                    .context("MaskingRunner: ensure user_unit_root exists")?;
                let link = self.user_unit_root.join(PPD_UNIT_BASENAME);
                let _ = std::fs::remove_file(&link);
                std::os::unix::fs::symlink("/dev/null", &link)
                    .context("MaskingRunner: install PPD mask symlink")?;
            }
            Ok(())
        }
    }

    /// Step 37 required: a second `sy power apply --yes` against a
    /// host where PPD is already masked surfaces the mask as
    /// `AlreadyMatches` and does NOT re-invoke systemctl. Mirrors the
    /// Step 27 `grub_dropin_idempotent` contract for the PPD path.
    #[test]
    fn idempotent_after_apply() {
        let td = TempDir::new().expect("tempdir");
        let fake = seed_fake_ppd(&td, "lib/systemd/system/power-profiles-daemon.service");
        let runner = std::sync::Arc::new(MockRunner::default());
        let user_unit_root = td.path().join("config/systemd/user");
        let make_opts = || InstallOpts {
            dry_run: false,
            state_root: td.path().join("state"),
            user_unit_root: user_unit_root.clone(),
            polkit_root: td.path().join("polkit"),
            grub_root: td.path().join("grub.d"),
            grub_cfg_file: td.path().join("default-grub"),
            dbus_root: td.path().join("dbus.d"),
            tmpfiles_root: td.path().join("tmpfiles.d"),
            system_unit_root: td.path().join("systemd/system"),
            udev_rules_root: td.path().join("udev/rules.d"),
            command_runner: Box::new(MaskingRunner {
                inner: runner.clone(),
                user_unit_root: user_unit_root.clone(),
            }),
            run_daemon_reload: false,
            yes: true,
            with_ppd: false,
            ppd_unit_paths: vec![fake.clone()],
            grubby_detect: Box::new(|| false),
        };

        let first = install(&make_opts()).expect("first install masks PPD");
        let mask_link = user_unit_root.join(PPD_UNIT_BASENAME);
        assert!(
            first.iter().any(|r| matches!(
                r,
                ChangeRecord::Updated(p) if p == &mask_link
            )),
            "first install must Update the mask, got {first:?}",
        );
        let mask_calls = runner
            .calls()
            .iter()
            .filter(|c| c.iter().any(|a| a == "mask"))
            .count();
        assert_eq!(
            mask_calls,
            1,
            "first install must shell out `systemctl mask` exactly once: {:?}",
            runner.calls(),
        );

        let second = install(&make_opts()).expect("re-apply is noop");
        assert!(
            second.iter().any(|r| matches!(
                r,
                ChangeRecord::AlreadyMatches(p) if p == &mask_link
            )),
            "re-apply must surface AlreadyMatches for the mask, got {second:?}",
        );
        let mask_calls_after = runner
            .calls()
            .iter()
            .filter(|c| c.iter().any(|a| a == "mask"))
            .count();
        assert_eq!(
            mask_calls_after,
            1,
            "re-apply must NOT re-invoke systemctl mask: {:?}",
            runner.calls(),
        );
    }

    /// Step 37 required: with `--with-ppd` set the installer emits a
    /// Warning explaining the side-by-side decision but does NOT mask
    /// PPD, regardless of whether `--yes` was also passed.
    #[test]
    fn keeps_ppd_when_with_ppd_set() {
        let td = TempDir::new().expect("tempdir");
        let fake = seed_fake_ppd(&td, "lib/systemd/system/power-profiles-daemon.service");
        let runner = std::sync::Arc::new(MockRunner::default());
        let opts = InstallOpts {
            dry_run: false,
            state_root: td.path().join("state"),
            user_unit_root: td.path().join("config/systemd/user"),
            polkit_root: td.path().join("polkit"),
            grub_root: td.path().join("grub.d"),
            grub_cfg_file: td.path().join("default-grub"),
            dbus_root: td.path().join("dbus.d"),
            tmpfiles_root: td.path().join("tmpfiles.d"),
            system_unit_root: td.path().join("systemd/system"),
            udev_rules_root: td.path().join("udev/rules.d"),
            command_runner: Box::new(SharedRunner(runner.clone())),
            run_daemon_reload: false,
            // `--yes` AND `--with-ppd`: `--with-ppd` wins, no mask.
            yes: true,
            with_ppd: true,
            ppd_unit_paths: vec![fake],
            grubby_detect: Box::new(|| false),
        };

        let records = install(&opts).expect("install with --with-ppd");

        assert!(
            records.iter().any(|r| matches!(
                r,
                ChangeRecord::Warning(msg)
                    if msg.contains("PPD kept active")
                        && msg.contains("net.hadess.PowerProfiles")
            )),
            "expected with-ppd Warning, got {records:?}",
        );
        let mask_link = opts.user_unit_root.join(PPD_UNIT_BASENAME);
        assert!(
            !records.iter().any(|r| matches!(
                r,
                ChangeRecord::Updated(p) | ChangeRecord::Created(p) if p == &mask_link
            )),
            "with-ppd must not emit a mask record: {records:?}",
        );
        assert!(
            !runner
                .calls()
                .iter()
                .any(|call| call.iter().any(|a| a == "mask")),
            "with-ppd must not shell out `systemctl mask`: {:?}",
            runner.calls(),
        );
    }

    /// Step 27 required: when `/etc/default/grub` already pins
    /// `amd_dynamic_epp=enable` (operator opted in explicitly), the
    /// installer refuses to silently override it. It emits a clear
    /// Warning, skips the drop-in install, and does NOT shell out.
    #[test]
    fn grub_dropin_warns_when_existing_amd_dynamic_epp_enable() {
        let td = TempDir::new().expect("tempdir");
        let (opts, runner) = opts_with_shared_runner(&td);
        std::fs::write(
            &opts.grub_cfg_file,
            "GRUB_TIMEOUT=5\nGRUB_CMDLINE_LINUX=\"quiet amd_dynamic_epp=enable rhgb\"\n",
        )
        .expect("seed grub cfg");

        let records = install(&opts).expect("install must not fail on conflict");
        assert!(
            records.iter().any(|r| matches!(
                r,
                ChangeRecord::Warning(msg)
                    if msg.contains("conflict")
                        && msg.contains("amd_dynamic_epp=enable")
                        && msg.contains("GRUB_CMDLINE_LINUX")
            )),
            "expected conflict warning naming both tokens, got {records:?}",
        );
        let dropin = opts.grub_root.join(GRUB_DROPIN_BASENAME);
        assert!(
            !dropin.exists(),
            "conflict must skip the drop-in write: {dropin:?} exists",
        );
        assert_eq!(
            grub_call_count(&runner),
            0,
            "conflict must skip grub2-mkconfig: {:?}",
            runner.calls(),
        );
    }

    /// Step P3-1: on a Fedora host (grubby present) the installer
    /// MUST invoke `grubby --update-kernel=ALL
    /// --args=amd_dynamic_epp=disable` instead of writing the Debian
    /// drop-in. The drop-in path is silently no-op on Fedora because
    /// `grub2-mkconfig` does not source `/etc/default/grub.d/`.
    #[test]
    fn uses_grubby_when_present() {
        let td = TempDir::new().expect("tempdir");
        let (mut opts, runner) = opts_with_shared_runner(&td);
        opts.grubby_detect = Box::new(|| true);

        let records = install(&opts).expect("install with grubby present");

        assert!(
            records.iter().any(|r| matches!(
                r,
                ChangeRecord::KernelCmdlineUpdated {
                    method: GrubbyOrDropIn::Grubby
                }
            )),
            "expected KernelCmdlineUpdated::Grubby, got {records:?}",
        );
        let saw_grubby_call = runner.calls().iter().any(|call| {
            call.first().map(String::as_str) == Some("grubby")
                && call.iter().any(|a| a == "--update-kernel=ALL")
                && call.iter().any(|a| a == "--args=amd_dynamic_epp=disable")
        });
        assert!(
            saw_grubby_call,
            "expected `grubby --update-kernel=ALL --args=amd_dynamic_epp=disable`, got {:?}",
            runner.calls(),
        );
        let dropin = opts.grub_root.join(GRUB_DROPIN_BASENAME);
        assert!(
            !dropin.exists(),
            "grubby path must NOT write the Debian drop-in: {dropin:?} exists",
        );
        assert_eq!(
            grub_call_count(&runner),
            0,
            "grubby path must NOT shell out to grub2-mkconfig / update-grub: {:?}",
            runner.calls(),
        );
    }

    /// Step P3-1: on a Debian / Arch host (grubby absent) the installer
    /// keeps writing `<grub_root>/10-sy-power.cfg` and regenerating
    /// `grub.cfg` via `grub2-mkconfig` — the original Step 27 contract.
    /// The new `KernelCmdlineUpdated::GrubD` record still fires so the
    /// CLI can render a uniform "kernel cmdline updated" line on both
    /// distros.
    #[test]
    fn falls_back_to_grub_d_when_grubby_absent() {
        let td = TempDir::new().expect("tempdir");
        let (mut opts, runner) = opts_with_shared_runner(&td);
        opts.grubby_detect = Box::new(|| false);

        let records = install(&opts).expect("install with grubby absent");

        let dropin = opts.grub_root.join(GRUB_DROPIN_BASENAME);
        assert!(
            records.iter().any(|r| matches!(
                r,
                ChangeRecord::Created(p) if p == &dropin
            )),
            "expected Created(10-sy-power.cfg) drop-in, got {records:?}",
        );
        assert!(dropin.is_file(), "drop-in must land on disk: {dropin:?}",);
        assert!(
            records.iter().any(|r| matches!(
                r,
                ChangeRecord::KernelCmdlineUpdated {
                    method: GrubbyOrDropIn::GrubD
                }
            )),
            "expected KernelCmdlineUpdated::GrubD, got {records:?}",
        );
        assert_eq!(
            grub_call_count(&runner),
            1,
            "grub.d path must shell out to grub2-mkconfig exactly once: {:?}",
            runner.calls(),
        );
        let saw_grubby_call = runner
            .calls()
            .iter()
            .any(|call| call.first().map(String::as_str) == Some("grubby"));
        assert!(
            !saw_grubby_call,
            "grubby-absent branch must NOT invoke `grubby`: {:?}",
            runner.calls(),
        );
    }

    /// Step P3-2: a clean install must drop `sy-power-cpufreq.service`
    /// at `<system_unit_root>/` AND invoke `systemctl daemon-reload` +
    /// `systemctl enable --now sy-power-cpufreq.service` so the
    /// amd-pstate=active + scaling_governor=powersave writes happen
    /// immediately AND persist across reboot. Mirrors the H6 tmpfiles
    /// shape (Created record + file matches embedded payload + targeted
    /// shell-out assertions against a shared `MockRunner`).
    #[test]
    fn installs_cpufreq_oneshot() {
        let td = TempDir::new().expect("tempdir");
        let (opts, runner) = opts_with_shared_runner(&td);

        let records = install(&opts).expect("install");

        let dest = opts.system_unit_root.join(CPUFREQ_ONESHOT_BASENAME);
        assert!(
            records.iter().any(|r| matches!(
                r,
                ChangeRecord::Created(p) if p == &dest
            )),
            "expected Created(sy-power-cpufreq.service), got {records:?}",
        );
        let written = std::fs::read_to_string(&dest).expect("read installed cpufreq unit");
        assert_eq!(
            written, CPUFREQ_ONESHOT_UNIT,
            "installed cpufreq unit must match embedded include_str! payload",
        );
        let saw_daemon_reload = runner.calls().iter().any(|call| {
            call.first().map(String::as_str) == Some("systemctl")
                && call.iter().any(|a| a == "daemon-reload")
        });
        assert!(
            saw_daemon_reload,
            "expected `systemctl daemon-reload`, got {:?}",
            runner.calls(),
        );
        let saw_enable_now = runner.calls().iter().any(|call| {
            call.first().map(String::as_str) == Some("systemctl")
                && call.iter().any(|a| a == "enable")
                && call.iter().any(|a| a == "--now")
                && call.iter().any(|a| a == CPUFREQ_ONESHOT_BASENAME)
        });
        assert!(
            saw_enable_now,
            "expected `systemctl enable --now {CPUFREQ_ONESHOT_BASENAME}`, got {:?}",
            runner.calls(),
        );
    }

    /// Step P3-2 (H4-style fallback): when the system-unit dest is
    /// unwritable AND already exists with content matching
    /// `CPUFREQ_ONESHOT_UNIT`, surface `AlreadyMatches` instead of the
    /// misleading "unwritable" Warning. Mirrors the H5/H6 content-diff
    /// fallback. The runner must NOT receive `daemon-reload` /
    /// `enable --now` calls — the unit is already installed.
    #[test]
    fn cpufreq_oneshot_already_matches_when_dest_equal_content() {
        let td = TempDir::new().expect("tempdir");
        let system_unit_root = td.path().join("systemd/system");
        std::fs::create_dir_all(&system_unit_root).expect("mkdir system unit dir");
        let dest = system_unit_root.join(CPUFREQ_ONESHOT_BASENAME);
        std::fs::write(&dest, CPUFREQ_ONESHOT_UNIT).expect("seed matching cpufreq unit");
        let ro_perm = std::fs::Permissions::from_mode(0o555);
        std::fs::set_permissions(&system_unit_root, ro_perm).expect("chmod 0555 system unit dir");

        let runner = std::sync::Arc::new(MockRunner::default());
        let opts = InstallOpts {
            dry_run: false,
            state_root: td.path().join("state"),
            user_unit_root: td.path().join("config/systemd/user"),
            polkit_root: td.path().join("polkit"),
            grub_root: td.path().join("grub.d"),
            grub_cfg_file: td.path().join("default-grub"),
            dbus_root: td.path().join("dbus.d"),
            tmpfiles_root: td.path().join("tmpfiles.d"),
            system_unit_root: system_unit_root.clone(),
            udev_rules_root: td.path().join("udev/rules.d"),
            command_runner: Box::new(SharedRunner(runner.clone())),
            run_daemon_reload: false,
            yes: false,
            with_ppd: false,
            ppd_unit_paths: Vec::new(),
            grubby_detect: Box::new(|| false),
        };

        let records = install(&opts).expect("install must succeed when dest matches");

        // Restore write perm so TempDir::drop can clean up.
        let _ = std::fs::set_permissions(&system_unit_root, std::fs::Permissions::from_mode(0o755));

        assert!(
            records.iter().any(|r| matches!(
                r,
                ChangeRecord::AlreadyMatches(p) if p == &dest
            )),
            "expected AlreadyMatches for content-equal cpufreq dest, got {records:?}",
        );
        assert!(
            !records.iter().any(|r| matches!(
                r,
                ChangeRecord::Warning(msg) if msg.contains("cpufreq oneshot destination")
            )),
            "must NOT emit cpufreq unwritable warning when content matches: {records:?}",
        );
        let cpufreq_call = runner.calls().iter().any(|call| {
            call.first().map(String::as_str) == Some("systemctl")
                && (call.iter().any(|a| a == "daemon-reload")
                    || call.iter().any(|a| a == CPUFREQ_ONESHOT_BASENAME))
        });
        assert!(
            !cpufreq_call,
            "must NOT shell out daemon-reload / enable --now when nothing was written: {:?}",
            runner.calls(),
        );
    }

    /// Step P3-4: a clean install must drop `99-sy-power.rules` at
    /// `<udev_rules_root>/` AND invoke both `udevadm control
    /// --reload-rules` and `udevadm trigger --subsystem-match=drm` so
    /// the new rule takes effect immediately without reboot. The
    /// card-renumber-survival rationale is in the udev rule's header.
    #[test]
    fn installs_udev_rule_and_triggers_reload() {
        let td = TempDir::new().expect("tempdir");
        let (opts, runner) = opts_with_shared_runner(&td);

        let records = install(&opts).expect("install");

        let dest = opts.udev_rules_root.join(UDEV_RULE_BASENAME);
        assert!(
            records.iter().any(|r| matches!(
                r,
                ChangeRecord::Created(p) if p == &dest
            )),
            "expected Created(99-sy-power.rules), got {records:?}",
        );
        let written = std::fs::read_to_string(&dest).expect("read installed udev rule");
        assert_eq!(
            written, UDEV_RULE,
            "installed udev rule must match embedded include_str! payload",
        );
        let saw_reload = runner.calls().iter().any(|call| {
            call.first().map(String::as_str) == Some("udevadm")
                && call.iter().any(|a| a == "control")
                && call.iter().any(|a| a == "--reload-rules")
        });
        assert!(
            saw_reload,
            "expected `udevadm control --reload-rules`, got {:?}",
            runner.calls(),
        );
        let saw_trigger = runner.calls().iter().any(|call| {
            call.first().map(String::as_str) == Some("udevadm")
                && call.iter().any(|a| a == "trigger")
                && call.iter().any(|a| a == "--subsystem-match=drm")
        });
        assert!(
            saw_trigger,
            "expected `udevadm trigger --subsystem-match=drm`, got {:?}",
            runner.calls(),
        );
    }
}
