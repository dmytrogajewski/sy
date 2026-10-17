//! Append-only JSONL audit sink — SPEC §4.4 "Audit log" second
//! bullet.
//!
//! Layout under `$XDG_STATE_HOME/sy/`:
//! - `audit.jsonl` — live file, one JSON object per line.
//! - `audit.jsonl.1.zst` … `audit.jsonl.10.zst` — rotated archives,
//!   newest first. The eleventh would-be archive is deleted on
//!   rotation, capping disk use to ~10 × 64 MiB compressed.
//!
//! ### Rotation order (atomic re-rename, never compress-in-place)
//!
//! When the live file exceeds [`ROTATE_THRESHOLD_BYTES`] = 64 MiB:
//! 1. Shift every existing `audit.jsonl.{n}.zst` to
//!    `audit.jsonl.{n+1}.zst`, walking from highest `n` down to 1
//!    so we never overwrite an archive that still needs to move.
//!    `audit.jsonl.{ARCHIVE_RETENTION}.zst` (if it would otherwise
//!    become `.{ARCHIVE_RETENTION+1}.zst`) is deleted instead.
//! 2. Rename `audit.jsonl` → `audit.jsonl.1` (atomic on same FS).
//!    A crash here leaves a `.1` file behind without `.1.zst`; the
//!    next rotation overwrites it during the compress step.
//! 3. Stream-compress `audit.jsonl.1` → `audit.jsonl.1.zst`. If a
//!    stale `.1.zst` is present from a prior crashed rotation we
//!    overwrite it; the rename in step 2 already gave us the
//!    canonical input.
//! 4. Delete `audit.jsonl.1` on successful compression.
//!
//! The function is fire-and-forget at the call-site level (see
//! `agt::audit::emit`); errors propagate to the caller for the
//! `tracing::error!` mirror but never panic.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::agt::audit::AuditRecord;

/// Rotate the live file when it grows past this many bytes (SPEC
/// §4.4 mandates 64 MiB). Public for the `rotate_if_needed` helper
/// used by the rotation tests; production callers go through
/// [`emit_jsonl`].
pub const ROTATE_THRESHOLD_BYTES: u64 = 64 * 1024 * 1024;

/// Keep this many compressed archives; older ones are deleted on
/// rotation. 10 × 64 MiB = ~640 MiB worst-case before any compression
/// gain, which lands well inside the per-user state budget.
pub const ARCHIVE_RETENTION: usize = 10;

/// Basename of the live JSONL file.
const LIVE_FILE_NAME: &str = "audit.jsonl";

/// Append one record to `<dir>/audit.jsonl` and rotate if the
/// post-append size crosses [`ROTATE_THRESHOLD_BYTES`]. Creates `dir`
/// if missing — matches the daemon's `create_dir_all` behaviour for
/// `$XDG_STATE_HOME/sy/`.
pub fn emit_jsonl(record: &AuditRecord, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let live = live_path(dir);
    let line = serde_json::to_string(record).context("serialise audit record")?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&live)
        .with_context(|| format!("open {}", live.display()))?;
    // One `write_all` carries the line + newline; an interrupted
    // write would leave a partial JSON line that downstream readers
    // (`jq -c '.' audit.jsonl`) would reject — but rotation runs in
    // the same thread so this is bounded to the current syscall.
    writeln!(file, "{line}").with_context(|| format!("append to {}", live.display()))?;
    file.flush()
        .with_context(|| format!("flush {}", live.display()))?;
    rotate_if_needed(dir)?;
    Ok(())
}

/// Inspect the live file's size; rotate if it exceeds
/// [`ROTATE_THRESHOLD_BYTES`]. Exposed so tests can drive rotation
/// without writing 64 MiB through `serde_json::to_string` for every
/// line (they pre-stuff the file and call this directly).
pub fn rotate_if_needed(dir: &Path) -> Result<()> {
    let live = live_path(dir);
    let size = match std::fs::metadata(&live) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("stat {}", live.display())),
    };
    if size <= ROTATE_THRESHOLD_BYTES {
        return Ok(());
    }
    rotate(dir)
}

/// Unconditional rotation. Public-in-module so the tests can call it
/// directly to validate the archive-shift logic with synthetic files
/// well under 64 MiB.
pub fn rotate(dir: &Path) -> Result<()> {
    shift_archives(dir)?;

    let live = live_path(dir);
    let staged = archive_unzipped_path(dir, 1);
    std::fs::rename(&live, &staged)
        .with_context(|| format!("rename {} -> {}", live.display(), staged.display()))?;

    let dest = archive_path(dir, 1);
    compress_archive(&staged, &dest)
        .with_context(|| format!("compress {} -> {}", staged.display(), dest.display()))?;
    // Successful compression supersedes the staged uncompressed
    // file; remove it. A crash between rename and compress would
    // leave a `.1` file behind; the next rotation overwrites it.
    std::fs::remove_file(&staged).with_context(|| format!("remove staged {}", staged.display()))?;
    Ok(())
}

fn shift_archives(dir: &Path) -> Result<()> {
    // Walk from the retention cap downward. If `.{N}.zst` exists and
    // would shift to `.{N+1}.zst` (i.e. past the cap), delete it
    // instead. Then move each lower archive up one slot. This avoids
    // overwriting an archive we haven't shifted yet.
    let oldest = archive_path(dir, ARCHIVE_RETENTION);
    if oldest.exists() {
        std::fs::remove_file(&oldest)
            .with_context(|| format!("evict oldest archive {}", oldest.display()))?;
    }
    for n in (1..ARCHIVE_RETENTION).rev() {
        let src = archive_path(dir, n);
        if !src.exists() {
            continue;
        }
        let dst = archive_path(dir, n + 1);
        std::fs::rename(&src, &dst)
            .with_context(|| format!("shift {} -> {}", src.display(), dst.display()))?;
    }
    Ok(())
}

fn compress_archive(input: &Path, output: &Path) -> Result<()> {
    let mut src = File::open(input).with_context(|| format!("open input {}", input.display()))?;
    // Truncate any stale `.zst` left by a previously crashed
    // rotation — the rename guarantees `input` is the authoritative
    // payload, so a half-written `.zst` from before must go.
    let dst =
        File::create(output).with_context(|| format!("create archive {}", output.display()))?;
    let mut encoder =
        zstd::stream::write::Encoder::new(dst, COMPRESS_LEVEL).context("zstd encoder init")?;
    // 64 KiB copy buffer — tracking peak heap is unimportant here
    // because rotation runs once per ~64 MiB of audit traffic.
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = src.read(&mut buf).context("read input chunk")?;
        if n == 0 {
            break;
        }
        encoder.write_all(&buf[..n]).context("encode chunk")?;
    }
    encoder.finish().context("finalise zstd stream")?;
    Ok(())
}

/// zstd compression level. Level 3 is the crate default — balances
/// CPU cost (rotation runs inline in the audit hot path) against
/// ratio. Bumping to higher levels can quadruple rotation latency.
const COMPRESS_LEVEL: i32 = 3;

fn live_path(dir: &Path) -> PathBuf {
    dir.join(LIVE_FILE_NAME)
}

fn archive_path(dir: &Path, n: usize) -> PathBuf {
    dir.join(format!("{LIVE_FILE_NAME}.{n}.zst"))
}

fn archive_unzipped_path(dir: &Path, n: usize) -> PathBuf {
    dir.join(format!("{LIVE_FILE_NAME}.{n}"))
}

/// Pre-stuff the live file with `bytes` of synthetic content so
/// rotation tests don't burn IO writing real records through
/// `emit_jsonl` 70 MiB at a time. Public-in-module so tests reach it.
#[cfg(test)]
fn stuff_live_file(dir: &Path, bytes: u64) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    std::fs::create_dir_all(dir)?;
    let path = live_path(dir);
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)?;
    f.seek(SeekFrom::Start(bytes.saturating_sub(1)))?;
    f.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agt::audit::AuditDecision;
    use tempfile::tempdir;

    /// SPEC §4.4: rotation triggers when the live file crosses
    /// 64 MiB. After rotation the live file is below threshold and
    /// `audit.jsonl.1.zst` exists with the prior payload.
    #[test]
    fn jsonl_rotation_at_64mib() {
        let tmp = tempdir().expect("tempdir");
        let dir = tmp.path();
        let over_threshold = ROTATE_THRESHOLD_BYTES + 6 * 1024 * 1024; // 70 MiB
        stuff_live_file(dir, over_threshold).expect("stuff live");
        rotate_if_needed(dir).expect("rotate");

        let live_meta = std::fs::metadata(live_path(dir)).ok();
        let live_size = live_meta.map(|m| m.len()).unwrap_or(0);
        assert!(
            live_size <= ROTATE_THRESHOLD_BYTES,
            "live file should have shrunk; size={live_size}"
        );
        assert!(
            archive_path(dir, 1).exists(),
            "expected audit.jsonl.1.zst after rotation"
        );
    }

    /// Rotate 12 times — newest archive moves to `.1.zst` each time,
    /// older ones shift up, anything that would land past
    /// `ARCHIVE_RETENTION` is evicted. The assertion is on the
    /// surviving file *count*; their names are deterministic per the
    /// rotation order.
    #[test]
    fn jsonl_keeps_last_10_archives() {
        let tmp = tempdir().expect("tempdir");
        let dir = tmp.path();
        for i in 0..12 {
            // Each iteration stuffs the live file with a unique
            // tiny byte so the resulting `.zst` payloads differ —
            // any incorrect archive shifting (overwrite, leak)
            // changes the per-archive content fingerprint and the
            // count check still trips.
            let live = live_path(dir);
            std::fs::write(&live, format!("payload-{i}\n").as_bytes()).expect("seed live");
            rotate(dir).expect("rotate iter");
        }
        let zsts: Vec<PathBuf> = (1..=ARCHIVE_RETENTION + 2)
            .map(|n| archive_path(dir, n))
            .filter(|p| p.exists())
            .collect();
        assert_eq!(
            zsts.len(),
            ARCHIVE_RETENTION,
            "expected exactly {} archives; got {:?}",
            ARCHIVE_RETENTION,
            zsts
        );
        // Cap check: `.{N+1}.zst` must be absent.
        let beyond = archive_path(dir, ARCHIVE_RETENTION + 1);
        assert!(
            !beyond.exists(),
            "archive beyond retention cap leaked: {}",
            beyond.display()
        );
    }

    /// Round-trip a single emit: writes a line, ends with a newline,
    /// stays under the rotation threshold.
    #[test]
    fn emit_appends_one_json_line() {
        let tmp = tempdir().expect("tempdir");
        let dir = tmp.path();
        let record = AuditRecord::now(
            "/usr/bin/rg",
            "deadbeef",
            AuditDecision::Allow,
            vec!["needle".into()],
        );
        emit_jsonl(&record, dir).expect("emit");
        let body = std::fs::read_to_string(live_path(dir)).expect("read live");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 1, "expected one JSONL line; got {body:?}");
        let v: serde_json::Value = serde_json::from_str(lines[0]).expect("parse line");
        assert_eq!(v["tool"], "/usr/bin/rg");
        assert_eq!(v["decision"], "allow");
        assert!(!archive_path(dir, 1).exists());
    }
}
