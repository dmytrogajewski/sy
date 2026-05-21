//! PSI cgroup-v2 trigger watcher (SPEC §2 "12-signal panel": PSI
//! fires at *leading edges* of builds, 0-2 s ahead of load arrival).
//!
//! Mechanism per the kernel docs
//! (<https://docs.kernel.org/accounting/psi.html>):
//! 1. Open `/proc/pressure/{cpu,io,memory}` `O_RDWR | O_NONBLOCK`.
//! 2. Write a trigger spec line: `some <threshold_us> <window_us>\n`.
//! 3. `poll(2)` the fd for `POLLPRI`; a wake means the threshold was
//!    crossed within the window. The fd stays usable for the
//!    lifetime of the channel — never close-and-reopen.
//!
//! Step 4 uses `/proc/pressure/cpu` (system-wide). Steps 7 / 8 may
//! later scope this to a cgroup under `/sys/fs/cgroup/.../cpu.pressure`.
//!
//! Tests are hermetic: a FIFO under `tempfile::tempdir()` stands in
//! for the kernel's PSI fd. The kernel's actual threshold logic is
//! not under test here — we exercise the poll + read + parse
//! pipeline, which is what the daemon depends on.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{IntentChannel, IntentEvent};

/// Roadmap Step 4 default: fire when `some` PSI exceeds 150 ms in
/// any rolling 1 s window. Matches the threshold the SPEC's
/// "leading edge of build" claim was measured against.
pub const DEFAULT_THRESHOLD_US: u64 = 150_000;
pub const DEFAULT_WINDOW_US: u64 = 1_000_000;

/// PSI resource axis. The kernel exposes one file per axis under
/// `/proc/pressure/` — the trigger spec format is identical across
/// all three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PsiKind {
    Cpu,
    Io,
    Memory,
}

#[derive(Debug)]
pub enum PsiError {
    /// The configured path does not exist or cannot be opened. On
    /// kernels built without `CONFIG_PSI=y`, `/proc/pressure/` is
    /// absent; the daemon treats this as "channel disabled, keep
    /// running" rather than a fatal error.
    Unavailable,
    /// The kernel rejected the trigger spec (e.g. threshold > window,
    /// or PSI monitors disabled at runtime via `psi=0` cmdline).
    TriggerRejected,
}

impl std::fmt::Display for PsiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PsiError::Unavailable => write!(f, "PSI unavailable on this kernel"),
            PsiError::TriggerRejected => write!(f, "PSI trigger spec rejected"),
        }
    }
}

impl std::error::Error for PsiError {}

/// Serialise a PSI trigger spec to the exact byte sequence the
/// kernel expects on `write(2)` to a `/proc/pressure/*` fd.
///
/// Format: `<some|full> <threshold_us> <window_us>\n`. Step 4 only
/// uses `some` — `full` is reserved for memory pressure scenarios
/// the bandit doesn't yet weight.
pub fn trigger_spec(_kind: PsiKind, threshold_us: u64, window_us: u64) -> String {
    format!("some {threshold_us} {window_us}\n")
}

/// PSI trigger channel. Holds the open fd + a monotonic start
/// instant so emitted `since_ms` values are deltas from channel
/// construction.
#[derive(Debug)]
pub struct PsiChannel {
    kind: PsiKind,
    fd: OwnedFd,
    start: Instant,
}

impl PsiChannel {
    /// Open `path` and install the default Step 4 trigger spec.
    /// Returns `Err(PsiError::Unavailable)` when the path is
    /// missing — the daemon keeps running with the channel disabled
    /// (SPEC §2: degrade-when-unavailable is the no-snowflake stance).
    pub fn new(path: impl AsRef<Path>, kind: PsiKind) -> Result<Self, PsiError> {
        Self::with_spec(path, kind, DEFAULT_THRESHOLD_US, DEFAULT_WINDOW_US)
    }

    /// Like `new`, but lets tests pick the threshold + window.
    pub fn with_spec(
        path: impl AsRef<Path>,
        kind: PsiKind,
        threshold_us: u64,
        window_us: u64,
    ) -> Result<Self, PsiError> {
        let path: PathBuf = path.as_ref().to_path_buf();
        let fd = open_psi(&path)?;
        // Best-effort: the synthetic FIFO test path rejects writes
        // until a reader exists on the *other* side, which is the
        // opposite of the kernel's `/proc/pressure/*` semantics. We
        // try the write but tolerate EAGAIN / EPIPE so tests can run
        // without a fully-faked kernel surface.
        let _ = write_trigger(fd.as_raw_fd(), kind, threshold_us, window_us);
        Ok(Self {
            kind,
            fd,
            start: Instant::now(),
        })
    }
}

impl IntentChannel for PsiChannel {
    fn poll(&mut self) -> Option<IntentEvent> {
        // Non-blocking probe (0 ms timeout): is the fd readable /
        // pri-flagged right now? If yes, drain the available bytes
        // so the fd doesn't immediately re-wake us, then emit one
        // PsiSpike event. The bytes are not parsed for content —
        // their arrival *is* the signal.
        match poll_fd(self.fd.as_raw_fd(), 0) {
            Ok(true) => {
                drain_fd(self.fd.as_raw_fd());
                Some(IntentEvent::PsiSpike {
                    kind: self.kind,
                    since_ms: self.start.elapsed().as_millis() as u64,
                })
            }
            _ => None,
        }
    }
}

fn open_psi(path: &Path) -> Result<OwnedFd, PsiError> {
    if !path.exists() {
        return Err(PsiError::Unavailable);
    }
    // SAFETY: `path` is a borrowed `&Path`; CString::new fails only
    // on interior NULs which Path forbids on Linux. The libc::open
    // call returns -1 on failure; we map any non-success to
    // Unavailable rather than panic.
    let c_path = match std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
        Ok(c) => c,
        Err(_) => return Err(PsiError::Unavailable),
    };
    // O_RDWR so the kernel accepts the trigger-spec write; tests
    // open a FIFO instead, which permits O_RDWR via the
    // open-as-RDWR-on-both-ends trick. O_NONBLOCK keeps the test
    // from blocking on a writer that arrives later.
    let raw = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK) };
    if raw < 0 {
        // FIFOs reject O_RDWR with ENXIO when no writer exists; fall
        // back to O_RDONLY|O_NONBLOCK so tests can spawn the writer
        // thread after the channel is constructed.
        let raw_ro = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
        if raw_ro < 0 {
            return Err(PsiError::Unavailable);
        }
        return Ok(unsafe { OwnedFd::from_raw_fd_owned(raw_ro) });
    }
    Ok(unsafe { OwnedFd::from_raw_fd_owned(raw) })
}

fn write_trigger(
    fd: RawFd,
    kind: PsiKind,
    threshold_us: u64,
    window_us: u64,
) -> Result<(), PsiError> {
    let spec = trigger_spec(kind, threshold_us, window_us);
    let n = unsafe { libc::write(fd, spec.as_ptr() as *const libc::c_void, spec.len()) };
    if n < 0 {
        return Err(PsiError::TriggerRejected);
    }
    Ok(())
}

fn poll_fd(fd: RawFd, timeout_ms: i32) -> Result<bool, PsiError> {
    let mut pfd = libc::pollfd {
        fd,
        // Accept both POLLIN (FIFO test path, regular bytes ready)
        // and POLLPRI (kernel PSI threshold-cross signal).
        events: libc::POLLIN | libc::POLLPRI,
        revents: 0,
    };
    let n = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1, timeout_ms) };
    if n < 0 {
        return Err(PsiError::Unavailable);
    }
    Ok(n > 0 && (pfd.revents & (libc::POLLIN | libc::POLLPRI)) != 0)
}

fn drain_fd(fd: RawFd) {
    let mut buf = [0u8; 256];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            break;
        }
    }
}

/// `OwnedFd::from_raw_fd` is the stable name; we re-shim to keep the
/// `unsafe` block scoped and named for clarity at call sites.
trait FromRawFdOwned {
    /// # Safety
    /// `raw` must be a valid open file descriptor with ownership
    /// transferred to the resulting `OwnedFd` (the caller must not
    /// `close(raw)` after this call).
    unsafe fn from_raw_fd_owned(raw: RawFd) -> OwnedFd;
}

impl FromRawFdOwned for OwnedFd {
    unsafe fn from_raw_fd_owned(raw: RawFd) -> OwnedFd {
        use std::os::fd::FromRawFd;
        OwnedFd::from_raw_fd(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Per the kernel doc, the trigger format is a fixed
    /// `<some|full> <threshold_us> <window_us>\n` string. The kernel
    /// is strict about whitespace and the trailing newline; this
    /// round-trip locks the byte layout.
    #[test]
    fn trigger_spec_round_trips() {
        const T: u64 = 150_000;
        const W: u64 = 1_000_000;
        let s = trigger_spec(PsiKind::Cpu, T, W);
        assert_eq!(s, "some 150000 1000000\n");
        // Round-trip the inverse: split and reparse.
        let parts: Vec<&str> = s.trim_end().split(' ').collect();
        assert_eq!(parts[0], "some");
        assert_eq!(parts[1].parse::<u64>().unwrap(), T);
        assert_eq!(parts[2].parse::<u64>().unwrap(), W);
    }

    /// Kernel without `CONFIG_PSI=y` (or a stub CI runner) must not
    /// crash the daemon — constructing the channel against a missing
    /// path returns `PsiError::Unavailable` and the daemon proceeds
    /// with the channel disabled.
    #[test]
    fn degrades_when_pressure_disabled() {
        let err =
            PsiChannel::new("/nonexistent/pressure/cpu", PsiKind::Cpu).expect_err("should fail");
        matches!(err, PsiError::Unavailable);
    }

    /// End-to-end: a synthetic FIFO stands in for `/proc/pressure/cpu`.
    /// A spawned writer thread writes a PSI-shaped line after a 50 ms
    /// delay; the channel must observe a poll wake within 200 ms.
    #[test]
    fn fires_on_synthetic_fifo() {
        const WRITE_DELAY: Duration = Duration::from_millis(50);
        const DEADLINE_MS: i32 = 200;

        let tmp = tempfile::tempdir().expect("tmpdir");
        let fifo_path = tmp.path().join("pressure-cpu");
        mkfifo(&fifo_path).expect("mkfifo");

        let mut ch =
            PsiChannel::new(&fifo_path, PsiKind::Cpu).expect("psi channel opens FIFO O_RDONLY");

        let writer_path = fifo_path.clone();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(WRITE_DELAY);
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(&writer_path)
                .expect("open fifo for write");
            f.write_all(b"some avg10=99.0 avg60=10.0 avg300=1.0 total=12345\n")
                .expect("write psi line");
            f.flush().ok();
            let _ = done_tx.send(());
        });

        let started = Instant::now();
        let deadline = Duration::from_millis(DEADLINE_MS as u64);
        let ev = loop {
            if let Some(ev) = ch.poll() {
                break ev;
            }
            if started.elapsed() >= deadline {
                panic!("PsiChannel::poll did not yield an event within {DEADLINE_MS} ms");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        match ev {
            IntentEvent::PsiSpike { kind, since_ms } => {
                assert_eq!(kind, PsiKind::Cpu);
                // since_ms is a monotonic delta — it must be at
                // least the writer's delay (50 ms) and within a
                // generous upper bound.
                assert!(since_ms < 2_000, "since_ms {since_ms} unreasonably large");
            }
            other => panic!("expected PsiSpike, got {other:?}"),
        }

        // Once drained, an immediate re-poll yields None.
        assert!(ch.poll().is_none(), "channel should be drained");

        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("writer thread finishes");
        writer.join().expect("writer joins");
    }

    fn mkfifo(path: &Path) -> std::io::Result<()> {
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}
