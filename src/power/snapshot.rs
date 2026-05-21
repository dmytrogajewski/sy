//! Snapshot assembler — drains the 12-channel intent + sensor panel
//! into a single immutable `Snapshot` per 1 Hz tick.
//!
//! ## 12-channel feature layout
//!
//! The vector is pinned forever: the Step 24 GRU input shape and the
//! Step 33 metrics extractor both index into the same offsets, so any
//! reordering is a breaking change to the audit log schema.
//!
//! | Idx | Field              | Source                          | Units            |
//! |----:|--------------------|---------------------------------|------------------|
//! |  0  | `tctl_c`           | hwmon k10temp Tctl              | °C (NaN on err)  |
//! |  1  | `package_power_w`  | RAPL 5-tap moving average       | W                |
//! |  2  | `igpu_busy_pct`    | amdgpu `gpu_busy_percent`       | 0..100           |
//! |  3  | `npu_workloads`    | aiplane registry depth          | count            |
//! |  4  | `battery_soc_pct`  | `BAT*/capacity`                 | 0..100           |
//! |  5  | `ac_online`        | `Mains/online` (or `AC*`)       | 0.0 or 1.0       |
//! |  6  | `battery_drain_w`  | battery `power_now` (0 on AC)   | W                |
//! |  7  | `psi_cpu_some_avg10` | PSI cpu spike intensity       | 0..100           |
//! |  8  | `call_active`      | logind/portal/mpris coalesce    | 0 or 1           |
//! |  9  | `user_idle_s`      | `UserIdle.since_ms` / 1000      | s                |
//! | 10  | `tod_sin`          | `TimeOfDay.sin`                 | unit-circle      |
//! | 11  | `tod_cos`          | `TimeOfDay.cos`                 | unit-circle      |
//!
//! Day-of-week components from `TimeOfDay` (`dow_sin`, `dow_cos`) are
//! retained in [`SnapshotRaw`] but excluded from the GRU feature vec
//! today — Step 24 decides what makes the final 16-input window.
//!
//! ## Privacy invariants (SPEC §4)
//!
//! No raw window titles, notification bodies, keystrokes, clipboard
//! contents, or media metadata reach the `Snapshot` struct or the
//! [`SnapshotRaw`] companion. The classifier outputs that *do* cross
//! the boundary are coarse bools (`call_active`, `media_playing`,
//! `screen_cast_active`, `fan_complaint`) and the niri `app_id`
//! (already stripped of title at the intent parser).
//!
//! Tests `no_title_in_serialised_snapshot` enforces this by JSON
//! introspection.

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::power::activity::{ActivityLabel, ACTIVITY_CLASS_COUNT};
use crate::power::clock::Clock;
use crate::power::intent::{
    AiplaneIntentChannel, CgroupAncestryChannel, IdleChannel, IntentChannel, IntentEvent,
    LogindChannel, MprisChannel, NiriChannel, NotifyChannel, PsiChannel, ScreenCastChannel,
    TimeChannel,
};
use crate::power::sensors::{
    BatterySensor, HwmonSensor, IgpuSensor, NpuSensor, PlatformSensor, PstateSensor, RaplSensor,
    Sensor, SensorReading,
};

/// Versioned schema identifier — every persisted snapshot carries this
/// so the Step 9 NDJSON reader can dispatch on schema even when the
/// 12-channel layout evolves in a later breaking step.
///
/// ## v1 → v2 (Step 29)
///
/// Step 29 bumps the wire tag to `v2` to reflect that `SnapshotRaw`
/// now carries [`SnapshotRaw::activity_label`] — the current activity
/// class scored by [`crate::power::activity::OnlineClassifier`]. The
/// 12-channel `features` array is unchanged; v1 NDJSON still
/// deserialises because `activity_label` is `#[serde(default)] = None`.
/// Status JSON v1 stays stable — only the audit-internal snapshot
/// shape grew.
pub const SCHEMA_ID: &str = "sy.power.snapshot/v2";

/// Width of the GRU feature vec. Pinned: every later step indexes
/// into the same slot table, so widening the vec is a breaking change
/// to the audit log + model input shape.
pub const FEATURE_LEN: usize = 12;

// Feature-vec indices. Named constants over magic literals so the
// docstring table stays the single source of truth.
const IDX_TCTL_C: usize = 0;
const IDX_PACKAGE_POWER_W: usize = 1;
const IDX_IGPU_BUSY_PCT: usize = 2;
const IDX_NPU_WORKLOADS: usize = 3;
const IDX_BATTERY_SOC_PCT: usize = 4;
const IDX_AC_ONLINE: usize = 5;
const IDX_BATTERY_DRAIN_W: usize = 6;
const IDX_PSI_CPU_SOME_AVG10: usize = 7;
const IDX_CALL_ACTIVE: usize = 8;
const IDX_USER_IDLE_S: usize = 9;
const IDX_TOD_SIN: usize = 10;
const IDX_TOD_COS: usize = 11;

const MS_PER_S: f32 = 1000.0;
const UW_PER_W: f32 = 1_000_000.0;

/// Typed raw readings alongside the feature vec — preserved so the
/// audit log (Step 9) can reconstruct human-readable summaries
/// without re-parsing sensors. Every field is `Option<_>` so a
/// missing sensor degrades to `None` rather than panicking.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SnapshotRaw {
    pub tctl_c: Option<f32>,
    pub package_power_w: Option<f32>,
    pub igpu_busy_pct: Option<u8>,
    pub npu_workloads: Option<u32>,
    pub battery_soc_pct: Option<u8>,
    pub ac_online: Option<bool>,
    pub battery_drain_w: Option<f32>,
    pub psi_cpu_some_avg10: Option<f32>,
    pub call_active: Option<bool>,
    pub user_idle_s: Option<f32>,
    pub tod_sin: Option<f32>,
    pub tod_cos: Option<f32>,
    pub tod_dow_sin: Option<f32>,
    pub tod_dow_cos: Option<f32>,
    /// niri-stripped `app_id` (no title) — the only string field on
    /// the snapshot. SPEC §4 Privacy explicitly permits this.
    pub focused_app_id: Option<String>,
    /// Step 28: the "13th feature" — current activity class as scored
    /// by the [`crate::power::activity::OnlineClassifier`]. Stays
    /// `None` until Step 29 wires the classifier into the daemon's
    /// `collect_tick` path. Carried on `SnapshotRaw` (not on the
    /// pinned 12-channel `features` array) per SPEC §4: the feature
    /// vec is sensor-derived only; classifier outputs ride alongside
    /// in `raw` so the audit log's downstream consumers (GRU input
    /// in Step 29, bandit context, `sy power explain`) see the label
    /// without breaking the v1 feature-vec hash.
    #[serde(default)]
    pub activity_label: Option<ActivityLabel>,
    /// Step P2-3: the GRU forecaster's 5-class probability
    /// distribution over [`crate::power::activity::ActivityLabel`]
    /// (indices match `ACTIVITY_CLASSES` order: idle/browse/call/code
    /// /build). Populated by the daemon's [`crate::power::daemon::
    /// one_tick`] right after sensor collection so the next tick's
    /// drift detector can compare `argmax(prev_forecast)` against
    /// the realised activity label. `#[serde(default)]` keeps v2
    /// NDJSON entries (written before this field existed) parseable.
    #[serde(default)]
    pub activity_forecast: Option<[f32; ACTIVITY_CLASS_COUNT_RAW]>,
}

/// Width of the [`SnapshotRaw::activity_forecast`] vector. Pinned
/// to match [`crate::power::activity::ACTIVITY_CLASS_COUNT`] and
/// [`crate::power::forecast::model::FORECAST_CLASS_COUNT`]; widening
/// is a breaking change to the audit-log schema.
pub const ACTIVITY_CLASS_COUNT_RAW: usize = ACTIVITY_CLASS_COUNT;

/// Default for the [`Snapshot::schema`] field on deserialization.
/// The field is `&'static str` (cheap, pinned) and serializes as the
/// constant [`SCHEMA_ID`]; on the read side it is reconstituted from
/// this default rather than borrowed from the wire format, so the
/// reader works against ND-JSON written by any v1 producer.
fn default_snapshot_schema() -> &'static str {
    SCHEMA_ID
}

/// Deserialise the 12-slot feature array tolerating JSON `null` for
/// any slot — `serde_json` emits `null` for `f32::NAN` on the
/// serialise side (NaN isn't representable in JSON), and the default
/// deserialiser rejects `null → f32`. Without this shim the four
/// read-side CLI surfaces silently return zero entries despite the
/// daemon writing 1 Hz to disk. Each `null` slot reconstitutes as
/// `f32::NAN`, preserving the sensor-failure semantics in
/// [`SnapshotRaw`].
fn deserialize_features_null_as_nan<'de, D>(deserializer: D) -> Result<[f32; FEATURE_LEN], D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opts: [Option<f32>; FEATURE_LEN] = serde::Deserialize::deserialize(deserializer)?;
    Ok(opts.map(|o| o.unwrap_or(f32::NAN)))
}

/// One immutable 1 Hz observation. Step 9's NDJSON writer serialises
/// this verbatim; Step 11 surfaces a hash + selected fields on
/// `sy power status --json`. Step 12 reads it back via
/// [`Deserialize`] for `sy power log --since=…`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// Always [`SCHEMA_ID`] on the wire. The reader reconstructs the
    /// pinned `&'static str` from the default rather than borrowing
    /// from the JSON line, so the field type stays `&'static str`
    /// (matches the daemon's hot-path struct literal).
    #[serde(default = "default_snapshot_schema", skip_deserializing)]
    pub schema: &'static str,
    pub ts: DateTime<Utc>,
    /// On the wire each slot is either a JSON number or `null`. The
    /// default `serde_json` serialiser already emits `null` for
    /// `f32::NAN` (NaN is not representable in JSON); the custom
    /// deserialiser below restores `null → NaN` so a sensor that
    /// failed at read time round-trips losslessly through Step H1's
    /// audit-log tail readers.
    #[serde(deserialize_with = "deserialize_features_null_as_nan")]
    pub features: [f32; FEATURE_LEN],
    pub raw: SnapshotRaw,
    /// BLAKE3-hex of the feature bytes (LE float32 layout). Excludes
    /// `ts` deliberately so identical context → identical hash, which
    /// is what the audit replay (Step 23) keys against.
    pub snapshot_hash: String,
}

/// Bundle of constructed sensors. Each field is `Option<_>` so the
/// daemon can skip a sensor that errored at construction time —
/// `collect_tick` then surfaces `f32::NAN` for that feature slot.
#[derive(Debug, Default)]
pub struct Sensors {
    pub pstate: Option<PstateSensor>,
    pub platform: Option<PlatformSensor>,
    pub hwmon: Option<HwmonSensor>,
    pub rapl: Option<RaplSensor>,
    pub igpu: Option<IgpuSensor>,
    pub npu: Option<NpuSensor>,
    pub battery: Option<BatterySensor>,
}

impl Sensors {
    /// Construct the canonical production set — every sensor type is
    /// stateless or self-initialising, so this never fails.
    pub fn all() -> Self {
        Self {
            pstate: Some(PstateSensor::new()),
            platform: Some(PlatformSensor::new()),
            hwmon: Some(HwmonSensor::new()),
            rapl: Some(RaplSensor::new()),
            igpu: Some(IgpuSensor::new()),
            npu: Some(NpuSensor::new()),
            battery: Some(BatterySensor::new()),
        }
    }
}

/// Bundle of constructed intent channels. Each field is `Option<_>`
/// so a channel that failed to construct (bus unreachable, socket
/// missing) is silently absent from the snapshot rather than blocking
/// the tick.
#[derive(Default)]
pub struct Intent {
    pub psi_cpu: Option<PsiChannel>,
    pub logind: Option<LogindChannel>,
    pub niri: Option<NiriChannel>,
    pub aiplane: Option<AiplaneIntentChannel>,
    pub mpris: Option<MprisChannel>,
    pub portal: Option<ScreenCastChannel>,
    pub idle: Option<IdleChannel>,
    pub cgroup: Option<CgroupAncestryChannel>,
    pub notify: Option<NotifyChannel>,
    pub time: TimeChannel,
}

/// Read every sensor + drain every intent channel into one
/// `Snapshot`. **Never panics**: a sensor that errors degrades the
/// matching feature slot to `f32::NAN`; an intent channel that
/// yielded no event leaves its slot at the documented default
/// (0.0 for coarse bools, NaN for continuous channels with no
/// last-known value).
pub fn collect_tick(
    sensors: &Sensors,
    intent: &mut Intent,
    clock: &dyn Clock,
    sysfs_root: &Path,
) -> Snapshot {
    let ts = clock.now();
    let mut features = [f32::NAN; FEATURE_LEN];
    let mut raw = SnapshotRaw::default();

    read_hwmon(&sensors.hwmon, sysfs_root, &mut features, &mut raw);
    read_rapl(&sensors.rapl, sysfs_root, &mut features, &mut raw);
    read_igpu(&sensors.igpu, sysfs_root, &mut features, &mut raw);
    read_npu(&sensors.npu, sysfs_root, &mut features, &mut raw);
    read_battery(&sensors.battery, sysfs_root, &mut features, &mut raw);
    // Keep pstate / platform sensors live: their readings feed Step 11's
    // `sy power status --json` payload through `raw`, not the GRU vec.
    let _ = sensors.pstate.as_ref().map(|s| s.read(sysfs_root));
    let _ = sensors.platform.as_ref().map(|s| s.read(sysfs_root));

    drain_intent(intent, &mut features, &mut raw);

    let snapshot_hash = hash_features(&features);
    Snapshot {
        schema: SCHEMA_ID,
        ts,
        features,
        raw,
        snapshot_hash,
    }
}

fn read_hwmon(
    sensor: &Option<HwmonSensor>,
    sysfs_root: &Path,
    features: &mut [f32; FEATURE_LEN],
    raw: &mut SnapshotRaw,
) {
    if let Some(s) = sensor {
        if let Ok(SensorReading::Hwmon(h)) = s.read(sysfs_root) {
            features[IDX_TCTL_C] = h.tctl_c;
            raw.tctl_c = Some(h.tctl_c);
            // Fallback for hosts where RAPL `energy_uj` is mode 0400
            // (Plundervolt mitigation): amdgpu's `power1_average` is
            // SoC-wide package power in microwatts. `read_rapl` runs
            // next; if it succeeds it overwrites with the CPU-package
            // 5-tap value, which is more authoritative.
            if let Some(uw) = h.package_power_uw {
                let w = (uw as f32) / UW_PER_W;
                features[IDX_PACKAGE_POWER_W] = w;
                raw.package_power_w = Some(w);
            }
        }
    }
}

fn read_rapl(
    sensor: &Option<RaplSensor>,
    sysfs_root: &Path,
    features: &mut [f32; FEATURE_LEN],
    raw: &mut SnapshotRaw,
) {
    if let Some(s) = sensor {
        if let Ok(SensorReading::Rapl(r)) = s.read(sysfs_root) {
            if let Some(w) = r.package_power_w_5tap {
                features[IDX_PACKAGE_POWER_W] = w;
                raw.package_power_w = Some(w);
            }
        }
    }
}

fn read_igpu(
    sensor: &Option<IgpuSensor>,
    sysfs_root: &Path,
    features: &mut [f32; FEATURE_LEN],
    raw: &mut SnapshotRaw,
) {
    if let Some(s) = sensor {
        if let Ok(SensorReading::Igpu(g)) = s.read(sysfs_root) {
            if let Some(pct) = g.busy_pct {
                features[IDX_IGPU_BUSY_PCT] = pct as f32;
                raw.igpu_busy_pct = Some(pct);
            }
        }
    }
}

fn read_npu(
    sensor: &Option<NpuSensor>,
    sysfs_root: &Path,
    features: &mut [f32; FEATURE_LEN],
    raw: &mut SnapshotRaw,
) {
    if let Some(s) = sensor {
        if let Ok(SensorReading::Npu(n)) = s.read(sysfs_root) {
            features[IDX_NPU_WORKLOADS] = n.workload_count as f32;
            raw.npu_workloads = Some(n.workload_count);
        }
    }
}

fn read_battery(
    sensor: &Option<BatterySensor>,
    sysfs_root: &Path,
    features: &mut [f32; FEATURE_LEN],
    raw: &mut SnapshotRaw,
) {
    if let Some(s) = sensor {
        if let Ok(SensorReading::Battery(b)) = s.read(sysfs_root) {
            if let Some(soc) = b.soc_pct {
                features[IDX_BATTERY_SOC_PCT] = soc as f32;
                raw.battery_soc_pct = Some(soc);
            }
            features[IDX_AC_ONLINE] = if b.ac_online { 1.0 } else { 0.0 };
            raw.ac_online = Some(b.ac_online);
            features[IDX_BATTERY_DRAIN_W] = b.drain_w;
            raw.battery_drain_w = Some(b.drain_w);
        }
    }
}

/// Drain one non-blocking `poll()` from every intent channel, fold
/// the resulting events into the feature vec + raw record. Coarse
/// bools (`call_active`, etc.) coalesce per SPEC §3 — any positive
/// signal across logind / portal / mpris promotes the
/// `call_active` slot to 1.0.
fn drain_intent(intent: &mut Intent, features: &mut [f32; FEATURE_LEN], raw: &mut SnapshotRaw) {
    let mut call_active = false;
    if let Some(ch) = intent.psi_cpu.as_mut() {
        if ch.poll().is_some() {
            // PSI fires on a leading-edge crossing of the 150 ms
            // threshold; absence of a kernel-reported intensity means
            // the panel uses the threshold itself as the magnitude.
            features[IDX_PSI_CPU_SOME_AVG10] = 100.0
                * (crate::power::intent::psi::DEFAULT_THRESHOLD_US as f32
                    / crate::power::intent::psi::DEFAULT_WINDOW_US as f32);
            raw.psi_cpu_some_avg10 = Some(features[IDX_PSI_CPU_SOME_AVG10]);
        }
    }
    if let Some(ch) = intent.logind.as_mut() {
        if matches!(ch.poll(), Some(IntentEvent::CallActive { .. })) {
            call_active = true;
        }
    }
    if let Some(ch) = intent.portal.as_mut() {
        if matches!(ch.poll(), Some(IntentEvent::ScreenCastActive)) {
            call_active = true;
        }
    }
    if let Some(ch) = intent.mpris.as_mut() {
        if matches!(ch.poll(), Some(IntentEvent::MediaPlaying)) {
            call_active = true;
        }
    }
    features[IDX_CALL_ACTIVE] = if call_active { 1.0 } else { 0.0 };
    raw.call_active = Some(call_active);

    if let Some(ch) = intent.niri.as_mut() {
        if let Some(IntentEvent::FocusedAppChanged { app_id }) = ch.poll() {
            raw.focused_app_id = Some(app_id);
        }
    }
    if let Some(ch) = intent.aiplane.as_mut() {
        if let Some(IntentEvent::NpuQueue { depth, .. }) = ch.poll() {
            features[IDX_NPU_WORKLOADS] = depth as f32;
            raw.npu_workloads = Some(depth as u32);
        }
    }
    if let Some(ch) = intent.idle.as_mut() {
        if let Some(IntentEvent::UserIdle { since_ms }) = ch.poll() {
            let s = since_ms as f32 / MS_PER_S;
            features[IDX_USER_IDLE_S] = s;
            raw.user_idle_s = Some(s);
        }
    }
    if let Some(ch) = intent.cgroup.as_mut() {
        // Drain to keep the dedup state moving; the matched name lives
        // in audit-only context (Step 23), not the GRU input.
        let _ = ch.poll();
    }
    if let Some(ch) = intent.notify.as_mut() {
        // FanComplaint is coalesced into the audit log only — no
        // dedicated feature slot in v1.
        let _ = ch.poll();
    }
    if let Some(IntentEvent::TimeOfDay {
        sin,
        cos,
        dow_sin,
        dow_cos,
    }) = intent.time.poll()
    {
        features[IDX_TOD_SIN] = sin;
        features[IDX_TOD_COS] = cos;
        raw.tod_sin = Some(sin);
        raw.tod_cos = Some(cos);
        raw.tod_dow_sin = Some(dow_sin);
        raw.tod_dow_cos = Some(dow_cos);
    }
}

/// BLAKE3 over the 48-byte little-endian feature vec. The hash is
/// pinned to feature bytes only (no `ts`, no `raw`) so identical
/// context across daemon restarts yields identical hashes — the
/// invariant Step 23 ("audit replay") depends on.
fn hash_features(features: &[f32; FEATURE_LEN]) -> String {
    let mut bytes = [0u8; FEATURE_LEN * 4];
    for (i, v) in features.iter().enumerate() {
        bytes[i * 4..(i + 1) * 4].copy_from_slice(&v.to_le_bytes());
    }
    blake3::hash(&bytes).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::clock::MockClock;
    use chrono::TimeZone;
    use std::path::PathBuf;

    fn hx370_fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/power/fixtures/sys/hx370")
    }

    /// Pin a test clock to the SPEC's reference snapshot — every
    /// test that exercises feature determinism uses this exact
    /// instant so the resulting hashes are documented.
    fn pinned_clock() -> MockClock {
        MockClock::new(
            Utc.with_ymd_and_hms(2026, 5, 19, 12, 0, 0)
                .single()
                .expect("pinned UTC instant"),
        )
    }

    /// Build a minimal `Sensors` bundle pointed at the hx370 fixture.
    /// The hwmon / igpu / battery sensors all parse the fixture; rapl
    /// returns `None` for `package_power_w_5tap` on first read (no
    /// delta yet) — covered by `package_power_w` staying NaN.
    fn hx370_sensors() -> Sensors {
        Sensors::all()
    }

    /// Construct an Intent bundle with only the `TimeChannel`
    /// populated (every other channel needs a bus / socket the test
    /// environment doesn't have). Snapshot output stays byte-stable
    /// because the `MockClock` pins the TimeOfDay encoding.
    fn time_only_intent() -> Intent {
        Intent::default()
    }

    /// Roadmap Step 8 DoD test: byte-stable feature vec under a
    /// frozen MockClock. Two `collect_tick` calls with fresh
    /// `Sensors`/`Intent` bundles + the same MockClock must produce
    /// identical `features` arrays and identical `snapshot_hash`
    /// values. (Stateful sensors like `RaplSensor` carry a 5-tap
    /// window; re-constructing them each call gives the same empty
    /// initial state — that is what "same input" means for the
    /// snapshot determinism contract.)
    #[test]
    fn feature_vec_is_deterministic_under_mock_clock() {
        let clock = pinned_clock();
        let fixture = hx370_fixture();

        let s1 = collect_tick(&hx370_sensors(), &mut time_only_intent(), &clock, &fixture);
        let s2 = collect_tick(&hx370_sensors(), &mut time_only_intent(), &clock, &fixture);

        // Compare bit-pattern (NaN ≠ NaN under PartialEq, so reach
        // into the underlying bytes — this is the invariant Step 9's
        // NDJSON writer depends on).
        for i in 0..FEATURE_LEN {
            assert_eq!(
                s1.features[i].to_bits(),
                s2.features[i].to_bits(),
                "feature[{i}] differs across deterministic calls: {} vs {}",
                s1.features[i],
                s2.features[i],
            );
        }
        assert_eq!(s1.snapshot_hash, s2.snapshot_hash);
        assert_eq!(s1.ts, s2.ts);
    }

    /// DoD test: when a sensor cannot find its sysfs node, the
    /// matching feature slot degrades to `f32::NAN` and the daemon
    /// does *not* crash. We force this by pointing the assembler at
    /// `/nonexistent`.
    #[test]
    fn missing_sensor_degrades_to_nan_not_panic() {
        let sensors = hx370_sensors();
        let clock = pinned_clock();
        let snap = collect_tick(
            &sensors,
            &mut time_only_intent(),
            &clock,
            Path::new("/nonexistent"),
        );

        // hwmon, rapl, igpu, npu, battery — all driven from sysfs.
        // Every one of their feature slots must be NaN now.
        for idx in [
            IDX_TCTL_C,
            IDX_PACKAGE_POWER_W,
            IDX_IGPU_BUSY_PCT,
            IDX_BATTERY_SOC_PCT,
            IDX_BATTERY_DRAIN_W,
        ] {
            assert!(
                snap.features[idx].is_nan(),
                "feature[{idx}] expected NaN under missing sysfs, got {}",
                snap.features[idx]
            );
        }
        // NPU sensor is the deterministic-zero stub — returns Ok even
        // when sysfs is absent.
        assert_eq!(snap.features[IDX_NPU_WORKLOADS], 0.0);
        // Time-of-day comes from the clock, not sysfs — always populated.
        assert!(!snap.features[IDX_TOD_SIN].is_nan());
        assert!(!snap.features[IDX_TOD_COS].is_nan());
    }

    /// DoD test: identical inputs ⇒ identical `snapshot_hash`. The
    /// hash excludes `ts` so the same feature payload across daemon
    /// restarts hashes identically (Step 23 replay invariant). Two
    /// fresh `Sensors` bundles model that "daemon restarted" shape:
    /// stateful sensors (RAPL window, niri slot) start empty in both
    /// calls.
    #[test]
    fn snapshot_hash_stable_across_runs() {
        let fixture = hx370_fixture();

        let c1 = pinned_clock();
        let s1 = collect_tick(&hx370_sensors(), &mut time_only_intent(), &c1, &fixture);
        let c2 = pinned_clock();
        let s2 = collect_tick(&hx370_sensors(), &mut time_only_intent(), &c2, &fixture);

        assert_eq!(s1.snapshot_hash, s2.snapshot_hash);
        // BLAKE3 hex is 64 chars.
        assert_eq!(s1.snapshot_hash.len(), 64);
        // And changing the input changes the hash — sanity check the
        // hash isn't a stub constant.
        let mut tweaked = s1.features;
        tweaked[IDX_TCTL_C] = if tweaked[IDX_TCTL_C].is_nan() {
            42.0
        } else {
            tweaked[IDX_TCTL_C] + 1.0
        };
        let h_tweaked = hash_features(&tweaked);
        assert_ne!(s1.snapshot_hash, h_tweaked);
    }

    /// SPEC §4 Privacy: the serialised snapshot must not contain any
    /// key named `title`, `body`, `keystroke`, or `clipboard`. The
    /// only string field on the schema is `focused_app_id`, which is
    /// stripped at the niri parser to app-id only.
    #[test]
    fn no_title_or_body_in_serialised_snapshot() {
        let sensors = hx370_sensors();
        let clock = pinned_clock();
        let snap = collect_tick(&sensors, &mut time_only_intent(), &clock, &hx370_fixture());
        let v = serde_json::to_value(&snap).expect("snapshot serialises");
        let banned = ["title", "body", "keystroke", "clipboard"];
        for needle in banned {
            assert!(
                !key_present(&v, needle),
                "snapshot must not surface key {needle:?} — found in {v:?}",
            );
        }
    }

    /// Step H1 DoD: sensors that fail produce `f32::NAN`; `serde_json`
    /// serialises NaN as JSON `null` and the default deserialiser
    /// rejects `null → f32`. The custom deserialiser restores NaN so
    /// the four read-side CLI surfaces (`sy power status --json`,
    /// `sy power log`, `sy power explain`, `sy power show --since`)
    /// can tail the live daemon's NDJSON losslessly.
    #[test]
    fn round_trips_through_serde_when_features_have_nan() {
        let mut snap = collect_tick(
            &hx370_sensors(),
            &mut time_only_intent(),
            &pinned_clock(),
            &hx370_fixture(),
        );
        snap.features[IDX_TCTL_C] = f32::NAN;
        let line = serde_json::to_string(&snap).expect("snapshot serialises");
        let back: Snapshot = serde_json::from_str(&line).expect("snapshot round-trips");
        assert!(
            back.features[IDX_TCTL_C].is_nan(),
            "NaN feature must round-trip as NaN, got {}",
            back.features[IDX_TCTL_C],
        );
    }

    /// Step H1 DoD: lock the on-disk shape — NaN serialises as JSON
    /// `null` so the 630+ entries already written by the live daemon
    /// stay parseable. Guards against a future drift to e.g. the
    /// string `"NaN"` which would silently break back-compat.
    #[test]
    fn nan_serializes_as_json_null_for_back_compat() {
        let mut snap = collect_tick(
            &hx370_sensors(),
            &mut time_only_intent(),
            &pinned_clock(),
            &hx370_fixture(),
        );
        snap.features[IDX_TCTL_C] = f32::NAN;
        let line = serde_json::to_string(&snap).expect("snapshot serialises");
        let v: serde_json::Value = serde_json::from_str(&line).expect("valid json");
        assert!(
            v["features"][IDX_TCTL_C].is_null(),
            "NaN slot must serialise as JSON null for NDJSON back-compat: {}",
            v["features"],
        );
    }

    /// Step 29 DoD: bumping the wire schema to `sy.power.snapshot/v2`
    /// is observable on the serialised snapshot — the `schema` slot now
    /// reads `v2`, and `raw.activity_label` is a documented key
    /// (`null` until the Step 29 daemon wiring populates it).
    #[test]
    fn v2_schema_includes_activity() {
        let snap = collect_tick(
            &hx370_sensors(),
            &mut time_only_intent(),
            &pinned_clock(),
            &hx370_fixture(),
        );
        let v = serde_json::to_value(&snap).expect("snapshot serialises");
        assert_eq!(
            v["schema"].as_str(),
            Some("sy.power.snapshot/v2"),
            "Step 29 bumps the wire schema to v2: {v}",
        );
        assert!(
            v["raw"].get("activity_label").is_some(),
            "v2 raw must surface the activity_label slot: {v}",
        );
    }

    /// Step P2-3 back-compat: a v2 NDJSON entry written before
    /// [`SnapshotRaw::activity_forecast`] existed must still
    /// deserialise — the field is `#[serde(default)]` so older log
    /// lines reconstruct cleanly with `activity_forecast = None`.
    #[test]
    fn activity_forecast_defaults_when_absent_from_json() {
        let snap = collect_tick(
            &hx370_sensors(),
            &mut time_only_intent(),
            &pinned_clock(),
            &hx370_fixture(),
        );
        let mut v = serde_json::to_value(&snap).expect("snapshot serialises");
        // Strip the new field from the wire JSON to emulate a v2
        // line that pre-dates Step P2-3.
        v.get_mut("raw")
            .and_then(|r| r.as_object_mut())
            .and_then(|m| m.remove("activity_forecast"));
        let back: Snapshot = serde_json::from_value(v).expect("v2 line round-trips");
        assert!(
            back.raw.activity_forecast.is_none(),
            "missing field must default to None: {:?}",
            back.raw.activity_forecast,
        );
    }

    /// Build a hwmon-only sysfs root: k10temp + amdgpu (with
    /// `power1_average`), no RAPL node. Exercises the production
    /// case on the live HX 370 host where `intel-rapl:0/energy_uj`
    /// is mode 0400 root:root (Plundervolt mitigation) so userspace
    /// must fall back to amdgpu's SoC-wide power sensor.
    fn write_hwmon_only_root(power_uw: u32) -> tempfile::TempDir {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let k10 = temp.path().join("class/hwmon/hwmon0");
        let amd = temp.path().join("class/hwmon/hwmon1");
        std::fs::create_dir_all(&k10).expect("mkdir k10");
        std::fs::create_dir_all(&amd).expect("mkdir amdgpu");
        std::fs::write(k10.join("name"), "k10temp\n").expect("name k10");
        std::fs::write(k10.join("temp1_input"), "82000\n").expect("tctl");
        std::fs::write(amd.join("name"), "amdgpu\n").expect("name amdgpu");
        std::fs::write(amd.join("temp1_input"), "60000\n").expect("edge");
        std::fs::write(amd.join("power1_average"), format!("{power_uw}\n")).expect("power");
        temp
    }

    /// Production bug fix: on the live HX 370, RAPL's `energy_uj` is
    /// mode 0400 root:root so `read_rapl` always fails — but amdgpu
    /// hwmon exposes `power1_average` as the SoC-wide package power
    /// in microwatts. `read_hwmon` must populate `raw.package_power_w`
    /// from that reading so the audit log and bandit context get a
    /// real number instead of `None`.
    #[test]
    fn amdgpu_power_populates_package_power_when_rapl_absent() {
        const POWER_UW: u32 = 13_069_000;
        const EXPECTED_W: f32 = 13.069;
        const EPS_W: f32 = 0.001;
        let root = write_hwmon_only_root(POWER_UW);
        let snap = collect_tick(
            &Sensors::all(),
            &mut time_only_intent(),
            &pinned_clock(),
            root.path(),
        );
        let got = snap
            .raw
            .package_power_w
            .expect("amdgpu must populate package_power_w");
        assert!(
            (got - EXPECTED_W).abs() < EPS_W,
            "package_power_w {got} expected ~{EXPECTED_W} (amdgpu fallback)",
        );
        assert!(
            (snap.features[IDX_PACKAGE_POWER_W] - EXPECTED_W).abs() < EPS_W,
            "features[IDX_PACKAGE_POWER_W] {} expected ~{EXPECTED_W}",
            snap.features[IDX_PACKAGE_POWER_W],
        );
    }

    /// RAPL is more authoritative for CPU package power (5-tap smooths
    /// spikes; amdgpu is SoC-wide on APUs). When both are readable the
    /// RAPL value must win — `read_rapl` runs after `read_hwmon` and
    /// overwrites the feature slot.
    #[test]
    fn rapl_overwrites_amdgpu_when_both_present() {
        const POWER_UW: u32 = 13_069_000;
        const AMDGPU_W: f32 = 13.069;
        let temp = write_hwmon_only_root(POWER_UW);
        // Add a RAPL node with a deliberately-distinct energy delta so
        // the resulting RAPL average is clearly NOT the amdgpu value.
        let rapl = temp.path().join("class/powercap/intel-rapl:0");
        std::fs::create_dir_all(&rapl).expect("mkdir rapl");
        std::fs::write(rapl.join("name"), "package-0\n").expect("rapl name");
        std::fs::write(rapl.join("max_energy_range_uj"), "262143328850\n").expect("max range");
        // Two reads against the same Sensors bundle build the RAPL
        // moving-average window. Seed energy then advance it.
        std::fs::write(rapl.join("energy_uj"), "1000000000\n").expect("seed energy");
        let sensors = Sensors::all();
        let _ = collect_tick(
            &sensors,
            &mut time_only_intent(),
            &pinned_clock(),
            temp.path(),
        );
        // Tick 2: advance energy so the delta yields a distinct power.
        // The instantaneous power lands at delta_uj / 1e6 / dt_s; dt is
        // the wall-clock gap between collect_tick calls, which is tiny
        // (microseconds), so the average will be MUCH larger than the
        // amdgpu 13.069 W — exactly the "RAPL value, not amdgpu" signal.
        std::fs::write(rapl.join("energy_uj"), "1100000000\n").expect("advance energy");
        let snap = collect_tick(
            &sensors,
            &mut time_only_intent(),
            &pinned_clock(),
            temp.path(),
        );
        let got = snap
            .raw
            .package_power_w
            .expect("rapl must populate package_power_w");
        const EPS_W: f32 = 0.001;
        assert!(
            (got - AMDGPU_W).abs() > EPS_W,
            "rapl must win over amdgpu, got {got} which matches the amdgpu value {AMDGPU_W}",
        );
    }

    fn key_present(v: &serde_json::Value, needle: &str) -> bool {
        match v {
            serde_json::Value::Object(map) => {
                for (k, val) in map {
                    if k.contains(needle) {
                        return true;
                    }
                    if key_present(val, needle) {
                        return true;
                    }
                }
                false
            }
            serde_json::Value::Array(arr) => arr.iter().any(|x| key_present(x, needle)),
            _ => false,
        }
    }
}
