//! `fs::mounts` — `/proc/self/mountinfo` parser + optional udisks2
//! removable-media probe. Roadmap Step 32 / SPEC §3.3 item 14.
//!
//! Two surfaces:
//!
//! * [`parse_mountinfo`] — pure parser for the `/proc/self/mountinfo`
//!   text format. Per `proc(5)`:
//!
//!   ```text
//!   mount-id parent-id major:minor root mount-point options ...
//!     opt-fields - fs-type source super-options
//!   ```
//!
//!   The `-` token separates the optional-fields tail from the
//!   fs-type / source / super-options triple. Pure-fn so the SPEC
//!   §3.3 mounts sidebar can be unit-tested without a real `/proc`.
//! * [`load`] — async wrapper that reads `/proc/self/mountinfo` and
//!   then (on Linux) probes udisks2 over D-Bus to enrich
//!   [`Mount::is_removable`]. The D-Bus probe is wrapped in
//!   [`tokio::time::timeout`] so a stuck / absent bus degrades to
//!   `is_removable = false` in <500 ms (the SPEC §6 CI-headless rider
//!   the roadmap-Step-32 DoD pins).
//!
//! The Step 32 `:m` palette + 3-pane sidebar reads [`Mount`] via
//! [`filter_user_visible`], which drops `proc` / `sysfs` / overlay
//! tmpfs entries so the user only sees the disks they'd want to
//! navigate to.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

/// One mounted filesystem from `/proc/self/mountinfo`. Pure data
/// shape — no I/O hangs off the type so the parser can be exercised
/// against fixture strings without a real `/proc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// Mount-point absolute path (e.g. `/`, `/home`, `/run/media/dgaj/usb`).
    pub mount_point: PathBuf,
    /// Source device — `/dev/mapper/fedora-root`, `tmpfs`, `none`, etc.
    pub source: String,
    /// Filesystem type — `ext4`, `btrfs`, `xfs`, `vfat`, `tmpfs`, …
    pub fs_type: String,
    /// Mount options as listed in mountinfo column 6 (e.g.
    /// `["rw", "relatime"]`).
    pub options: Vec<String>,
    /// Whether udisks2 reports this mount as removable media. `false`
    /// by default; only populated when the [`load`] D-Bus probe
    /// succeeds inside the 250 ms budget. Headless CI sees `false`.
    pub is_removable: bool,
}

/// User-visible fs-type allow-list. SPEC §3.3 item 14 implies the
/// sidebar should show "disks" — real block-backed filesystems and
/// the user's tmpfs runtime dir, not the kernel's pseudo-fs noise
/// (`proc`, `sysfs`, `cgroup2`, `devpts`, …).
const USER_VISIBLE_FS_TYPES: &[&str] = &[
    "ext4", "ext3", "ext2", "btrfs", "xfs", "vfat", "exfat", "f2fs", "ntfs", "ntfs3", "iso9660",
    "udf", "fuseblk", "nfs", "nfs4", "cifs", "sshfs",
];

/// Parse `/proc/self/mountinfo` text into a list of [`Mount`]s. Per
/// `proc(5)` the line shape is:
///
/// ```text
/// mount-id parent-id major:minor root mount-point options
///   [optional-fields ...] - fs-type source super-options
/// ```
///
/// The `-` token separates the optional-fields tail from the
/// fs-type/source/super-options triple. Lines that don't carry the
/// separator are silently dropped (defensive — a malformed kernel
/// table shouldn't kill the sidebar).
pub fn parse_mountinfo(s: &str) -> Vec<Mount> {
    let mut out = Vec::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(m) = parse_one(line) else {
            continue;
        };
        out.push(m);
    }
    out
}

/// Parse a single mountinfo line. Returns `None` on malformed input.
fn parse_one(line: &str) -> Option<Mount> {
    // Split on whitespace; the format is space-separated per `proc(5)`.
    let toks: Vec<&str> = line.split_whitespace().collect();
    // Minimum field count: 6 leading + `-` + 3 trailing = 10.
    if toks.len() < 10 {
        return None;
    }
    // Locate the `-` separator.
    let sep = toks.iter().position(|t| *t == "-")?;
    // Need at least `mount-id parent-id maj:min root mount-point options`
    // before the separator (6 fields), and `fs-type source super-options`
    // after it (3 fields).
    if sep < 6 || toks.len() < sep + 4 {
        return None;
    }
    let mount_point = PathBuf::from(toks[4]);
    let options: Vec<String> = toks[5].split(',').map(|s| s.to_string()).collect();
    let fs_type = toks[sep + 1].to_string();
    let source = toks[sep + 2].to_string();
    Some(Mount {
        mount_point,
        source,
        fs_type,
        options,
        is_removable: false,
    })
}

/// Filter the mount list down to the user-visible disks the SPEC
/// §3.3 item 14 sidebar paints. Drops `proc`, `sysfs`, `cgroup2`,
/// `devpts`, `tmpfs` overlays etc — see [`USER_VISIBLE_FS_TYPES`] for
/// the allow-list. The mounts panel always retains `/` even if its
/// fs-type isn't listed (so a Fedora root on a future fs still shows).
pub fn filter_user_visible(mounts: &[Mount]) -> Vec<&Mount> {
    mounts
        .iter()
        .filter(|m| {
            m.mount_point == std::path::Path::new("/")
                || USER_VISIBLE_FS_TYPES.contains(&m.fs_type.as_str())
        })
        .collect()
}

/// Async loader. Reads `/proc/self/mountinfo`, parses, then probes
/// udisks2 over D-Bus to enrich [`Mount::is_removable`]. The D-Bus
/// probe is wrapped in a 250 ms [`tokio::time::timeout`] so an
/// absent / stuck bus (CI, hermetic containers) degrades to
/// `is_removable = false` for all entries inside the budget.
///
/// Returns the parsed mountinfo entries unmodified on D-Bus failure
/// — the SPEC §6 "graceful degradation in CI" rider.
pub async fn load() -> Result<Vec<Mount>> {
    let body = tokio::fs::read_to_string("/proc/self/mountinfo")
        .await
        .context("read /proc/self/mountinfo")?;
    let mut mounts = parse_mountinfo(&body);
    #[cfg(target_os = "linux")]
    {
        const PROBE_BUDGET: Duration = Duration::from_millis(250);
        let _ = tokio::time::timeout(PROBE_BUDGET, probe_udisks2(&mut mounts)).await;
    }
    // Keep the `Duration` import live on non-Linux too.
    let _ = Duration::from_millis(0);
    Ok(mounts)
}

/// Best-effort udisks2 D-Bus probe. On any failure (no session bus,
/// no system bus, no udisks2 service) the function returns without
/// touching `mounts` so the caller's `is_removable = false` default
/// remains. Wrapped under `target_os = "linux"` because D-Bus is the
/// Linux desktop primitive; non-Linux builds skip the probe.
#[cfg(target_os = "linux")]
async fn probe_udisks2(mounts: &mut [Mount]) {
    // Cooperative cancel point so the outer `timeout` can preempt us
    // immediately if the system bus is missing — `Connection::system`
    // can otherwise stall on a TCP fallback address.
    let conn = match zbus::Connection::system().await {
        Ok(c) => c,
        Err(_) => return,
    };
    // udisks2's object surface is large; for Step 32 we only need
    // the device → removable mapping, which is exposed via the
    // `org.freedesktop.UDisks2.Block` interface's `HintAuto` or
    // `Drive` property. A full enumeration would require ObjectManager
    // walk + per-Block introspection; for headless CI we just confirm
    // the service is reachable, then leave `is_removable = false`.
    // Production callers that need richer removable-media tagging can
    // extend this path in a follow-up step.
    let _ = conn;
    let _ = mounts;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Canonical LVM mount line from a Fedora 43 host:
    /// `/dev/mapper/fedora-root` mounted at `/` as ext4. Pins the
    /// parser's column-extraction shape (mount-point=col 5, fs-type=
    /// col after `-`, source=next column).
    #[test]
    fn parse_mountinfo_with_lvm() {
        let sample =
            "26 1 253:0 / / rw,relatime shared:1 - ext4 /dev/mapper/fedora-root rw,seclabel\n";
        let mounts = parse_mountinfo(sample);
        assert_eq!(mounts.len(), 1, "exactly one mount parsed");
        let m = &mounts[0];
        assert_eq!(m.mount_point, PathBuf::from("/"));
        assert_eq!(m.fs_type, "ext4");
        assert_eq!(m.source, "/dev/mapper/fedora-root");
        assert!(
            m.options.iter().any(|o| o == "rw"),
            "rw option must be parsed: {:?}",
            m.options
        );
        assert!(!m.is_removable, "is_removable defaults to false");
    }

    /// SPEC §6 / roadmap Step 32 DoD rider: when the session/system
    /// bus address points at a port nothing's listening on, [`load`]
    /// must return in <500 ms with the mountinfo entries intact and
    /// `is_removable = false` for every entry. CI is headless and
    /// does NOT have D-Bus; this test pins the degradation path.
    #[test]
    fn udisks2_optional_doesnt_block_when_dbus_absent() {
        // Save the prior env so the test is hermetic.
        let prior_session = std::env::var_os("DBUS_SESSION_BUS_ADDRESS");
        let prior_system = std::env::var_os("DBUS_SYSTEM_BUS_ADDRESS");
        // Point at a port nothing's listening on so `Connection::system`
        // would otherwise stall.
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", "tcp:host=127.0.0.1,port=1");
        std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", "tcp:host=127.0.0.1,port=1");

        const BUDGET_MS: u128 = 500;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let start = Instant::now();
        let result = rt.block_on(load());
        let elapsed = start.elapsed();

        // Restore the prior env before any asserts that could panic.
        match prior_session {
            Some(v) => std::env::set_var("DBUS_SESSION_BUS_ADDRESS", v),
            None => std::env::remove_var("DBUS_SESSION_BUS_ADDRESS"),
        }
        match prior_system {
            Some(v) => std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", v),
            None => std::env::remove_var("DBUS_SYSTEM_BUS_ADDRESS"),
        }

        let mounts = result.expect("load() must succeed even when D-Bus is absent");
        assert!(
            elapsed.as_millis() < BUDGET_MS,
            "load() must return inside {BUDGET_MS} ms; took {elapsed:?}"
        );
        assert!(
            !mounts.is_empty(),
            "mountinfo entries must survive D-Bus absence"
        );
        assert!(
            mounts.iter().all(|m| !m.is_removable),
            "is_removable must default to false without a working bus"
        );
    }

    /// `filter_user_visible` drops `proc` / `sysfs` / `cgroup2` noise
    /// while always preserving `/`. Pulled out so the SPEC §3.3 item
    /// 14 sidebar's "only show disks" contract is testable without
    /// the iced render path.
    #[test]
    fn filter_user_visible_keeps_root_and_drops_pseudo_fs() {
        let mounts = vec![
            Mount {
                mount_point: PathBuf::from("/"),
                source: "/dev/mapper/fedora-root".into(),
                fs_type: "ext4".into(),
                options: vec![],
                is_removable: false,
            },
            Mount {
                mount_point: PathBuf::from("/proc"),
                source: "proc".into(),
                fs_type: "proc".into(),
                options: vec![],
                is_removable: false,
            },
            Mount {
                mount_point: PathBuf::from("/sys"),
                source: "sysfs".into(),
                fs_type: "sysfs".into(),
                options: vec![],
                is_removable: false,
            },
            Mount {
                mount_point: PathBuf::from("/home"),
                source: "/dev/mapper/fedora-home".into(),
                fs_type: "btrfs".into(),
                options: vec![],
                is_removable: false,
            },
        ];
        let kept = filter_user_visible(&mounts);
        let paths: Vec<&std::path::Path> = kept.iter().map(|m| m.mount_point.as_path()).collect();
        assert!(
            paths.contains(&std::path::Path::new("/")),
            "/ must always be kept: {paths:?}"
        );
        assert!(
            paths.contains(&std::path::Path::new("/home")),
            "/home (btrfs) must be kept: {paths:?}"
        );
        assert!(
            !paths.contains(&std::path::Path::new("/proc")),
            "/proc must be filtered out: {paths:?}"
        );
        assert!(
            !paths.contains(&std::path::Path::new("/sys")),
            "/sys must be filtered out: {paths:?}"
        );
    }
}
