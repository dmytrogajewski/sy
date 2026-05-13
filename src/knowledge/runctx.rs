//! Per-pass runtime context: cancellation token, fixed-duration throttle,
//! and an optional adaptive throttle that targets a CPU-usage cap.
//!
//! Threaded into `cli::run_index` via `&RunCtx`. Interactive CLI calls
//! (`sy knowledge index`, `sync`, `search`) build a `default_interactive`
//! context — no cancel, no cap. Daemon scheduled passes use
//! `for_daemon_pass` which wires up the daemon-owned `cancel` flag and the
//! adaptive throttle if `[knowledge].cpu_max_percent` is configured.

use std::{
    sync::{atomic::AtomicBool, Arc, Mutex},
    time::{Duration, Instant},
};

use super::sources;

/// Read once per ProbeEnv build; cheap. Used by the adaptive throttle to
/// translate /proc/self/stat jiffy deltas into CPU-time fractions.
fn ticks_per_sec() -> f64 {
    // SAFETY: sysconf is thread-safe. _SC_CLK_TCK is well-defined on Linux.
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v <= 0 {
        100.0
    } else {
        v as f64
    }
}

/// Adaptive CPU cap. After each batch, samples /proc/self/stat to compute
/// the daemon's CPU-time usage over the last interval. If usage > target,
/// sleeps proportionally to the overshoot so the rolling average drifts
/// toward the target. Self-correcting; no privileged calls.
#[derive(Debug)]
pub struct AdaptiveThrottle {
    target: f64, // 0.0..1.0
    last_sample: Instant,
    last_jiffies: u64,
    ticks_per_sec: f64,
}

impl AdaptiveThrottle {
    pub fn new(target_pct: u8) -> Self {
        Self {
            target: (target_pct as f64 / 100.0).clamp(0.01, 1.0),
            last_sample: Instant::now(),
            last_jiffies: read_self_jiffies().unwrap_or(0),
            ticks_per_sec: ticks_per_sec(),
        }
    }

    /// Call after each batch finishes. Sleeps if the recent CPU fraction
    /// exceeded the target. Capped at 2 s per call to avoid runaway sleeps
    /// after a stall.
    pub fn tick(&mut self) {
        let now = Instant::now();
        let wall = now.duration_since(self.last_sample).as_secs_f64();
        if wall < 0.05 {
            // Too small a sample window — wait for the next batch.
            return;
        }
        let jiffies = match read_self_jiffies() {
            Some(j) => j,
            None => {
                self.last_sample = now;
                return;
            }
        };
        let delta_cpu_secs =
            (jiffies.saturating_sub(self.last_jiffies)) as f64 / self.ticks_per_sec;
        let frac = (delta_cpu_secs / wall).clamp(0.0, 64.0); // multi-core normalised against wall
        if frac > self.target {
            // overshoot ratio: how much extra time we need to sleep so the
            // average over (wall + sleep) hits the target.
            // target = cpu_used / (wall + sleep)  →  sleep = cpu_used/target - wall
            let needed = (delta_cpu_secs / self.target) - wall;
            let sleep = needed.clamp(0.0, 2.0);
            if sleep > 0.001 {
                std::thread::sleep(Duration::from_secs_f64(sleep));
            }
        }
        // Re-sample after the sleep so the next window is accurate.
        self.last_sample = Instant::now();
        self.last_jiffies = read_self_jiffies().unwrap_or(jiffies);
    }
}

fn read_self_jiffies() -> Option<u64> {
    let body = std::fs::read_to_string("/proc/self/stat").ok()?;
    // The 14th and 15th fields (utime, stime) are jiffies. The comm field
    // can contain spaces inside parens, so split off the trailing portion
    // after the closing ')'.
    let close = body.rfind(')')?;
    let tail = &body[close + 1..];
    let parts: Vec<&str> = tail.split_whitespace().collect();
    // After the closing ')', field indices start at 3. utime is field 14
    // → tail index 11; stime is field 15 → tail index 12.
    let utime: u64 = parts.get(11)?.parse().ok()?;
    let stime: u64 = parts.get(12)?.parse().ok()?;
    Some(utime + stime)
}

/// Runtime context threaded through `run_index` / `flush_batch`. Cheap to
/// clone (the cancel flag + throttle are wrapped in `Arc`). The adaptive
/// throttle is in a `Mutex` because it carries mutable state and we want
/// the same instance reused across batches — cloning would lose it.
#[derive(Clone)]
pub struct RunCtx {
    pub throttle: Duration,
    pub cancel: Arc<AtomicBool>,
    pub adaptive: Option<Arc<Mutex<AdaptiveThrottle>>>,
}

impl RunCtx {
    /// Default for interactive `sy knowledge index/sync/search` calls. No
    /// cancellation token, no adaptive cap, no throttle.
    pub fn interactive() -> Self {
        Self {
            throttle: Duration::ZERO,
            cancel: Arc::new(AtomicBool::new(false)),
            adaptive: None,
        }
    }

    /// Build the daemon's per-pass context. Reuses the daemon's owned
    /// cancellation flag so the user can preempt mid-pass.
    pub fn for_daemon_pass(cancel: Arc<AtomicBool>, throttle: Duration) -> Self {
        let adaptive =
            sources::cpu_max_percent().map(|p| Arc::new(Mutex::new(AdaptiveThrottle::new(p))));
        Self {
            throttle,
            cancel,
            adaptive,
        }
    }

    /// Has a Cancel been requested?
    pub fn cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Apply both the fixed and the adaptive throttle (in that order).
    /// Called by `flush_batch` after each upsert.
    pub fn after_batch(&self) {
        if !self.throttle.is_zero() {
            std::thread::sleep(self.throttle);
        }
        if let Some(a) = &self.adaptive {
            if let Ok(mut a) = a.lock() {
                a.tick();
            }
        }
    }
}
