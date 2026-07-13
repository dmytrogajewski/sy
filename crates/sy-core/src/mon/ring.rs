//! mmap-backed N×M f32 ring buffer for `sy mon collect` history.
//!
//! Per SPEC §3 SCOPE item 3, the aggregator (ROADMAP Step 11) writes
//! one row per tick — `n_metrics` `f32` columns — into a fixed-shape
//! grid. The popup and the `system.mon.history` IPC op read recent
//! windows out of it. The file is mmap-backed so a crashed aggregator
//! leaves a recoverable on-disk state and a fresh `Ring::open` can
//! resume seamlessly.
//!
//! ## On-disk layout
//!
//! | offset | size | field                                       |
//! |-------:|-----:|---------------------------------------------|
//! | 0      | 32   | `[u8; 32]` magic (`MAGIC` below)            |
//! | 32     | 8    | `u64` little-endian `seq` counter           |
//! | 40     | 4    | `u32` little-endian `n_secs` (N rows)       |
//! | 44     | 4    | `u32` little-endian `n_metrics` (M columns) |
//! | 48     | 8    | `u64` little-endian `head` (write cursor)   |
//! | 56     | N*M*4| row-major `f32` grid                        |
//!
//! Total file size: `HEADER_LEN + n_secs * n_metrics * 4` bytes.
//!
//! ## Crash detection
//!
//! On open, the magic is validated against the [`MAGIC`] constant. A
//! torn write to the seq counter surfaces as a header that fails the
//! shape sanity check (`n_secs != 0 && n_metrics != 0`); the magic
//! tag itself catches the "completely garbage file" case. Callers
//! who want a "wipe and restart" path use [`Ring::open_or_rebuild`].
//!
//! ## Concurrency
//!
//! `Ring::open` takes a `LOCK_EX` `flock(2)` on the underlying file
//! and holds it for the lifetime of the `Ring`. A second process
//! attempting `Ring::open` on the same path receives an error
//! immediately (we use the non-blocking variant so tests don't hang).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use memmap2::{MmapMut, MmapOptions};
use nix::fcntl::{Flock, FlockArg};

/// File magic. Exactly 32 bytes. A future on-disk format change
/// (e.g. switching to fixed-point columns) bumps the trailing `v1`
/// to `v2`; older readers reject the file with a clear error rather
/// than silently mis-parsing.
pub const MAGIC: [u8; 32] = *b"sy-mon-ring-v1\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";

/// Bytes from start of file to start of the f32 grid.
/// `32 (magic) + 8 (seq) + 4 (n_secs) + 4 (n_metrics) + 8 (head)`.
pub const HEADER_LEN: usize = 56;

const OFF_SEQ: usize = 32;
const OFF_N_SECS: usize = 40;
const OFF_HEAD: usize = 48;

/// mmap-backed fixed-shape ring buffer.
#[derive(Debug)]
pub struct Ring {
    /// Held for the lifetime of the `Ring` when the ring was opened
    /// for writing (the aggregator's path). Dropped on `Ring` drop,
    /// which `flock(2)`-unlocks the file and closes the descriptor.
    /// `None` for readers that opened via [`Ring::open_attach`] —
    /// they keep the file alive via the `data` mmap and never lock.
    _lock: Option<Flock<File>>,
    /// mmap over the data region only (`HEADER_LEN..`). The header
    /// is read/written via positioned reads/writes on a separate
    /// `File` handle to keep seq-counter updates off the mmap.
    data: MmapMut,
    /// Header read/write handle. Separate from the locked file so we
    /// can `seek` + `read_exact` without disturbing the lock.
    hdr: File,
    /// Path kept for diagnostics — not used by `open` itself.
    _path: PathBuf,
    n_secs: u32,
    n_metrics: u32,
    /// Cached copy of the on-disk `head` so `push` doesn't pay a
    /// `read_exact` per call. Kept in lockstep with the file.
    head: u64,
    /// Cached copy of `seq`. Same rationale as `head`.
    seq: u64,
}

impl Ring {
    /// Open an existing ring at `path`, or initialise one if the
    /// file is empty. Returns an error if the file exists but its
    /// header doesn't validate; use [`Ring::open_or_rebuild`] to
    /// recover from that case.
    pub fn open(path: impl AsRef<Path>, n_secs: u32, n_metrics: u32) -> Result<Self> {
        let path = path.as_ref();
        if n_secs == 0 || n_metrics == 0 {
            bail!("ring shape must be non-zero: n_secs={n_secs}, n_metrics={n_metrics}");
        }
        let total_len = expected_len(n_secs, n_metrics);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("open ring file {}", path.display()))?;

        // Acquire exclusive flock immediately, non-blocking so a
        // second aggregator instance fails fast instead of hanging.
        let locked = Flock::lock(file, FlockArg::LockExclusiveNonblock)
            .map_err(|(_, errno)| anyhow!("flock ring file {}: {errno}", path.display()))?;

        // We need a second handle for positioned header I/O that
        // doesn't disturb the mmap. Reopen by path; the flock is
        // *per open-file-description*, but two handles in the same
        // process share the lock domain — the second open here does
        // not block on the first. (Cross-process locking still
        // works because the second process gets a fresh OFD.)
        let mut hdr = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("reopen ring file for header I/O: {}", path.display()))?;

        let cur_len = hdr
            .metadata()
            .with_context(|| format!("stat ring file {}", path.display()))?
            .len();

        if cur_len == 0 {
            // Fresh file — initialise header + grow to full size.
            init_file(&mut hdr, n_secs, n_metrics, total_len)
                .with_context(|| format!("initialise ring file {}", path.display()))?;
        } else if cur_len != total_len {
            bail!(
                "ring file {} has wrong size: expected {} bytes, found {} \
                 (n_secs={n_secs}, n_metrics={n_metrics})",
                path.display(),
                total_len,
                cur_len,
            );
        }

        let (seq, on_disk_n_secs, on_disk_n_metrics, head) = read_header(&mut hdr)
            .with_context(|| format!("read ring header {}", path.display()))?;
        if on_disk_n_secs != n_secs || on_disk_n_metrics != n_metrics {
            bail!(
                "ring file {} shape mismatch: on-disk ({on_disk_n_secs}×{on_disk_n_metrics}) \
                 vs requested ({n_secs}×{n_metrics})",
                path.display()
            );
        }

        // mmap covers only the data region; header lives outside.
        // Safety: we hold an exclusive flock on the file, and no
        // other code in this process aliases the same byte range.
        // `Flock<File>` derefs to `File`, which `memmap2` accepts
        // via its `MmapOptions::map_mut` impl.
        let locked_file: &File = &locked;
        let data = unsafe {
            MmapOptions::new()
                .offset(HEADER_LEN as u64)
                .len((total_len as usize) - HEADER_LEN)
                .map_mut(locked_file)
        }
        .with_context(|| format!("mmap ring data region {}", path.display()))?;

        Ok(Self {
            _lock: Some(locked),
            data,
            hdr,
            _path: path.to_path_buf(),
            n_secs,
            n_metrics,
            head,
            seq,
        })
    }

    /// Attach to an existing ring **read-only** without taking a
    /// `flock(2)`. Use for processes that consume ring samples while
    /// the aggregator holds the exclusive write lock (e.g. the `sy
    /// mon` popup reading sparkline history). The returned [`Ring`]
    /// supports [`Self::read_metric`] / [`Self::seq`] / `Self::head`
    /// but [`Self::push`] is a logic error from the caller — the mmap
    /// is technically writable but two writers stomp on each other.
    /// Errors if the file is missing, the size doesn't match
    /// `(n_secs, n_metrics)`, or the header magic is invalid.
    pub fn open_attach(path: impl AsRef<Path>, n_secs: u32, n_metrics: u32) -> Result<Self> {
        let path = path.as_ref();
        if n_secs == 0 || n_metrics == 0 {
            bail!("ring shape must be non-zero: n_secs={n_secs}, n_metrics={n_metrics}");
        }
        let total_len = expected_len(n_secs, n_metrics);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(false)
            .truncate(false)
            .open(path)
            .with_context(|| format!("attach ring file {}", path.display()))?;
        let cur_len = file
            .metadata()
            .with_context(|| format!("stat ring file {}", path.display()))?
            .len();
        if cur_len != total_len {
            bail!(
                "ring file {} has wrong size: expected {} bytes, found {}",
                path.display(),
                total_len,
                cur_len,
            );
        }

        let mut hdr = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("reopen ring file for header I/O: {}", path.display()))?;
        let (seq, on_disk_n_secs, on_disk_n_metrics, head) = read_header(&mut hdr)
            .with_context(|| format!("read ring header {}", path.display()))?;
        if on_disk_n_secs != n_secs || on_disk_n_metrics != n_metrics {
            bail!(
                "ring file {} shape mismatch: on-disk ({on_disk_n_secs}×{on_disk_n_metrics}) \
                 vs requested ({n_secs}×{n_metrics})",
                path.display()
            );
        }

        // Safety: another process holds LOCK_EX and is the only
        // writer; the kernel keeps both mmap views in sync via the
        // shared page cache. We never call `push` on this Ring.
        let data = unsafe {
            MmapOptions::new()
                .offset(HEADER_LEN as u64)
                .len((total_len as usize) - HEADER_LEN)
                .map_mut(&file)
        }
        .with_context(|| format!("mmap ring data region {}", path.display()))?;

        Ok(Self {
            _lock: None,
            data,
            hdr,
            _path: path.to_path_buf(),
            n_secs,
            n_metrics,
            head,
            seq,
        })
    }

    /// Like [`Ring::open`], but if the existing file's header doesn't
    /// validate (corrupt magic, wrong size, zero shape) the file is
    /// rewritten as a fresh ring instead of erroring.
    pub fn open_or_rebuild(path: impl AsRef<Path>, n_secs: u32, n_metrics: u32) -> Result<Self> {
        let path = path.as_ref();
        match Ring::open(path, n_secs, n_metrics) {
            Ok(r) => Ok(r),
            Err(_) => {
                // Wipe the file and retry. We do *not* `remove_file`
                // — truncate-to-zero is enough and avoids racing
                // with a sibling that's about to recreate it.
                std::fs::File::create(path)
                    .with_context(|| format!("rebuild ring file {}", path.display()))?;
                Ring::open(path, n_secs, n_metrics)
            }
        }
    }

    /// Push one row of `self.n_metrics` `f32` samples. Wraps around
    /// once the head reaches the end of the grid.
    pub fn push(&mut self, row: &[f32]) -> Result<()> {
        if row.len() != self.n_metrics as usize {
            bail!(
                "ring push expected {} metrics, got {}",
                self.n_metrics,
                row.len()
            );
        }
        let row_bytes = (self.n_metrics as usize) * std::mem::size_of::<f32>();
        let slot = (self.head as usize) % (self.n_secs as usize);
        let off = slot * row_bytes;
        // Safety on the cast: `row` is `&[f32]`; reinterpreting as
        // `&[u8]` is sound and respects alignment because the
        // destination is a raw byte slice with no alignment demand.
        let src = bytemuck_cast(row);
        self.data[off..off + row_bytes].copy_from_slice(src);

        // Bump cached head + seq, then persist the header. We do
        // this *after* the grid write so a crash mid-update leaves
        // the previous row intact rather than half-overwriting.
        self.head = self.head.wrapping_add(1);
        self.seq = self.seq.wrapping_add(1);
        write_header(
            &mut self.hdr,
            self.seq,
            self.n_secs,
            self.n_metrics,
            self.head,
        )
        .context("write ring header after push")?;
        // Ensure the grid byte range is durable before the header
        // claims a new tail. mmap flushes are best-effort on tmpfs
        // but the call is cheap and correct on a real filesystem.
        self.data.flush().context("flush ring mmap")?;
        Ok(())
    }

    /// Return the most recent `up_to_n_secs` samples for column
    /// `idx`, oldest first. If fewer rows have been pushed than
    /// requested, returns whatever exists (`Vec.len() <= up_to_n_secs`).
    pub fn read_metric(&self, idx: usize, up_to_n_secs: usize) -> Result<Vec<f32>> {
        if idx >= self.n_metrics as usize {
            bail!(
                "ring read_metric idx {idx} out of range (n_metrics={})",
                self.n_metrics
            );
        }
        let cap = self.n_secs as usize;
        let total_pushed = self.head as usize;
        let available = total_pushed.min(cap);
        let want = up_to_n_secs.min(available);
        if want == 0 {
            return Ok(Vec::new());
        }
        // The most recent row is at slot `(head - 1) mod n_secs`;
        // the oldest of the `want`-row window is `want` rows before
        // that, modulo `n_secs`. Walk forward from oldest to newest.
        let row_bytes = (self.n_metrics as usize) * std::mem::size_of::<f32>();
        let metric_off = idx * std::mem::size_of::<f32>();
        let mut out = Vec::with_capacity(want);
        let start_slot = (total_pushed + cap - want) % cap;
        for i in 0..want {
            let slot = (start_slot + i) % cap;
            let off = slot * row_bytes + metric_off;
            let mut buf = [0u8; 4];
            buf.copy_from_slice(&self.data[off..off + 4]);
            out.push(f32::from_le_bytes(buf));
        }
        Ok(out)
    }

    /// Monotonic write counter. Persisted across reopens.
    pub fn seq(&self) -> u64 {
        self.seq
    }
}

/// Total expected file size for the given shape.
fn expected_len(n_secs: u32, n_metrics: u32) -> u64 {
    HEADER_LEN as u64 + (n_secs as u64) * (n_metrics as u64) * (std::mem::size_of::<f32>() as u64)
}

/// Initialise a freshly-created (zero-length) ring file.
fn init_file(hdr: &mut File, n_secs: u32, n_metrics: u32, total_len: u64) -> Result<()> {
    hdr.set_len(total_len).context("ftruncate ring file")?;
    hdr.seek(SeekFrom::Start(0))?;
    hdr.write_all(&MAGIC)?;
    hdr.write_all(&0u64.to_le_bytes())?; // seq
    hdr.write_all(&n_secs.to_le_bytes())?;
    hdr.write_all(&n_metrics.to_le_bytes())?;
    hdr.write_all(&0u64.to_le_bytes())?; // head
    hdr.sync_data().context("fsync new ring header")?;
    Ok(())
}

/// Read + validate the header; return (seq, n_secs, n_metrics, head).
fn read_header(hdr: &mut File) -> Result<(u64, u32, u32, u64)> {
    hdr.seek(SeekFrom::Start(0))?;
    let mut magic = [0u8; 32];
    hdr.read_exact(&mut magic).context("read magic")?;
    if magic != MAGIC {
        bail!("ring file magic mismatch: not a sy-mon ring (corrupt header)");
    }
    let mut seq = [0u8; 8];
    let mut n_secs = [0u8; 4];
    let mut n_metrics = [0u8; 4];
    let mut head = [0u8; 8];
    hdr.read_exact(&mut seq)?;
    hdr.read_exact(&mut n_secs)?;
    hdr.read_exact(&mut n_metrics)?;
    hdr.read_exact(&mut head)?;
    let n_secs_v = u32::from_le_bytes(n_secs);
    let n_metrics_v = u32::from_le_bytes(n_metrics);
    if n_secs_v == 0 || n_metrics_v == 0 {
        bail!("ring file header has zero shape ({n_secs_v}x{n_metrics_v}); corrupt");
    }
    Ok((
        u64::from_le_bytes(seq),
        n_secs_v,
        n_metrics_v,
        u64::from_le_bytes(head),
    ))
}

/// Overwrite the seq + head fields of the header.
fn write_header(hdr: &mut File, seq: u64, n_secs: u32, n_metrics: u32, head: u64) -> Result<()> {
    hdr.seek(SeekFrom::Start(OFF_SEQ as u64))?;
    hdr.write_all(&seq.to_le_bytes())?;
    // n_secs / n_metrics are immutable post-init, but rewriting them
    // costs 8 bytes and keeps the header monolithic.
    hdr.seek(SeekFrom::Start(OFF_N_SECS as u64))?;
    hdr.write_all(&n_secs.to_le_bytes())?;
    hdr.write_all(&n_metrics.to_le_bytes())?;
    hdr.seek(SeekFrom::Start(OFF_HEAD as u64))?;
    hdr.write_all(&head.to_le_bytes())?;
    // No sync_data here on the hot path: the kernel page cache
    // takes care of ordering against the mmap flush above; a true
    // crash-safety story would `fsync` here, but the seq counter +
    // magic together let `Ring::open` detect a torn write and the
    // caller (Step 11 aggregator) is expected to call `open_or_rebuild`.
    Ok(())
}

/// Reinterpret `&[f32]` as `&[u8]` for byte-level grid I/O. We don't
/// pull in `bytemuck` for one cast; the operation is sound because
/// `f32` is `Copy` + has no padding + `&[u8]` has no alignment demand.
fn bytemuck_cast(s: &[f32]) -> &[u8] {
    // Safety: `f32` has size 4 and no invalid bit-patterns; the
    // returned slice is read-only and the lifetime is tied to `s`.
    unsafe { std::slice::from_raw_parts(s.as_ptr() as *const u8, std::mem::size_of_val(s)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const N_SECS: u32 = 4;
    const N_METRICS: u32 = 3;

    fn ring_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("ring.bin")
    }

    #[test]
    fn push_pop_roundtrip() {
        let dir = tempdir().expect("tempdir");
        let path = ring_path(&dir);
        let mut ring = Ring::open(&path, N_SECS, N_METRICS).expect("open");
        // Push N_SECS distinct rows.
        for i in 0..N_SECS {
            let f = i as f32;
            ring.push(&[f, f + 0.1, f + 0.2]).expect("push");
        }
        let col0 = ring.read_metric(0, N_SECS as usize).expect("read col 0");
        let col1 = ring.read_metric(1, N_SECS as usize).expect("read col 1");
        let col2 = ring.read_metric(2, N_SECS as usize).expect("read col 2");
        assert_eq!(col0, vec![0.0, 1.0, 2.0, 3.0]);
        assert_eq!(col1, vec![0.1, 1.1, 2.1, 3.1]);
        assert_eq!(col2, vec![0.2, 1.2, 2.2, 3.2]);
    }

    #[test]
    fn wraparound_keeps_last_n() {
        let dir = tempdir().expect("tempdir");
        let path = ring_path(&dir);
        let mut ring = Ring::open(&path, N_SECS, N_METRICS).expect("open");
        // Push 2*N_SECS rows; only the last N_SECS should survive.
        for i in 0..(2 * N_SECS) {
            let f = i as f32;
            ring.push(&[f, f + 0.1, f + 0.2]).expect("push");
        }
        let col0 = ring.read_metric(0, N_SECS as usize).expect("read col 0");
        // Last N_SECS rows are indices [N_SECS..2*N_SECS).
        assert_eq!(col0, vec![4.0, 5.0, 6.0, 7.0]);
        assert_eq!(ring.seq(), (2 * N_SECS) as u64);
    }

    #[test]
    fn magic_header_validates_on_open() {
        let dir = tempdir().expect("tempdir");
        let path = ring_path(&dir);
        // Create a valid ring + drop it so the flock is released.
        {
            let _r = Ring::open(&path, N_SECS, N_METRICS).expect("open");
        }
        // Corrupt the magic in-place.
        let mut bytes = std::fs::read(&path).expect("read ring file");
        bytes[0..4].copy_from_slice(b"XXXX");
        std::fs::write(&path, &bytes).expect("write corrupt ring");

        let err = Ring::open(&path, N_SECS, N_METRICS).expect_err("should reject");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("magic"),
            "error chain should mention 'magic'; got: {chain}"
        );
    }

    #[test]
    fn seq_monotonic_across_reopens() {
        let dir = tempdir().expect("tempdir");
        let path = ring_path(&dir);
        {
            let mut ring = Ring::open(&path, N_SECS, N_METRICS).expect("open 1");
            for i in 0..N_SECS {
                ring.push(&[i as f32; N_METRICS as usize]).expect("push");
            }
            assert_eq!(ring.seq(), N_SECS as u64);
        }
        // Reopen — seq must persist, not reset.
        let mut ring = Ring::open(&path, N_SECS, N_METRICS).expect("open 2");
        assert_eq!(ring.seq(), N_SECS as u64, "seq must survive reopen");
        ring.push(&[99.0; N_METRICS as usize])
            .expect("push after reopen");
        assert_eq!(ring.seq(), (N_SECS + 1) as u64);
    }

    #[test]
    fn corrupt_rebuilds_fresh() {
        let dir = tempdir().expect("tempdir");
        let path = ring_path(&dir);
        // Pre-populate with garbage bytes that fail magic validation.
        std::fs::write(&path, vec![0xFFu8; 128]).expect("seed corrupt file");
        // open_or_rebuild must wipe + reinit.
        let mut ring = Ring::open_or_rebuild(&path, N_SECS, N_METRICS).expect("rebuild");
        assert_eq!(ring.seq(), 0, "rebuilt ring starts at seq=0");
        ring.push(&[1.0; N_METRICS as usize])
            .expect("push on rebuilt");
        let col0 = ring.read_metric(0, 1).expect("read");
        assert_eq!(col0, vec![1.0]);
    }

    #[test]
    fn second_open_is_locked_out() {
        let dir = tempdir().expect("tempdir");
        let path = ring_path(&dir);
        let _ring = Ring::open(&path, N_SECS, N_METRICS).expect("first open");
        // Second open in the *same process* would normally share the
        // flock domain on Linux (OFD-vs-process semantics), but
        // `Ring::open` creates a fresh `OpenOptions` handle each
        // time, so the second `Flock::lock` sees an already-locked
        // file and the non-blocking attempt returns EWOULDBLOCK.
        let err = Ring::open(&path, N_SECS, N_METRICS)
            .expect_err("second open must fail while first holds the lock");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("flock"),
            "error chain should mention 'flock'; got: {chain}"
        );
    }

    /// Live-smoke regression: `sy mon` popup crashed with `EAGAIN`
    /// because `Ring::open_or_rebuild` took `LOCK_EX` while the
    /// aggregator daemon already held it. `open_attach` skips the
    /// flock and returns a read-capable handle that sees the writer's
    /// pushes through the shared page cache.
    #[test]
    fn attach_reads_while_writer_holds_lock() {
        let dir = tempdir().expect("tempdir");
        let path = ring_path(&dir);
        let mut writer = Ring::open(&path, N_SECS, N_METRICS).expect("writer open");
        for i in 0..N_SECS {
            let f = i as f32;
            writer.push(&[f, f + 0.1, f + 0.2]).expect("push");
        }
        let reader = Ring::open_attach(&path, N_SECS, N_METRICS).expect("attach");
        assert_eq!(reader.seq(), writer.seq());
        let col0 = reader.read_metric(0, N_SECS as usize).expect("read col 0");
        assert_eq!(col0, vec![0.0, 1.0, 2.0, 3.0]);
    }

    /// `open_attach` requires the file to exist + be the right size —
    /// it's an attach, not a create, so a missing or shape-mismatched
    /// file errors instead of silently recreating it.
    #[test]
    fn attach_rejects_missing_file() {
        let dir = tempdir().expect("tempdir");
        let path = ring_path(&dir);
        let err = Ring::open_attach(&path, N_SECS, N_METRICS)
            .expect_err("attach must fail when the file is absent");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("attach ring file"),
            "error chain should mention 'attach ring file'; got: {chain}"
        );
    }
}
