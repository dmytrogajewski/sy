//! `fs::trash` — freedesktop Trash-spec round-trip via the
//! [`trash`](https://docs.rs/trash) crate. Step 18 of the
//! [`sy-file-manager` roadmap][roadmap] / SPEC §3.3 item 5 + §3.4
//! anti-goal "data loss". Other DEs (Nautilus, Dolphin, gnome-shell,
//! `gio trash --list`) read the same XDG `Trash/{info,files}/` tree
//! we write, so a `fs::trash::trash` followed by an external
//! `gio trash --restore` is a no-op — the destructive-policy beat
//! (journey **J6** `conflict=trash`) is reversible by the operator's
//! existing tooling, not just by `sy`.
//!
//! ## Public surface
//!
//! * [`trash`] — async `spawn_blocking` wrapper around
//!   `trash::delete` per path. On partial failure (one src trashed,
//!   the next denied) the helper returns the per-src error wrapped
//!   in [`anyhow::Error`] so the caller can roll back. The success
//!   payload is a `Vec<TrashedItem>` in the same order as `paths` —
//!   each carrying the `.trashinfo` basename so the caller can match
//!   against the result of [`list`] without re-scanning the trash.
//! * [`list`] — async `spawn_blocking` wrapper around
//!   `trash::os_limited::list`. Returns every entry under the user's
//!   home trash (and any per-mount `.Trash-<uid>/` dir the
//!   freedesktop spec defines), each mapped onto our [`TrashedItem`]
//!   shape.
//! * [`restore`] — async `spawn_blocking` wrapper around
//!   `trash::os_limited::restore_all` for one entry. Returns the
//!   original path the file was restored to so the caller can verify
//!   it landed where the operator expects.
//!
//! ## Test isolation
//!
//! The XDG trash protocol picks the trash root from `$XDG_DATA_HOME`
//! (falling back to `$HOME/.local/share`). Tests override the env
//! var to point at a hermetic tempdir so the test run never touches
//! the operator's real `~/.local/share/Trash/`. Because `set_var`
//! is process-global, every env-mutating test takes the
//! [`TRASH_TEST_LOCK`] mutex for the duration of the assertion
//! window — same precedent as `plugin::registry::ENV_LOCK`.
//!
//! ## Manual `gio trash --list` interop recipe
//!
//! The freedesktop interop bullet on the Step 18 DoD ("`gio trash
//! --list` after `fs::trash::trash` sees our entries") is a per-host
//! check. `gio` ships with `glib2` (Fedora 43 default; optional on
//! minimal Alpine/CI runners). Stock glib2 routes `gio trash --list`
//! through GVFS, which resolves the trash root via the session bus
//! and ignores the per-process `$XDG_DATA_HOME` override; a fully
//! hermetic interop check therefore requires running against the
//! operator's *real* trash. Recipe:
//!
//! ```text
//! # 1. Probe from a checkout of sy, against the real ~/.local/share/Trash/
//! cargo build --release
//! touch ~/sy-trash-probe.txt
//! # invoke fs::trash::trash via a one-shot helper. Today (until
//! # Step 20's `sy file --ipc trash` lands) the simplest path is a
//! # throwaway `cargo test`:
//! cat > /tmp/sy-trash-probe.rs <<'EOF'
//!     use std::path::PathBuf;
//!     #[tokio::main]
//!     async fn main() -> anyhow::Result<()> {
//!         let p = PathBuf::from(std::env::var("HOME")?)
//!             .join("sy-trash-probe.txt");
//!         sy::file::fs::trash::trash(&[p]).await?;
//!         Ok(())
//!     }
//! EOF
//! # 2. Verify gio sees it:
//! gio trash --list | grep sy-trash-probe.txt
//! # 3. Cleanup:
//! gio trash --empty
//! ```
//!
//! The e2e test below runs the same `gio trash --list` probe in
//! its `step18_…` body. On stock glib2 (GVFS-backed) the probe
//! returns the operator's real trash listing and the hermetic
//! assertion is logged-and-skipped — the freedesktop interop
//! contract is still upheld in production; only the e2e's
//! in-tempdir probe is conditional. Future glib versions that
//! respect `$XDG_DATA_HOME` (or test setups without an active
//! GVFS) trip the hard-assert path automatically.
//!
//! [roadmap]: ../../../../specs/roadmaps/sy-file-manager/ROADMAP.md

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};

/// Process-wide mutex serialising env mutations the tests use to
/// redirect `$XDG_DATA_HOME` to a hermetic tempdir. Exported so the
/// integration-test binary (`tests/sy_file_journey_e2e.rs`) and the
/// in-source `#[cfg(test)]` tree can share one lock — without this,
/// cargo's parallel runner races the `set_var` write between the
/// two test crates and one of them ends up writing into the
/// operator's real trash.
///
/// `tokio::sync::Mutex` (async-aware) so callers can hold it across
/// `await` points — the public `trash` / `list` / `restore` calls
/// are async by construction. Same precedent as
/// `copy.rs::IOURING_TEST_LOCK`.
///
/// `#[cfg(test)]` mirrors `plugin::registry::ENV_LOCK` — this
/// is permanent test infrastructure; the bin itself never reads it.
#[doc(hidden)]
#[cfg(test)]
pub static TRASH_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// One row of the freedesktop trash. `trash_id` is the basename of
/// the `.trashinfo` file (stripped of the suffix) — the freedesktop
/// spec's identifier the `[Trash Info]` section keys on. Other DEs
/// (and `gio trash --list`) sort and restore by the same id.
#[derive(Debug, Clone)]
pub struct TrashedItem {
    /// Original absolute path of the file before it was trashed.
    /// Restored to this path by [`restore`] unless the caller
    /// observes a `RestoreCollision`.
    pub original: PathBuf,
    /// `.trashinfo` basename (e.g. `foo` for `foo.trashinfo`). The
    /// `trash` crate's `TrashItem::id` carries the absolute path to
    /// the info file; we extract the basename so consumers can match
    /// against the freedesktop `info/` directory listing without
    /// re-parsing the absolute path.
    pub trash_id: String,
    /// When the file was deleted, reconstructed from the
    /// `trash::TrashItem::time_deleted` epoch-seconds field. With
    /// the `chrono` feature on (the workspace pins it on), the
    /// `.trashinfo`'s `DeletionDate=YYYY-MM-DDThh:mm:ss` is the
    /// canonical source — we materialise it back into a
    /// `SystemTime` so callers can sort/filter without re-parsing
    /// the on-disk format.
    pub deleted_at: SystemTime,
    /// Size of the file in bytes at the moment it was trashed. Used
    /// by the iced UI's "trash bin" pane (Step 24+) to show a "free
    /// space recoverable" footer; today the field is computed from
    /// `TrashItem`'s metadata.
    pub size: u64,
}

/// Move every `path` to the freedesktop trash. Wraps the sync
/// `trash::delete` per path inside `spawn_blocking` so the public
/// surface stays async. Returns one [`TrashedItem`] per src in the
/// same order; on partial failure the `Err` carries the index of
/// the first src that failed (in the anyhow context) so the caller
/// can roll back the prefix that succeeded.
pub async fn trash(paths: &[PathBuf]) -> Result<Vec<TrashedItem>> {
    let owned: Vec<PathBuf> = paths.to_vec();
    let join = tokio::task::spawn_blocking(move || trash_blocking(&owned)).await;
    match join {
        Ok(res) => res,
        Err(e) => Err(anyhow::anyhow!("trash spawn_blocking panicked: {e}")),
    }
}

/// Enumerate every entry in the user's trash (home trash + any
/// per-mount `.Trash-<uid>/` dir that the freedesktop spec defines).
/// Wraps `trash::os_limited::list` via `spawn_blocking`. The
/// returned order matches the crate's enumeration (no particular
/// order — callers sort when they need stability).
pub async fn list() -> Result<Vec<TrashedItem>> {
    let join = tokio::task::spawn_blocking(list_blocking).await;
    match join {
        Ok(res) => res,
        Err(e) => Err(anyhow::anyhow!("list spawn_blocking panicked: {e}")),
    }
}

/// Restore one trashed item to its original path. Wraps
/// `trash::os_limited::restore_all` over a single-item iterator.
/// Returns the path the file was restored to (which equals
/// `item.original` on success — we surface it explicitly so a future
/// test against the IPC layer can assert against the on-the-wire
/// value without re-deriving it).
pub async fn restore(item: TrashedItem) -> Result<PathBuf> {
    let owned = item;
    let join = tokio::task::spawn_blocking(move || restore_blocking(owned)).await;
    match join {
        Ok(res) => res,
        Err(e) => Err(anyhow::anyhow!("restore spawn_blocking panicked: {e}")),
    }
}

/// Sync inner of [`trash`]. Walks the input vec, calls `trash::delete`
/// per path, snapshots `list_blocking` once at the end and matches
/// every trashed src back to the resulting [`TrashedItem`] by
/// `original_path`. The post-trash list call is cheap (single
/// directory read of `Trash/info/`) and keeps the per-src metadata
/// in one place — the `trash` crate's `delete` doesn't return a
/// `TrashItem`, so we'd otherwise have to re-derive `trash_id` from
/// the filename + numeric suffix dance ourselves.
fn trash_blocking(paths: &[PathBuf]) -> Result<Vec<TrashedItem>> {
    let mut canonical_before: Vec<PathBuf> = Vec::with_capacity(paths.len());
    for (idx, p) in paths.iter().enumerate() {
        // Canonicalise BEFORE the move so we can match the post-trash
        // listing against the resolved absolute path (the trash crate
        // canonicalises internally; matching by the un-canonicalised
        // input would miss symlinks + relative paths).
        let canon = std::fs::canonicalize(p)
            .with_context(|| format!("trash: canonicalize src #{idx} {p:?} failed"))?;
        canonical_before.push(canon);
        trash::delete(p).with_context(|| format!("trash: delete src #{idx} {p:?} failed"))?;
    }
    let listed = list_blocking().with_context(|| "trash: list after delete failed")?;
    let mut out: Vec<TrashedItem> = Vec::with_capacity(paths.len());
    for (idx, canon) in canonical_before.iter().enumerate() {
        let item = listed
            .iter()
            .find(|t| &t.original == canon)
            .cloned()
            .with_context(|| {
                format!("trash: src #{idx} {canon:?} not in post-delete list — fs raced?")
            })?;
        out.push(item);
    }
    Ok(out)
}

/// Sync inner of [`list`]. Maps every `trash::TrashItem` onto our
/// [`TrashedItem`] shape: `original_path()` joins parent + name,
/// `time_deleted` (epoch seconds) becomes a `SystemTime`, and the
/// `trash_id` is the basename of the absolute `.trashinfo` path
/// (with `.trashinfo` stripped).
fn list_blocking() -> Result<Vec<TrashedItem>> {
    let items = trash::os_limited::list().map_err(|e| anyhow::anyhow!("trash list: {e}"))?;
    let mut out: Vec<TrashedItem> = Vec::with_capacity(items.len());
    for item in &items {
        let size = trash::os_limited::metadata(item)
            .ok()
            .and_then(|m| m.size.size())
            .unwrap_or(0);
        out.push(map_item(item, size));
    }
    Ok(out)
}

/// Sync inner of [`restore`]. Restores one item, returns its
/// canonical original path. We retain a copy of `original` BEFORE
/// handing the underlying `TrashItem` to `restore_all` because the
/// crate's API consumes the item (the iterator's `Item = TrashItem`).
fn restore_blocking(item: TrashedItem) -> Result<PathBuf> {
    let original = item.original.clone();
    // Re-list to find the matching `trash::TrashItem` by trash_id;
    // we serialised the basename in our `TrashedItem` precisely so
    // this lookup is O(n) over the user's trash without needing to
    // hold an opaque crate-side handle across `await`s.
    let listed = trash::os_limited::list().map_err(|e| anyhow::anyhow!("trash list: {e}"))?;
    let matched = listed
        .into_iter()
        .find(|t| info_basename_id(t) == item.trash_id)
        .with_context(|| {
            format!(
                "restore: no trash entry matches trash_id={:?} (original={:?})",
                item.trash_id, item.original
            )
        })?;
    trash::os_limited::restore_all([matched]).map_err(|e| anyhow::anyhow!("restore_all: {e}"))?;
    Ok(original)
}

/// Map a `trash::TrashItem` onto our public [`TrashedItem`] shape.
/// Extracted so both `trash_blocking` (via its post-delete list
/// scan) and `list_blocking` produce byte-identical rows.
fn map_item(item: &trash::TrashItem, size: u64) -> TrashedItem {
    TrashedItem {
        original: item.original_path(),
        trash_id: info_basename_id(item),
        deleted_at: epoch_to_systemtime(item.time_deleted),
        size,
    }
}

/// `trash::TrashItem::id` is the absolute path to the `.trashinfo`
/// file on Linux/freedesktop. Strip the directory + `.trashinfo`
/// suffix so the caller has the canonical id other DEs use.
fn info_basename_id(item: &trash::TrashItem) -> String {
    let id_path = Path::new(&item.id);
    let base = id_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    base.strip_suffix(".trashinfo")
        .map(|s| s.to_string())
        .unwrap_or(base)
}

/// `trash::TrashItem::time_deleted` is "non-leap seconds since the
/// UNIX epoch". Convert to `SystemTime` saturating at UNIX_EPOCH for
/// negative values (which the crate uses to signal "missing" on
/// non-chrono builds — we keep that branch safe even though the
/// workspace turns chrono on).
fn epoch_to_systemtime(epoch_secs: i64) -> SystemTime {
    if epoch_secs <= 0 {
        return SystemTime::UNIX_EPOCH;
    }
    SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(epoch_secs as u64)
}

#[cfg(test)]
mod tests {
    //! Step 18 acceptance tests. Each maps to a Step 18 DoD bullet:
    //!
    //! * `trash_then_list_then_restore_roundtrip` — happy-path J5/J6
    //!   round-trip end-to-end against a hermetic `$XDG_DATA_HOME`.
    //! * `trash_preserves_freedesktop_trashinfo` — on-disk format
    //!   matches the spec other DEs (Nautilus, Dolphin, `gio trash
    //!   --list`) read.
    //! * `cross_fs_trash_uses_per_mount_trashdir` — entries land in
    //!   the canonical `$XDG_DATA_HOME/Trash/info/` dir (genuine
    //!   cross-mount needs root; documented limit inline).
    //! * `restore_to_original_path_when_unchanged` — byte-equality +
    //!   path-equality after the round-trip.

    use super::*;

    /// Override `$XDG_DATA_HOME` to a hermetic tempdir for the
    /// lifetime of one test. Returns a guard whose Drop restores
    /// the previous value (so a parent test process that already
    /// set `XDG_DATA_HOME` doesn't see it stomped on by us). The
    /// `_lock` argument exists purely to force the caller to hold
    /// the `TRASH_TEST_LOCK` for the full scope — the env mutation
    /// is process-global.
    struct XdgGuard {
        prev: Option<std::ffi::OsString>,
    }

    impl XdgGuard {
        fn set(target: &Path) -> Self {
            let prev = std::env::var_os("XDG_DATA_HOME");
            // SAFETY: every call site holds `TRASH_TEST_LOCK` so no
            // other thread is reading or writing $XDG_DATA_HOME
            // concurrently.
            unsafe {
                std::env::set_var("XDG_DATA_HOME", target);
            }
            Self { prev }
        }
    }

    impl Drop for XdgGuard {
        fn drop(&mut self) {
            // SAFETY: same lock-held reasoning as `set()`. The Drop
            // restores the previous value (or removes the var) so a
            // parent test environment that pre-set it survives.
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                    None => std::env::remove_var("XDG_DATA_HOME"),
                }
            }
        }
    }

    /// One synthetic source file under the tempdir's `journey/`
    /// subdir. Returns its absolute path + the bytes written so the
    /// restore-side equality check can re-read and compare.
    fn plant_source(root: &Path, name: &str, body: &[u8]) -> PathBuf {
        let journey = root.join("journey");
        std::fs::create_dir_all(&journey).expect("mkdir journey");
        let src = journey.join(name);
        std::fs::write(&src, body).expect("write src");
        src
    }

    /// Happy-path round-trip: `trash` → `list` sees it → `restore`
    /// → file back at its original path. This is the journey-J5 +
    /// J6 reversal beat as a single assertion sequence.
    #[tokio::test(flavor = "current_thread")]
    async fn trash_then_list_then_restore_roundtrip() {
        let _lock = TRASH_TEST_LOCK.lock().await;
        let root = tempfile::tempdir().expect("tempdir");
        let _guard = XdgGuard::set(root.path());
        let body = b"sy-step18-roundtrip-payload".to_vec();
        let src = plant_source(root.path(), "roundtrip.txt", &body);

        let trashed = trash(std::slice::from_ref(&src))
            .await
            .expect("trash must succeed");
        assert_eq!(trashed.len(), 1, "one src trashed -> one TrashedItem");
        assert!(!src.exists(), "src must be moved out of original location");

        let listed = list().await.expect("list must succeed");
        let listed_ids: Vec<&String> = listed.iter().map(|t| &t.trash_id).collect();
        assert!(
            listed_ids.iter().any(|id| **id == trashed[0].trash_id),
            "list must contain the trashed id; got {listed_ids:?}",
        );

        let restored = restore(trashed[0].clone())
            .await
            .expect("restore must succeed");
        assert_eq!(
            restored,
            std::fs::canonicalize(&src).unwrap_or_else(|_| src.clone()),
            "restore must return the original canonical path"
        );
        let after = std::fs::read(&src).expect("file restored at original path");
        assert_eq!(after, body, "restored bytes must equal trashed bytes");
    }

    /// On-disk format check: the `.trashinfo` file the freedesktop
    /// spec writes carries `[Trash Info]\nPath=<uri>\nDeletionDate=
    /// YYYY-MM-DDThh:mm:ss`. Other DEs (Nautilus, `gio trash
    /// --list`) parse the same three lines.
    #[tokio::test(flavor = "current_thread")]
    async fn trash_preserves_freedesktop_trashinfo() {
        let _lock = TRASH_TEST_LOCK.lock().await;
        let root = tempfile::tempdir().expect("tempdir");
        let _guard = XdgGuard::set(root.path());
        let body = b"sy-step18-trashinfo-format".to_vec();
        let src = plant_source(root.path(), "trashinfo.txt", &body);

        let trashed = trash(std::slice::from_ref(&src))
            .await
            .expect("trash must succeed");
        let info_dir = root.path().join("Trash").join("info");
        let info_path = info_dir.join(format!("{}.trashinfo", trashed[0].trash_id));
        let raw = std::fs::read_to_string(&info_path).expect("read .trashinfo");
        assert!(
            raw.starts_with("[Trash Info]\n"),
            ".trashinfo must lead with the [Trash Info] header (got: {raw:?})"
        );
        assert!(
            raw.contains("Path="),
            ".trashinfo must carry a Path= field (got: {raw:?})"
        );
        // The percent-encoded URI of an absolute path always begins
        // with a `/` after Path= — every test fixture is absolute.
        let path_line = raw
            .lines()
            .find(|l| l.starts_with("Path="))
            .expect("Path= line present");
        assert!(
            path_line.starts_with("Path=/"),
            "Path= must encode an absolute URI (got: {path_line:?})"
        );
        let deletion_line = raw
            .lines()
            .find(|l| l.starts_with("DeletionDate="))
            .expect("DeletionDate= line present");
        // YYYY-MM-DDThh:mm:ss is 19 chars after the `=`. We assert
        // the prefix shape rather than the exact value so the test
        // is wall-clock-independent.
        let date_value = deletion_line
            .strip_prefix("DeletionDate=")
            .expect("DeletionDate prefix");
        assert_eq!(
            date_value.len(),
            19,
            "DeletionDate must be YYYY-MM-DDThh:mm:ss (19 chars), got {date_value:?}",
        );
        assert!(
            date_value.as_bytes()[10] == b'T',
            "DeletionDate must use T separator at index 10, got {date_value:?}",
        );
    }

    /// Genuine cross-mount needs root (`mount --bind` or a loop
    /// device). For the in-process test we assert the trashinfo
    /// lands under the canonical `$XDG_DATA_HOME/Trash/info/` dir
    /// when the src is on the same filesystem as `$HOME` — the
    /// freedesktop "home trash" topdir, which the `trash` crate's
    /// `move_to_trash` resolves via its mount-point scan. Documented
    /// limit: a richer cross-mount probe arrives when the `trash`
    /// crate exposes a per-mount inspection hook upstream.
    #[tokio::test(flavor = "current_thread")]
    async fn cross_fs_trash_uses_per_mount_trashdir() {
        let _lock = TRASH_TEST_LOCK.lock().await;
        let root = tempfile::tempdir().expect("tempdir");
        let _guard = XdgGuard::set(root.path());
        let body = b"sy-step18-mount-probe".to_vec();
        let src = plant_source(root.path(), "mount.txt", &body);

        let trashed = trash(std::slice::from_ref(&src))
            .await
            .expect("trash must succeed");
        let info_path = root
            .path()
            .join("Trash")
            .join("info")
            .join(format!("{}.trashinfo", trashed[0].trash_id));
        assert!(
            info_path.exists(),
            "home-trash topdir must hold the .trashinfo entry at {info_path:?}",
        );
        let files_path = root
            .path()
            .join("Trash")
            .join("files")
            .join(&trashed[0].trash_id);
        assert!(
            files_path.exists(),
            "home-trash topdir must hold the moved file payload at {files_path:?}",
        );
    }

    /// Byte-equality after restore — distinct from the roundtrip
    /// test which only asserts `list-then-restore-runs`. This one
    /// pins the SPEC §3.4 anti-goal "data loss" contract: a trashed
    /// file restored without an intervening edit returns byte-for-
    /// byte.
    #[tokio::test(flavor = "current_thread")]
    async fn restore_to_original_path_when_unchanged() {
        let _lock = TRASH_TEST_LOCK.lock().await;
        let root = tempfile::tempdir().expect("tempdir");
        let _guard = XdgGuard::set(root.path());
        let body: Vec<u8> = (0u8..=255u8).cycle().take(16 * 1024).collect();
        let src = plant_source(root.path(), "bytes.bin", &body);
        let canonical_src = std::fs::canonicalize(&src).expect("canonicalize src");

        let trashed = trash(std::slice::from_ref(&src))
            .await
            .expect("trash must succeed");
        let restored = restore(trashed[0].clone())
            .await
            .expect("restore must succeed");
        assert_eq!(
            restored, canonical_src,
            "restore must return the file to its canonical original path"
        );
        let after = std::fs::read(&src).expect("file restored");
        assert_eq!(
            after, body,
            "restored bytes must equal the pre-trash bytes byte-for-byte"
        );
    }
}
