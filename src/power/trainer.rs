//! Offline forecaster trainer — `retrain_gru(telemetry_path, out_path)`.
//!
//! Roadmap Step 25 (Phase R4) seeded the architecture; Step P2-1
//! (sy-power-production) replaces the original 2-layer MLP with the
//! real GRU forecaster the SPEC promised. The trainer reads the NDJSON
//! audit log (one `AuditEntry` per line), trains a tiny GRU classifier
//! that maps an 8-step rolling window of 12-channel snapshot feature
//! vectors to one of the five activity classes (`idle | browse | call |
//! code | build`), and exports the trained weights as an ONNX file the
//! Step-24 [`super::forecast::Model`] can hot-load through `ArcSwap`.
//!
//! ## Architecture (post-P2-1)
//!
//! Per SPEC §3 the forecaster is a "tiny GRU forecaster (~2-5k params,
//! tract-on-CPU sub-millisecond)". This trainer fits a single-layer
//! GRU with `input=12`, `hidden=16`, followed by a `Gemm(16 → 5) +
//! Softmax` classification head. Parameter budget:
//!
//! | layer     | shape         | params |
//! |-----------|---------------|--------|
//! | `W` (GRU) | `[3·16, 12]`  |    576 |
//! | `R` (GRU) | `[3·16, 16]`  |    768 |
//! | `B` (GRU) | `[6·16]`      |     96 |
//! | head W    | `[5, 16]`     |     80 |
//! | head b    | `[5]`         |      5 |
//! | **total** |               | **1525** |
//!
//! Well within the SPEC's 2-5k envelope.
//!
//! ## ONNX export
//!
//! We hand-emit the protobuf (Step 25 precedent; `burn-onnx` 0.21
//! still doesn't cover GRU cleanly). The emitted graph is:
//!
//! ```text
//! features[seq, 1, 12]
//!     │
//!     ▼
//!   GRU (W, R, B) ──► Y_h[1, 1, 16]
//!                       │
//!                       ▼
//!                    Squeeze(axes=[0,1]) ──► [16]
//!                       │
//!                       ▼
//!                    Unsqueeze(axes=[0]) ──► [1, 16]
//!                       │
//!                       ▼
//!                    Gemm(W_head, b_head) ──► [1, 5]
//!                       │
//!                       ▼
//!                    Softmax(axis=-1) ──► probs[1, 5]
//! ```
//!
//! Tract 0.22's `GRU` op is the canonical native operator (no Scan
//! fallback needed). Gate order in `W`/`R`/`B` follows the ONNX spec:
//! `[z, r, h]` (update, reset, candidate). Bias is concatenated
//! `[Wb_z, Wb_r, Wb_h, Rb_z, Rb_r, Rb_h]`.
//!
//! ## Training
//!
//! Hand-rolled forward + backward in pure `Vec<f32>` per CLUCB / FTRL
//! precedent (Steps 20 / 28 / 30). No burn / autodiff dep on the
//! GRU — the burn surface doesn't ship a clean ONNX export path, and
//! the tiny model is small enough that a pure-Rust trainer is cheaper
//! than wiring up burn's `Gru` plus a side-channel weight extractor.
//!
//! Training rolls the NDJSON corpus into 8-step windows. The label is
//! the activity class of the LAST row in each window — that's the
//! daemon's "predict next-30-s" target. Validation accuracy is over the
//! last 20 % of windows held out from training.
//!
//! ## Label mapping (per Step 25 implementation guidance)
//!
//! Activity class is derived from the entry's `applied_arm`. The
//! mapping collapses the eight canonical arms onto the five-class
//! taxonomy:
//!
//! | `applied_arm`         | activity class | rationale                              |
//! |-----------------------|----------------|----------------------------------------|
//! | `idle` / `whisper`    | `idle`         | both are deep-idle / battery-saver     |
//! | `browse`              | `browse`       | direct                                 |
//! | `call`                | `call`         | direct                                 |
//! | `code`                | `code`         | direct                                 |
//! | `build` / `flat-out` / `npu-burst` | `build` | sustained-load workloads        |
//! | `None` / unknown      | dropped        | unlabelled rows are skipped, not zeroed|
//!
//! ## Validation gate (CI invariant, SPEC §6 risk table)
//!
//! After the trainer hand-emits the ONNX bytes it IMMEDIATELY
//! re-decodes them through [`super::forecast::Model::from_onnx_bytes`]
//! AND runs a single inference. If either step errors the trainer
//! returns [`TrainerError::ValidationFailed`] and refuses to write
//! to `out_path` — the daemon's live model is preserved.
//!
//! ### Lint scope
//!
//! The hand-rolled GRU + BPTT kernels are matrix-major numeric loops
//! that index `Vec`s by `[gate][hidden][input]` triples. Clippy's
//! `needless_range_loop` lint would rewrite every triple-nested loop
//! into `.iter().enumerate()` chains that obscure the maths; we
//! suppress it at the module level the same way burn / candle / linfa
//! do for their dense-tensor kernels. `useless_vec` is similarly
//! suppressed: the GRU pre-act buffers are `vec![0.0; HIDDEN_DIM]` and
//! stay heap-allocated so the BPTT borrows can swap them safely
//! without `let _ = mem::replace(...)` gymnastics.
#![allow(clippy::needless_range_loop, clippy::useless_vec)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use prost::Message;
use tract_onnx::pb;

use crate::power::forecast::model::{Model, ACTIVITY_CLASSES, FORECAST_CLASS_COUNT};
use crate::power::log::AuditEntry;
use crate::power::snapshot::FEATURE_LEN;

/// Hidden width of the trainer's GRU — matches SPEC §3 "16-hidden".
/// Combined with the 12-feature input and 5-class head this lands the
/// model at 1525 params (see module docs), well inside the SPEC's
/// 2-5k envelope.
const HIDDEN_DIM: usize = 16;

/// GRU gate count — `[z, r, h]` per the ONNX spec.
const GATE_COUNT: usize = 3;

/// Number of bias slabs the ONNX `GRU` op packs into the `B` tensor —
/// three for the input projection (`Wb_z`, `Wb_r`, `Wb_h`) and three
/// for the recurrent projection (`Rb_z`, `Rb_r`, `Rb_h`).
const BIAS_SLAB_COUNT: usize = 6;

/// Rolling-window length the trainer feeds the GRU. Eight 1 Hz ticks
/// at the daemon's cadence give the recurrent path enough room to
/// learn 30-120 s temporal structure (SPEC §3 horizon) without
/// stretching the training corpus past what a 200-row synthetic test
/// can support.
const WINDOW_LEN: usize = 8;

/// Minimum number of labelled rows the trainer accepts. Below this
/// the windowed corpus is too small to generalise; we surface
/// [`TrainerError::InsufficientData`] so the daemon stays on the
/// rules-baseline warmup model.
const MIN_TRAINING_ROWS: usize = 32;

/// Per-class row floor for the trainer's coverage gate (Step T3 /
/// BUG-20260525-2352). The hand-rolled SGD assigns a separate softmax
/// head to every class; without exemplars a head's weights drift to
/// zero and the validation accuracy hides the deficit behind the
/// dominant class's argmax wins. 16 rows is sized to give the per-class
/// FTRL head a non-trivial gradient signal across [`EPOCHS`] passes
/// while staying low enough that a sparsely-distributed class
/// (e.g. `Call` once the rules baseline lets it through) is not
/// preemptively rejected.
const MIN_ROWS_PER_CLASS: usize = 16;

/// Minimum number of classes that must clear [`MIN_ROWS_PER_CLASS`]
/// for a train to proceed (BUG-20260723-2210). A softmax head over a
/// single class is degenerate — it can only ever predict that class —
/// so below two covered classes the trainer still refuses. Above the
/// floor, under-covered classes are *excluded* (rows dropped, exclusion
/// reported) instead of blocking the train: labels derive from the
/// applied arm, so a class the rules baseline never applies would
/// otherwise block the first model forever and keep the drift alarm
/// latched.
const MIN_COVERED_CLASSES: usize = 2;

/// Per-class recall floor enforced by [`run_validation`] (Step T3 /
/// BUG-20260525-2352). Half the trivially-attainable "majority class"
/// baseline — a model that scores recall < 0.5 on a class with training
/// exemplars is worse than coin-flip, so the trainer refuses to ship it
/// and the daemon stays on the previous (or warmup) model.
const MIN_PER_CLASS_RECALL: f32 = 0.5;

/// Validation split — last 20% of the labelled windows form the
/// held-out set for [`TrainerReport::validation_accuracy`].
const VALIDATION_FRACTION: f32 = 0.2;

/// SGD learning rate. Tuned against the synthetic temporal-pattern
/// test: 0.5 with `lr / batch_size` normalisation lands per-sample
/// updates around 3e-3 on a 150-window batch, which is enough to drive
/// loss below 0.5 on a linearly-separable corpus AND learn the
/// temporal cue on the noise-suffix synthetic in `EPOCHS` passes.
const LEARNING_RATE: f32 = 0.5;

/// Training epochs per `retrain_gru` invocation. 300 epochs of full-
/// batch SGD lands both synthetic tests reliably while keeping
/// wall-time well under the SPEC's 60-s budget on Zen5.
const EPOCHS: usize = 300;

/// Deterministic seed for the GRU weight initialiser. Trainer runs are
/// reproducible per the SPEC §6 reproducibility invariant; the seed is
/// pinned rather than drawn from `OsRng` so a re-run on the same
/// corpus produces byte-identical ONNX.
const INIT_SEED: u64 = 0x5359_504F_5752_4754; // "SYPOWRGT"

/// ONNX IR + opset versions, kept lockstep with
/// `examples/gen_warmup_gru.rs` so both fixtures decode through the
/// same tract path.
const IR_VERSION: i64 = 7;
const OPSET_VERSION: i64 = 13;
const TENSOR_FLOAT: i32 = 1;
const TENSOR_INT64: i32 = 7;
const ATTR_INT: i32 = 2;
const ATTR_FLOAT: i32 = 1;

/// Stable version-SHA prefix the audit log + `sy power status`
/// surface. Computed BLAKE3-style over the trained ONNX bytes; the
/// first 12 hex chars match Step 24's `Model::from_onnx_bytes`
/// fingerprint.
const VERSION_SHA_LEN: usize = 12;

/// Concrete outcome of one [`retrain_gru`] call. Surfaced verbatim
/// by `sy power train` (and later by the daemon's retrain scheduler
/// in Step 26).
#[derive(Debug, Clone)]
pub struct TrainerReport {
    pub epochs: usize,
    pub final_loss: f32,
    pub validation_accuracy: f32,
    pub wall_time_ms: u128,
    pub version_sha: String,
    pub rows_used: usize,
    /// Classes left out of this train because they were under the
    /// [`MIN_ROWS_PER_CLASS`] floor (BUG-20260723-2210). Surfaced on
    /// `sy power status` as `model.missing_classes` so a partial model
    /// never hides its blind spots.
    pub excluded_classes: Vec<&'static str>,
}

/// Reasons [`retrain_gru`] can refuse a write. The variants line up
/// with `sy power train`'s stable exit codes: every variant except
/// `ValidationFailed` exits 1; `ValidationFailed` is the dedicated
/// "would have shipped a broken model" path that the daemon will
/// later treat as "stay on the current ArcSwap pointer".
#[derive(Debug)]
pub enum TrainerError {
    /// `telemetry_path` couldn't be opened, or a line couldn't be
    /// parsed. The trainer keeps strict semantics here — a corrupt
    /// log surfaces as an error rather than a silent skip.
    ReadFailed(std::io::Error),
    /// Fewer than [`MIN_TRAINING_ROWS`] labelled rows after applying
    /// the `applied_arm → activity-class` mapping. The daemon should
    /// keep the rules-baseline warmup model and surface the gap on
    /// `sy power status`.
    InsufficientData { required: usize, found: usize },
    /// Fewer than [`MIN_COVERED_CLASSES`] activity classes clear the
    /// [`MIN_ROWS_PER_CLASS`] row floor (BUG-20260723-2210) — too few
    /// to train a non-degenerate softmax head. `missing` names every
    /// under-floor class. The daemon stays on the rules-baseline /
    /// warmup model and `sy power status` surfaces the missing
    /// classes for operator visibility.
    InsufficientClassCoverage {
        counts: [usize; FORECAST_CLASS_COUNT],
        missing: Vec<&'static str>,
        required: usize,
    },
    /// Forward / backward pass diverged (NaN loss, etc.). Rare on
    /// the tiny model but surfaced rather than masked.
    TrainFailed(String),
    /// ONNX export step itself errored (encoding bug, weight shape
    /// mismatch). Distinct from `ValidationFailed`, which is the
    /// "exported bytes but tract refused them" case.
    ExportFailed(String),
    /// Tract refused to decode the freshly-exported ONNX, or the
    /// post-decode inference errored. The CI gate per SPEC §6 risk
    /// table; the trainer does NOT write to `out_path` and the live
    /// model on disk (if any) is preserved.
    ValidationFailed(String),
}

impl std::fmt::Display for TrainerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadFailed(e) => write!(f, "trainer: read telemetry log failed: {e}"),
            Self::InsufficientData { required, found } => write!(
                f,
                "trainer: insufficient labelled rows: need {required}, found {found}"
            ),
            Self::InsufficientClassCoverage {
                counts,
                missing,
                required,
            } => write!(
                f,
                "trainer: insufficient per-class coverage: missing {missing:?} (need ≥ {required} rows each; counts={counts:?})",
            ),
            Self::TrainFailed(e) => write!(f, "trainer: training diverged: {e}"),
            Self::ExportFailed(e) => write!(f, "trainer: ONNX export failed: {e}"),
            Self::ValidationFailed(e) => write!(f, "trainer: tract validation rejected ONNX: {e}"),
        }
    }
}

impl std::error::Error for TrainerError {}

impl From<std::io::Error> for TrainerError {
    fn from(e: std::io::Error) -> Self {
        Self::ReadFailed(e)
    }
}

/// Sink used to publish the trained ONNX bytes. Production wires
/// [`FileSink`]; tests inject `TruncatingSink` (or any other
/// `dyn TrainerExportSink`) so the validation gate can be driven
/// without racing against the real filesystem.
pub trait TrainerExportSink {
    /// Hand the trained bytes off to the sink. The trainer calls
    /// this exactly once per `retrain_gru`. Returning `Err` propagates
    /// as [`TrainerError::ExportFailed`].
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    /// Final bytes that round-trip through the tract validation
    /// gate. The sink controls these so tests can intentionally
    /// corrupt them after `write` but before validation, exercising
    /// the [`TrainerError::ValidationFailed`] path.
    fn validation_bytes(&self) -> Vec<u8>;
}

/// Production sink — writes the ONNX bytes to `out_path` atomically
/// (write to `<out_path>.tmp` then `rename`) so a crashed trainer
/// never leaves a half-written `model.onnx` for the daemon to
/// half-load. Validation happens *before* the rename, off the bytes
/// the sink still holds in memory.
pub struct FileSink {
    out_path: PathBuf,
    bytes: Vec<u8>,
}

impl FileSink {
    /// Construct a sink pointed at `out_path`. The file is not
    /// touched until [`FileSink::commit`] is called.
    pub fn new(out_path: PathBuf) -> Self {
        Self {
            out_path,
            bytes: Vec::new(),
        }
    }

    /// Commit the buffered bytes to `out_path` atomically. Called
    /// only after the tract validation gate passes — preserves any
    /// previously-shipped model when the trainer aborts mid-flight.
    pub fn commit(&self) -> std::io::Result<()> {
        if let Some(parent) = self.out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.out_path.with_extension("onnx.tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&self.bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.out_path)?;
        Ok(())
    }
}

impl TrainerExportSink for FileSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.bytes = bytes.to_vec();
        Ok(())
    }

    fn validation_bytes(&self) -> Vec<u8> {
        self.bytes.clone()
    }
}

/// Train a fresh forecaster from `telemetry_path` and write the ONNX
/// to `out_path` via the production [`FileSink`]. Returns a
/// [`TrainerReport`] on success.
pub fn retrain_gru(telemetry_path: &Path, out_path: &Path) -> Result<TrainerReport, TrainerError> {
    let mut sink = FileSink::new(out_path.to_path_buf());
    let report = retrain_with_sink(telemetry_path, &mut sink)?;
    sink.commit().map_err(|e| {
        TrainerError::ExportFailed(format!("commit to {}: {e}", out_path.display()))
    })?;
    Ok(report)
}

/// Same as [`retrain_gru`] but pipes the ONNX bytes through a
/// caller-supplied sink. Used by the `abort_when_validation_fails`
/// test to corrupt the bytes between export and validation; never
/// called from the production CLI path.
pub fn retrain_with_sink(
    telemetry_path: &Path,
    sink: &mut dyn TrainerExportSink,
) -> Result<TrainerReport, TrainerError> {
    let started = Instant::now();
    let rows = read_labelled_rows(telemetry_path)?;
    if rows.len() < MIN_TRAINING_ROWS {
        return Err(TrainerError::InsufficientData {
            required: MIN_TRAINING_ROWS,
            found: rows.len(),
        });
    }
    let counts = class_counts(&rows);
    let excluded: Vec<&'static str> = ACTIVITY_CLASSES
        .iter()
        .enumerate()
        .filter(|(idx, _)| counts[*idx] < MIN_ROWS_PER_CLASS)
        .map(|(_, name)| *name)
        .collect();
    // BUG-20260723-2210: under-covered classes are excluded rather than
    // fatal — but a softmax over < MIN_COVERED_CLASSES covered classes
    // is degenerate, so that floor still hard-rejects.
    if ACTIVITY_CLASSES.len() - excluded.len() < MIN_COVERED_CLASSES {
        return Err(TrainerError::InsufficientClassCoverage {
            counts,
            missing: excluded,
            required: MIN_ROWS_PER_CLASS,
        });
    }
    let rows: Vec<LabelledRow> = rows
        .into_iter()
        .filter(|r| counts[r.class_idx] >= MIN_ROWS_PER_CLASS)
        .collect();
    let windows = build_windows(&rows);
    if windows.len() < MIN_TRAINING_ROWS {
        return Err(TrainerError::InsufficientData {
            required: MIN_TRAINING_ROWS,
            found: windows.len(),
        });
    }
    let (train, validate) = split_train_validate(&windows);
    // BUG-20260723-2352: SGD runs on standardised features; the raw
    // `validate` windows go through the exported graph, which carries
    // the same constants as a Sub/Mul prefix — so validation exercises
    // the exact raw-features-in contract the daemon uses.
    let stats = FeatureStats::from_rows(&rows);
    let train_norm: Vec<LabelledWindow> = train
        .iter()
        .map(|w| {
            let mut seq = [[0.0f32; FEATURE_LEN]; WINDOW_LEN];
            for (slot, step) in seq.iter_mut().zip(w.seq.iter()) {
                *slot = stats.apply(step);
            }
            LabelledWindow {
                seq,
                class_idx: w.class_idx,
            }
        })
        .collect();
    let trained = train_gru(&train_norm).map_err(TrainerError::TrainFailed)?;
    let final_loss = trained.last_loss;
    let bytes = export_onnx(&trained.weights, &stats)
        .map_err(|e| TrainerError::ExportFailed(e.to_string()))?;
    sink.write(&bytes)
        .map_err(|e| TrainerError::ExportFailed(e.to_string()))?;
    let bytes_for_validation = sink.validation_bytes();
    let model = Model::from_onnx_bytes(&bytes_for_validation)
        .map_err(|e| TrainerError::ValidationFailed(format!("decode: {e:#}")))?;
    let validation_accuracy =
        run_validation(&model, &validate).map_err(TrainerError::ValidationFailed)?;
    let version_sha = blake3::hash(&bytes_for_validation).to_hex()[..VERSION_SHA_LEN].to_string();
    Ok(TrainerReport {
        epochs: EPOCHS,
        final_loss,
        validation_accuracy,
        wall_time_ms: started.elapsed().as_millis(),
        version_sha,
        rows_used: rows.len(),
        excluded_classes: excluded,
    })
}

/// Per-feature z-score constants (BUG-20260723-2352). The daemon logs
/// RAW sensor values (tctl in °C, package power in W, `user_idle_s`
/// up to five digits) — feeding them to the GRU unnormalized
/// saturates every gate and the model collapses to the majority
/// class. The trainer standardises features before SGD and bakes the
/// SAME constants into the exported ONNX as a `Sub`/`Mul` prefix, so
/// the on-disk model still consumes raw features and the daemon's
/// inference path needs no knowledge of the normalisation.
#[derive(Debug, Clone)]
struct FeatureStats {
    mean: [f32; FEATURE_LEN],
    /// `1/std`, or `0.0` for a zero-variance feature — the multiply
    /// zeroes a constant column instead of amplifying float dust,
    /// and stays division-free inside the ONNX graph.
    inv_std: [f32; FEATURE_LEN],
}

impl FeatureStats {
    /// Compute mean / inverse-std over the labelled corpus.
    fn from_rows(rows: &[LabelledRow]) -> Self {
        let n = rows.len().max(1) as f32;
        let mut mean = [0.0f32; FEATURE_LEN];
        for row in rows {
            for (m, v) in mean.iter_mut().zip(row.features.iter()) {
                *m += v / n;
            }
        }
        let mut inv_std = [0.0f32; FEATURE_LEN];
        for (i, slot) in inv_std.iter_mut().enumerate() {
            let var: f32 = rows
                .iter()
                .map(|r| {
                    let d = r.features[i] - mean[i];
                    d * d / n
                })
                .sum();
            let std = var.sqrt();
            *slot = if std > 1e-6 { 1.0 / std } else { 0.0 };
        }
        Self { mean, inv_std }
    }

    /// Identity transform — used by tests that drive `train_gru` /
    /// `export_onnx` directly with already-normalised fixtures.
    #[cfg(test)]
    fn identity() -> Self {
        Self {
            mean: [0.0; FEATURE_LEN],
            inv_std: [1.0; FEATURE_LEN],
        }
    }

    /// Standardise one feature vector.
    fn apply(&self, features: &[f32; FEATURE_LEN]) -> [f32; FEATURE_LEN] {
        let mut out = [0.0f32; FEATURE_LEN];
        for i in 0..FEATURE_LEN {
            out[i] = (features[i] - self.mean[i]) * self.inv_std[i];
        }
        out
    }
}

/// One training row: feature vector + activity class index. Lifted
/// out of the NDJSON parse so the trainer's tensor build doesn't
/// hold on to the full `AuditEntry` shape.
#[derive(Debug, Clone)]
struct LabelledRow {
    features: [f32; FEATURE_LEN],
    class_idx: usize,
}

/// One training window: `WINDOW_LEN` rows + the class label of the
/// last row. The GRU consumes the whole sequence and predicts the
/// final-row class.
#[derive(Debug, Clone)]
struct LabelledWindow {
    seq: [[f32; FEATURE_LEN]; WINDOW_LEN],
    class_idx: usize,
}

/// Read the labelled corpus at `path`, projecting `applied_arm` onto
/// the five-class taxonomy and dropping unlabelled rows.
///
/// `path` is either a single NDJSON file (the `sy power train
/// --telemetry <file>` CLI path + every trainer test) or the daemon's
/// telemetry *directory*. Live telemetry is daily-segmented
/// (`telemetry-YYYY-MM-DD.ndjson` plus overflow `…​.N.ndjson`, commit
/// 22df459), so a directory is read as the ordered concatenation of its
/// `telemetry-*.ndjson` segments (see [`telemetry_segments`]). A
/// directory with zero matching segments yields zero rows, which the
/// caller surfaces as [`TrainerError::InsufficientData`]
/// (BUG-20260712-1545).
fn read_labelled_rows(path: &Path) -> Result<Vec<LabelledRow>, TrainerError> {
    if fs::metadata(path)?.is_dir() {
        let mut out = Vec::new();
        for segment in telemetry_segments(path)? {
            out.extend(read_labelled_rows_from_file(&segment)?);
        }
        return Ok(out);
    }
    read_labelled_rows_from_file(path)
}

/// Collect the `telemetry-*.ndjson` segment files in `dir`, sorted
/// chronologically. The date in the filename is ISO-8601 so a plain
/// lexicographic sort orders days correctly; the overflow index of a
/// single day (`telemetry-<date>.<N>.ndjson`) is parsed as an integer
/// so the base file (index 0) sorts first and `.10` sorts after `.2`.
/// Files that don't match the segment shape (checkpoint.json,
/// forecaster.onnx, reports/, …) are skipped (BUG-20260712-1545).
fn telemetry_segments(dir: &Path) -> Result<Vec<PathBuf>, TrainerError> {
    let mut segments: Vec<(String, u64, PathBuf)> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Some((date, overflow)) = segment_sort_key(name) {
            segments.push((date, overflow, entry.path()));
        }
    }
    segments.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    Ok(segments.into_iter().map(|(_, _, p)| p).collect())
}

/// Parse a telemetry segment filename into its `(date, overflow-index)`
/// sort key, or `None` when `name` isn't a `telemetry-*.ndjson` segment.
/// The base file (`telemetry-<date>.ndjson`) has overflow index 0;
/// overflow segments (`telemetry-<date>.<N>.ndjson`) carry `N`.
fn segment_sort_key(name: &str) -> Option<(String, u64)> {
    let rest = name.strip_prefix("telemetry-")?.strip_suffix(".ndjson")?;
    match rest.rsplit_once('.') {
        Some((date, idx)) if !idx.is_empty() && idx.bytes().all(|b| b.is_ascii_digit()) => {
            Some((date.to_string(), idx.parse::<u64>().ok()?))
        }
        _ => Some((rest.to_string(), 0)),
    }
}

/// Shield states whose arm application is a *reaction to conditions*
/// rather than a read on user activity (BUG-20260723-2352): the HOT
/// clamp forces the idle arm on a flat-out machine and the
/// BATTERY_LOW clamp forces the whisper arm regardless of what the
/// user is doing, so labelling those rows by their applied arm
/// poisons the class with contradictory feature vectors. `MEETING` is
/// deliberately NOT here — the meeting shield fires off the
/// call-active sensor, so its rows are genuine `call` evidence.
const ACTIVITY_CLAMPING_SHIELDS: [&str; 2] = ["HOT", "BATTERY_LOW"];

/// Read every `AuditEntry` line out of a single NDJSON file, project
/// `applied_arm` onto the five-class taxonomy, drop unlabelled rows
/// and rows clamped by a condition-reactive shield.
fn read_labelled_rows_from_file(path: &Path) -> Result<Vec<LabelledRow>, TrainerError> {
    let f = fs::File::open(path)?;
    let mut out = Vec::new();
    for line_res in BufReader::new(f).lines() {
        let line = line_res?;
        if line.trim().is_empty() {
            continue;
        }
        // Best-effort: skip lines that don't parse as an AuditEntry
        // (e.g. the `rotated:size_cap` marker the logger emits at the
        // daily cap).
        let entry: AuditEntry = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let Some(arm) = entry.applied_arm.as_deref() else {
            continue;
        };
        if entry
            .shield_state
            .as_deref()
            .is_some_and(|s| ACTIVITY_CLAMPING_SHIELDS.contains(&s))
        {
            continue;
        }
        let Some(class_idx) = arm_to_class_idx(arm) else {
            continue;
        };
        out.push(LabelledRow {
            features: entry.snapshot.features,
            class_idx,
        });
    }
    Ok(out)
}

/// Count labelled rows per activity class. Drives the Step T3
/// coverage gate so the daemon can surface the gap on
/// `sy power status` instead of training a softmax head whose missing
/// classes silently collapse to zero gradient.
fn class_counts(rows: &[LabelledRow]) -> [usize; FORECAST_CLASS_COUNT] {
    let mut counts = [0usize; FORECAST_CLASS_COUNT];
    for row in rows {
        if row.class_idx < FORECAST_CLASS_COUNT {
            counts[row.class_idx] += 1;
        }
    }
    counts
}

/// Project a canonical arm name onto the five-class taxonomy
/// documented in the module header.
fn arm_to_class_idx(arm: &str) -> Option<usize> {
    let class_name = match arm {
        "idle" | "whisper" => "idle",
        "browse" => "browse",
        "call" => "call",
        "code" => "code",
        "build" | "flat-out" | "npu-burst" => "build",
        _ => return None,
    };
    ACTIVITY_CLASSES.iter().position(|c| *c == class_name)
}

/// Slide a `WINDOW_LEN`-row window across the corpus, one window per
/// possible start position. The window's label is the class of its
/// final row — i.e. "given the last 8 ticks, what is the activity
/// class at tick t?", which matches the daemon's per-tick prediction
/// target.
fn build_windows(rows: &[LabelledRow]) -> Vec<LabelledWindow> {
    if rows.len() < WINDOW_LEN {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(rows.len() - WINDOW_LEN + 1);
    for start in 0..=rows.len() - WINDOW_LEN {
        let mut seq = [[0.0f32; FEATURE_LEN]; WINDOW_LEN];
        for (i, slot) in seq.iter_mut().enumerate() {
            *slot = rows[start + i].features;
        }
        let class_idx = rows[start + WINDOW_LEN - 1].class_idx;
        out.push(LabelledWindow { seq, class_idx });
    }
    out
}

/// Hold the last 20% out for validation. The trainer never sees
/// these windows in the forward/backward loop; the report's
/// `validation_accuracy` is computed over them.
fn split_train_validate(windows: &[LabelledWindow]) -> (Vec<LabelledWindow>, Vec<LabelledWindow>) {
    let n = windows.len();
    let split = ((n as f32) * (1.0 - VALIDATION_FRACTION)).round() as usize;
    let split = split.clamp(1, n);
    let train = windows[..split].to_vec();
    let validate = windows[split..].to_vec();
    (train, validate)
}

// ---------------------------------------------------------------------------
// Hand-rolled GRU forward + backward.
// ---------------------------------------------------------------------------

/// Trained GRU weights. Layout follows the ONNX `GRU` op spec — the
/// `W` / `R` slabs are gate-major `[z, r, h]` so the export step can
/// flatten them straight into a single tensor.
///
/// Shapes:
/// - `w_x[gate][hidden][input]` — 3 × 16 × 12
/// - `w_h[gate][hidden][hidden]` — 3 × 16 × 16
/// - `wb[gate][hidden]` — 3 × 16 (input-projection bias)
/// - `rb[gate][hidden]` — 3 × 16 (recurrent-projection bias)
/// - `head_w[class][hidden]` — 5 × 16
/// - `head_b[class]` — 5
#[derive(Debug, Clone)]
struct GruWeights {
    w_x: Vec<Vec<Vec<f32>>>,
    w_h: Vec<Vec<Vec<f32>>>,
    wb: Vec<Vec<f32>>,
    rb: Vec<Vec<f32>>,
    head_w: Vec<Vec<f32>>,
    head_b: Vec<f32>,
}

/// Owned mutable gradient buffer mirroring [`GruWeights`].
#[derive(Debug, Clone)]
struct GruGrads {
    w_x: Vec<Vec<Vec<f32>>>,
    w_h: Vec<Vec<Vec<f32>>>,
    wb: Vec<Vec<f32>>,
    rb: Vec<Vec<f32>>,
    head_w: Vec<Vec<f32>>,
    head_b: Vec<f32>,
}

impl GruGrads {
    fn zeros() -> Self {
        Self {
            w_x: zeros3(GATE_COUNT, HIDDEN_DIM, FEATURE_LEN),
            w_h: zeros3(GATE_COUNT, HIDDEN_DIM, HIDDEN_DIM),
            wb: zeros2(GATE_COUNT, HIDDEN_DIM),
            rb: zeros2(GATE_COUNT, HIDDEN_DIM),
            head_w: zeros2(FORECAST_CLASS_COUNT, HIDDEN_DIM),
            head_b: vec![0.0; FORECAST_CLASS_COUNT],
        }
    }
}

fn zeros2(d0: usize, d1: usize) -> Vec<Vec<f32>> {
    vec![vec![0.0; d1]; d0]
}

fn zeros3(d0: usize, d1: usize, d2: usize) -> Vec<Vec<Vec<f32>>> {
    vec![vec![vec![0.0; d2]; d1]; d0]
}

/// Cached intermediates from a single forward pass, kept so the
/// backward pass doesn't have to redo the GRU step-by-step.
struct ForwardCache {
    zt: Vec<Vec<f32>>, // [T][H]
    rt: Vec<Vec<f32>>, // [T][H]
    nt: Vec<Vec<f32>>, // [T][H] (candidate, post-tanh)
    ht: Vec<Vec<f32>>, // [T+1][H] (h0 .. hT)
    probs: Vec<f32>,   // [C]
}

/// Outcome of the training loop. Owns the extracted weights so the
/// ONNX export step doesn't depend on any external types.
struct TrainedGru {
    weights: GruWeights,
    last_loss: f32,
}

/// Tiny linear-congruential PRNG. Deterministic from [`INIT_SEED`];
/// pure-Rust replacement for `rand::Rng` (the trainer's RNG dep was
/// previously inherited from burn).
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xDEAD_BEEF_CAFE_F00D)
    }

    /// Next f32 in `(-bound, bound)`. The 24-bit slice keeps the
    /// mantissa in float range without bias.
    fn next_uniform(&mut self, bound: f32) -> f32 {
        // Numerical Recipes LCG constants — adequate for weight init.
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let bits = (self.0 >> 40) as u32; // top 24 bits
        let normalised = (bits as f32) / ((1u32 << 24) as f32); // [0, 1)
        (normalised * 2.0 - 1.0) * bound
    }
}

/// Glorot-style symmetric initialiser. Scale is `sqrt(6 / (fan_in +
/// fan_out))`; for a 16-hidden GRU that lands ~0.43 which is the
/// standard starting point.
fn init_uniform(rng: &mut Lcg, fan_in: usize, fan_out: usize) -> f32 {
    let bound = (6.0_f32 / (fan_in + fan_out) as f32).sqrt();
    rng.next_uniform(bound)
}

fn init_weights() -> GruWeights {
    let mut rng = Lcg::new(INIT_SEED);
    let mut w_x = zeros3(GATE_COUNT, HIDDEN_DIM, FEATURE_LEN);
    let mut w_h = zeros3(GATE_COUNT, HIDDEN_DIM, HIDDEN_DIM);
    for g in 0..GATE_COUNT {
        for h in 0..HIDDEN_DIM {
            for f in 0..FEATURE_LEN {
                w_x[g][h][f] = init_uniform(&mut rng, FEATURE_LEN, HIDDEN_DIM);
            }
            for hh in 0..HIDDEN_DIM {
                w_h[g][h][hh] = init_uniform(&mut rng, HIDDEN_DIM, HIDDEN_DIM);
            }
        }
    }
    let mut head_w = zeros2(FORECAST_CLASS_COUNT, HIDDEN_DIM);
    for c in 0..FORECAST_CLASS_COUNT {
        for h in 0..HIDDEN_DIM {
            head_w[c][h] = init_uniform(&mut rng, HIDDEN_DIM, FORECAST_CLASS_COUNT);
        }
    }
    GruWeights {
        w_x,
        w_h,
        wb: zeros2(GATE_COUNT, HIDDEN_DIM),
        rb: zeros2(GATE_COUNT, HIDDEN_DIM),
        head_w,
        head_b: vec![0.0; FORECAST_CLASS_COUNT],
    }
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// One GRU step gate index lookup. Centralises the `[z, r, h]` mapping
/// so the forward / backward / export paths can't drift.
const GATE_Z: usize = 0;
const GATE_R: usize = 1;
const GATE_H: usize = 2;

/// Run the GRU + linear head + softmax across one `WINDOW_LEN`
/// sequence. Returns the cached intermediates for backprop.
fn forward(weights: &GruWeights, seq: &[[f32; FEATURE_LEN]; WINDOW_LEN]) -> ForwardCache {
    let mut ht = vec![vec![0.0; HIDDEN_DIM]; WINDOW_LEN + 1];
    let mut zt = vec![vec![0.0; HIDDEN_DIM]; WINDOW_LEN];
    let mut rt = vec![vec![0.0; HIDDEN_DIM]; WINDOW_LEN];
    let mut nt = vec![vec![0.0; HIDDEN_DIM]; WINDOW_LEN];

    for t in 0..WINDOW_LEN {
        let x = &seq[t];
        let h_prev = ht[t].clone();
        // z / r gates: sigmoid of (W_g x + R_g h_{t-1} + Wb_g + Rb_g).
        for h in 0..HIDDEN_DIM {
            let mut zp = weights.wb[GATE_Z][h] + weights.rb[GATE_Z][h];
            let mut rp = weights.wb[GATE_R][h] + weights.rb[GATE_R][h];
            for f in 0..FEATURE_LEN {
                zp += weights.w_x[GATE_Z][h][f] * x[f];
                rp += weights.w_x[GATE_R][h][f] * x[f];
            }
            for hh in 0..HIDDEN_DIM {
                zp += weights.w_h[GATE_Z][h][hh] * h_prev[hh];
                rp += weights.w_h[GATE_R][h][hh] * h_prev[hh];
            }
            zt[t][h] = sigmoid(zp);
            rt[t][h] = sigmoid(rp);
        }
        // Candidate gate — default ONNX (`linear_before_reset=0`):
        //   n_t = tanh(W_h x + (r_t (.) h_{t-1}) @ R_h^T + Rb_h + Wb_h)
        // Per the ONNX/tract wiring, `(r_t (.) h_{t-1})` is the
        // element-wise product (per-index `r_t[i] * h_prev[i]`), then
        // matmul'd against `R_h^T`. So
        //   out[h] = Σ_i  R_h[h][i] * r_t[i] * h_prev[i]
        // — `r_t` is indexed by the summation variable `i`, NOT by the
        // output index `h`. The backward pass therefore receives a
        // distinct gradient component for every `r_t[i]`.
        for h in 0..HIDDEN_DIM {
            let mut np = weights.wb[GATE_H][h] + weights.rb[GATE_H][h];
            for f in 0..FEATURE_LEN {
                np += weights.w_x[GATE_H][h][f] * x[f];
            }
            for i in 0..HIDDEN_DIM {
                np += weights.w_h[GATE_H][h][i] * rt[t][i] * h_prev[i];
            }
            nt[t][h] = np.tanh();
        }
        // h_t = (1 - z_t) * n_t + z_t * h_{t-1}
        let mut h_next = vec![0.0; HIDDEN_DIM];
        for h in 0..HIDDEN_DIM {
            h_next[h] = (1.0 - zt[t][h]) * nt[t][h] + zt[t][h] * h_prev[h];
        }
        ht[t + 1] = h_next;
    }
    // Linear head + softmax on final hidden state.
    let final_h = ht[WINDOW_LEN].clone();
    let mut logits = vec![0.0; FORECAST_CLASS_COUNT];
    for c in 0..FORECAST_CLASS_COUNT {
        let mut s = weights.head_b[c];
        for h in 0..HIDDEN_DIM {
            s += weights.head_w[c][h] * final_h[h];
        }
        logits[c] = s;
    }
    let max_logit = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f32> = logits.iter().map(|l| (l - max_logit).exp()).collect();
    let sum: f32 = exps.iter().sum();
    let probs: Vec<f32> = exps.iter().map(|e| e / sum).collect();
    let _ = logits; // intermediate; only `probs` survives into the cache
    ForwardCache {
        zt,
        rt,
        nt,
        ht,
        probs,
    }
}

/// Accumulate gradients for one window into `grads`. Returns the
/// cross-entropy loss for the window.
fn backward(
    weights: &GruWeights,
    seq: &[[f32; FEATURE_LEN]; WINDOW_LEN],
    label: usize,
    cache: &ForwardCache,
    grads: &mut GruGrads,
) -> f32 {
    // Cross-entropy + softmax fused gradient: dL/dlogits[c] = probs[c]
    // - 1[c == label]. Loss itself is -log(probs[label]).
    let mut d_logits = cache.probs.clone();
    d_logits[label] -= 1.0;
    let loss = -((cache.probs[label].max(1e-12)).ln());

    // Head grads.
    let final_h = &cache.ht[WINDOW_LEN];
    let mut d_h_next = vec![0.0; HIDDEN_DIM];
    for c in 0..FORECAST_CLASS_COUNT {
        grads.head_b[c] += d_logits[c];
        for h in 0..HIDDEN_DIM {
            grads.head_w[c][h] += d_logits[c] * final_h[h];
            d_h_next[h] += d_logits[c] * weights.head_w[c][h];
        }
    }

    // BPTT through the GRU.
    for t in (0..WINDOW_LEN).rev() {
        let h_prev = &cache.ht[t];
        let x = &seq[t];
        let z = &cache.zt[t];
        let r = &cache.rt[t];
        let n = &cache.nt[t];

        // h_t = (1 - z) * n + z * h_prev
        // dL/dz = d_h * (h_prev - n)
        // dL/dn = d_h * (1 - z)
        // dL/dh_prev (from z gate) += d_h * z
        let mut d_z = vec![0.0; HIDDEN_DIM];
        let mut d_n = vec![0.0; HIDDEN_DIM];
        let mut d_h_prev = vec![0.0; HIDDEN_DIM];
        for h in 0..HIDDEN_DIM {
            d_z[h] = d_h_next[h] * (h_prev[h] - n[h]);
            d_n[h] = d_h_next[h] * (1.0 - z[h]);
            d_h_prev[h] += d_h_next[h] * z[h];
        }
        // n = tanh(n_pre); dn_pre = d_n * (1 - n^2)
        // Forward: np[h] = Wb_h[h] + Rb_h[h] + Σ_f W_x[H][h][f] * x[f]
        //                + Σ_i W_h[H][h][i] * r[i] * h_prev[i]
        // So:
        //   dWb_h[h]       += dn_pre[h]
        //   dRb_h[h]       += dn_pre[h]
        //   dW_x[H][h][f]  += dn_pre[h] * x[f]
        //   dW_h[H][h][i]  += dn_pre[h] * r[i] * h_prev[i]
        //   d_r[i]         += Σ_h dn_pre[h] * W_h[H][h][i] * h_prev[i]
        //   d_h_prev[i]    += Σ_h dn_pre[h] * W_h[H][h][i] * r[i]
        let mut d_r = vec![0.0; HIDDEN_DIM];
        for h in 0..HIDDEN_DIM {
            let dn_pre = d_n[h] * (1.0 - n[h] * n[h]);
            grads.wb[GATE_H][h] += dn_pre;
            grads.rb[GATE_H][h] += dn_pre;
            for f in 0..FEATURE_LEN {
                grads.w_x[GATE_H][h][f] += dn_pre * x[f];
            }
            for i in 0..HIDDEN_DIM {
                let w = weights.w_h[GATE_H][h][i];
                grads.w_h[GATE_H][h][i] += dn_pre * r[i] * h_prev[i];
                d_r[i] += dn_pre * w * h_prev[i];
                d_h_prev[i] += dn_pre * w * r[i];
            }
        }
        // r = sigmoid(r_pre); dr_pre = d_r * r * (1 - r)
        // r_pre = Wb_r + Rb_r + sum_f W_x[R][f]*x[f] + sum_hh W_h[R][hh]*h_prev[hh]
        for h in 0..HIDDEN_DIM {
            let dr_pre = d_r[h] * r[h] * (1.0 - r[h]);
            grads.wb[GATE_R][h] += dr_pre;
            grads.rb[GATE_R][h] += dr_pre;
            for f in 0..FEATURE_LEN {
                grads.w_x[GATE_R][h][f] += dr_pre * x[f];
            }
            for hh in 0..HIDDEN_DIM {
                grads.w_h[GATE_R][h][hh] += dr_pre * h_prev[hh];
                d_h_prev[hh] += dr_pre * weights.w_h[GATE_R][h][hh];
            }
        }
        // z = sigmoid(z_pre); dz_pre = d_z * z * (1 - z)
        // z_pre = Wb_z + Rb_z + sum_f W_x[Z][f]*x[f] + sum_hh W_h[Z][hh]*h_prev[hh]
        for h in 0..HIDDEN_DIM {
            let dz_pre = d_z[h] * z[h] * (1.0 - z[h]);
            grads.wb[GATE_Z][h] += dz_pre;
            grads.rb[GATE_Z][h] += dz_pre;
            for f in 0..FEATURE_LEN {
                grads.w_x[GATE_Z][h][f] += dz_pre * x[f];
            }
            for hh in 0..HIDDEN_DIM {
                grads.w_h[GATE_Z][h][hh] += dz_pre * h_prev[hh];
                d_h_prev[hh] += dz_pre * weights.w_h[GATE_Z][h][hh];
            }
        }
        d_h_next = d_h_prev;
    }
    loss
}

fn apply_grads(weights: &mut GruWeights, grads: &GruGrads, lr: f32, batch_size: f32) {
    let scale = lr / batch_size;
    for g in 0..GATE_COUNT {
        for h in 0..HIDDEN_DIM {
            for f in 0..FEATURE_LEN {
                weights.w_x[g][h][f] -= scale * grads.w_x[g][h][f];
            }
            for hh in 0..HIDDEN_DIM {
                weights.w_h[g][h][hh] -= scale * grads.w_h[g][h][hh];
            }
            weights.wb[g][h] -= scale * grads.wb[g][h];
            weights.rb[g][h] -= scale * grads.rb[g][h];
        }
    }
    for c in 0..FORECAST_CLASS_COUNT {
        weights.head_b[c] -= scale * grads.head_b[c];
        for h in 0..HIDDEN_DIM {
            weights.head_w[c][h] -= scale * grads.head_w[c][h];
        }
    }
}

/// Train the GRU for [`EPOCHS`] full-batch passes. Pure-Rust, no autodiff.
fn train_gru(windows: &[LabelledWindow]) -> Result<TrainedGru, String> {
    if windows.is_empty() {
        return Err("train_gru: empty window set".into());
    }
    let mut weights = init_weights();
    let mut last_loss = f32::NAN;
    for _ in 0..EPOCHS {
        let mut grads = GruGrads::zeros();
        let mut epoch_loss = 0.0;
        for w in windows {
            let cache = forward(&weights, &w.seq);
            let loss = backward(&weights, &w.seq, w.class_idx, &cache, &mut grads);
            if !loss.is_finite() {
                return Err(format!("non-finite loss: {loss}"));
            }
            epoch_loss += loss;
        }
        apply_grads(&mut weights, &grads, LEARNING_RATE, windows.len() as f32);
        last_loss = epoch_loss / windows.len() as f32;
    }
    Ok(TrainedGru { weights, last_loss })
}

/// Run the freshly-loaded tract model against the held-out
/// validation windows. Returns the fraction of windows whose
/// final-row class is predicted correctly when the daemon-shape
/// `gru::infer` is called against the LAST snapshot of the window —
/// matches how the daemon will use the model at runtime (one tick at
/// a time, GRU degenerates to a single-step roll-out with h_0 = 0).
fn run_validation(model: &Model, windows: &[LabelledWindow]) -> Result<f32, String> {
    if windows.is_empty() {
        return Ok(0.0);
    }
    let mut hits = 0usize;
    let mut class_hits = [0usize; FORECAST_CLASS_COUNT];
    let mut class_totals = [0usize; FORECAST_CLASS_COUNT];
    for w in windows {
        let last = w.seq[WINDOW_LEN - 1];
        let probs = crate::power::forecast::gru::infer(model, &last)
            .map_err(|e| format!("validation inference failed: {e}"))?;
        let argmax = probs
            .class_probs
            .iter()
            .enumerate()
            .max_by(|a, b| {
                a.1 .1
                    .partial_cmp(&b.1 .1)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i)
            .unwrap_or(0);
        if w.class_idx < FORECAST_CLASS_COUNT {
            class_totals[w.class_idx] += 1;
            if argmax == w.class_idx {
                class_hits[w.class_idx] += 1;
            }
        }
        if argmax == w.class_idx {
            hits += 1;
        }
    }
    // Per-class recall floor (Step T3 / BUG-20260525-2352). Only
    // checks classes that actually appear in the validation set —
    // unobserved classes are already blocked by the
    // `InsufficientClassCoverage` gate upstream, and re-flagging them
    // here would deadlock fixtures that intentionally probe the
    // recall path in isolation.
    for (idx, total) in class_totals.iter().enumerate() {
        if *total == 0 {
            continue;
        }
        let recall = class_hits[idx] as f32 / *total as f32;
        if recall < MIN_PER_CLASS_RECALL {
            return Err(format!(
                "per-class recall floor: class {} recall = {recall:.3} < {MIN_PER_CLASS_RECALL}",
                ACTIVITY_CLASSES[idx],
            ));
        }
    }
    Ok(hits as f32 / windows.len() as f32)
}

/// Multi-step validation that feeds the full `WINDOW_LEN` sequence
/// through tract — used by the temporal-pattern test which depends on
/// the GRU's hidden-state carry. Internal helper; not on the daemon's
/// hot path.
#[cfg(test)]
fn run_sequence_validation(model: &Model, windows: &[LabelledWindow]) -> Result<f32, String> {
    use tract_onnx::prelude::*;
    if windows.is_empty() {
        return Ok(0.0);
    }
    let mut hits = 0usize;
    for w in windows {
        let mut flat = Vec::with_capacity(WINDOW_LEN * FEATURE_LEN);
        for step in &w.seq {
            flat.extend_from_slice(step);
        }
        let input = tract_ndarray::Array3::from_shape_vec((WINDOW_LEN, 1, FEATURE_LEN), flat)
            .map_err(|e| format!("build seq input: {e}"))?
            .into_tensor();
        let outputs = model
            .runnable()
            .run(tvec!(input.into()))
            .map_err(|e| format!("tract run: {e}"))?;
        let raw = outputs
            .first()
            .ok_or_else(|| "tract returned no outputs".to_string())?;
        let view = raw
            .to_array_view::<f32>()
            .map_err(|e| format!("tract output not f32: {e}"))?;
        let probs: Vec<f32> = view.iter().copied().collect();
        if probs.len() < FORECAST_CLASS_COUNT {
            return Err(format!(
                "tract output has {} elems, expected ≥ {FORECAST_CLASS_COUNT}",
                probs.len(),
            ));
        }
        let argmax = probs[..FORECAST_CLASS_COUNT]
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
            .unwrap_or(0);
        if argmax == w.class_idx {
            hits += 1;
        }
    }
    Ok(hits as f32 / windows.len() as f32)
}

// ---------------------------------------------------------------------------
// ONNX export — hand-emitted protobuf with native `GRU` op + head.
// ---------------------------------------------------------------------------

/// Build the trained-model ONNX bytes. The graph mirrors the GRU
/// forward pass with the BUG-20260723-2352 normalisation prefix:
/// `features[seq, 1, 12] → Sub(mean) → Mul(1/std) → GRU → Y_h[1, 1, 16]
/// → Reshape → Gemm(W_head, b_head) → Softmax → probs[1, 5]`. Every op
/// is in tract's well-supported core set so the validation gate is
/// reliable, and the model consumes RAW daemon features — the z-score
/// constants travel inside the graph.
fn export_onnx(weights: &GruWeights, stats: &FeatureStats) -> Result<Vec<u8>, String> {
    // Flatten W: [num_directions=1, 3*H, I] — gate-major, rows are
    // hidden units within each gate.
    let mut w_flat = Vec::with_capacity(GATE_COUNT * HIDDEN_DIM * FEATURE_LEN);
    for g in [GATE_Z, GATE_R, GATE_H] {
        for h in 0..HIDDEN_DIM {
            for f in 0..FEATURE_LEN {
                w_flat.push(weights.w_x[g][h][f]);
            }
        }
    }
    let mut r_flat = Vec::with_capacity(GATE_COUNT * HIDDEN_DIM * HIDDEN_DIM);
    for g in [GATE_Z, GATE_R, GATE_H] {
        for h in 0..HIDDEN_DIM {
            for hh in 0..HIDDEN_DIM {
                r_flat.push(weights.w_h[g][h][hh]);
            }
        }
    }
    // Bias: [1, 6*H]. Order: Wb_z, Wb_r, Wb_h, Rb_z, Rb_r, Rb_h.
    let mut b_flat = Vec::with_capacity(BIAS_SLAB_COUNT * HIDDEN_DIM);
    for g in [GATE_Z, GATE_R, GATE_H] {
        b_flat.extend_from_slice(&weights.wb[g]);
    }
    for g in [GATE_Z, GATE_R, GATE_H] {
        b_flat.extend_from_slice(&weights.rb[g]);
    }
    let gru_w = float_tensor(
        "GRU_W",
        &[1, (GATE_COUNT * HIDDEN_DIM) as i64, FEATURE_LEN as i64],
        w_flat,
    )?;
    let gru_r = float_tensor(
        "GRU_R",
        &[1, (GATE_COUNT * HIDDEN_DIM) as i64, HIDDEN_DIM as i64],
        r_flat,
    )?;
    let gru_b = float_tensor("GRU_B", &[1, (BIAS_SLAB_COUNT * HIDDEN_DIM) as i64], b_flat)?;

    // Head: Gemm computes `alpha * X * W^T + beta * B`. ONNX `Gemm` is
    // [M, K] @ [N, K]^T = [M, N] when `transB=1` — we store head_w as
    // [N, K] = [5, 16] so this matches without a transpose.
    let mut head_w_flat = Vec::with_capacity(FORECAST_CLASS_COUNT * HIDDEN_DIM);
    for c in 0..FORECAST_CLASS_COUNT {
        for h in 0..HIDDEN_DIM {
            head_w_flat.push(weights.head_w[c][h]);
        }
    }
    let head_w_tensor = float_tensor(
        "Head_W",
        &[FORECAST_CLASS_COUNT as i64, HIDDEN_DIM as i64],
        head_w_flat,
    )?;
    let head_b_tensor = float_tensor(
        "Head_b",
        &[FORECAST_CLASS_COUNT as i64],
        weights.head_b.clone(),
    )?;
    // Target shape initializer for the Reshape node — `[1, HIDDEN_DIM]`
    // collapses GRU's `[num_directions, batch, hidden]` Y_h tensor
    // down to the Gemm-friendly 2-D layout. Opset-13's `Reshape` takes
    // `shape` as an INT64 tensor input (Squeeze moved its `axes` to an
    // input in the same opset bump, so Reshape is the simpler choice
    // here — one node, no axes-tensor side-input).
    let reshape_shape = int64_tensor("Reshape_target", &[2], vec![1i64, HIDDEN_DIM as i64])?;
    // Normalisation constants — shape [12] broadcasts across the
    // `[seq, 1, 12]` input's trailing axis per ONNX numpy rules.
    let norm_mean = float_tensor("Norm_mean", &[FEATURE_LEN as i64], stats.mean.to_vec())?;
    let norm_scale = float_tensor("Norm_scale", &[FEATURE_LEN as i64], stats.inv_std.to_vec())?;

    let nodes = vec![
        // z-score prefix: (features - mean) * inv_std.
        binary_node("Sub", "norm_sub", "features", "Norm_mean", "centered"),
        binary_node("Mul", "norm_mul", "centered", "Norm_scale", "normed"),
        // GRU outputs Y_h with shape [num_directions=1, batch=1, hidden].
        gru_node("gru1", "normed", "GRU_W", "GRU_R", "GRU_B", "Y_h"),
        // Reshape [1, 1, hidden] → [1, hidden] so Gemm sees rank-2.
        reshape_node("rs1", "Y_h", "Reshape_target", "h_final"),
        // Linear head + softmax.
        gemm_node("gemm1", "h_final", "Head_W", "Head_b", "logits"),
        softmax_node("sm1", "logits", "probs"),
    ];
    let graph = pb::GraphProto {
        node: nodes,
        name: "sy_power_trainer_gru".into(),
        initializer: vec![
            norm_mean,
            norm_scale,
            gru_w,
            gru_r,
            gru_b,
            head_w_tensor,
            head_b_tensor,
            reshape_shape,
        ],
        input: vec![value_info_sym(
            "features",
            &[
                Dim::Param("S"),
                Dim::Value(1),
                Dim::Value(FEATURE_LEN as i64),
            ],
        )],
        output: vec![value_info("probs", &[1, FORECAST_CLASS_COUNT as i64])],
        ..Default::default()
    };
    let model = pb::ModelProto {
        ir_version: IR_VERSION,
        opset_import: vec![pb::OperatorSetIdProto {
            domain: String::new(),
            version: OPSET_VERSION,
        }],
        producer_name: "sy-power-trainer".into(),
        producer_version: env!("CARGO_PKG_VERSION").into(),
        graph: Some(graph),
        ..Default::default()
    };
    Ok(model.encode_to_vec())
}

fn float_tensor(name: &str, dims: &[i64], data: Vec<f32>) -> Result<pb::TensorProto, String> {
    let expected: i64 = dims.iter().product();
    if data.len() as i64 != expected {
        return Err(format!(
            "tensor {name}: expected {expected} elems for shape {dims:?}, got {}",
            data.len()
        ));
    }
    Ok(pb::TensorProto {
        dims: dims.to_vec(),
        data_type: TENSOR_FLOAT,
        float_data: data,
        name: name.into(),
        ..Default::default()
    })
}

/// Build a `GRU` op node. The third output slot (`Y_h`) is the only
/// one we keep — the per-step `Y` output is `""` (unused, ONNX spec
/// allows empty output names to indicate "not produced"). We use
/// `Y` empty + `Y_h` named so tract's Common-Rec wiring only emits
/// the last hidden state.
fn gru_node(name: &str, x: &str, w: &str, r: &str, b: &str, y_h: &str) -> pb::NodeProto {
    let hidden_attr = pb::AttributeProto {
        name: "hidden_size".into(),
        r#type: ATTR_INT,
        i: HIDDEN_DIM as i64,
        ..Default::default()
    };
    let lbr_attr = pb::AttributeProto {
        name: "linear_before_reset".into(),
        r#type: ATTR_INT,
        i: 0,
        ..Default::default()
    };
    pb::NodeProto {
        name: name.into(),
        input: vec![x.into(), w.into(), r.into(), b.into()],
        output: vec![String::new(), y_h.into()],
        op_type: "GRU".into(),
        attribute: vec![hidden_attr, lbr_attr],
        ..Default::default()
    }
}

/// Element-wise binary op node (`Sub` / `Mul`) — the normalisation
/// prefix's building block. Relies on ONNX numpy-style broadcasting.
fn binary_node(op: &str, name: &str, a: &str, b: &str, y: &str) -> pb::NodeProto {
    pb::NodeProto {
        name: name.into(),
        input: vec![a.into(), b.into()],
        output: vec![y.into()],
        op_type: op.into(),
        ..Default::default()
    }
}

/// `Reshape` to a caller-supplied `shape` initializer. Used to flatten
/// the GRU's `[num_directions, batch, hidden]` Y_h down to `[1,
/// hidden]` so the downstream Gemm sees rank-2.
fn reshape_node(name: &str, x: &str, shape: &str, y: &str) -> pb::NodeProto {
    pb::NodeProto {
        name: name.into(),
        input: vec![x.into(), shape.into()],
        output: vec![y.into()],
        op_type: "Reshape".into(),
        ..Default::default()
    }
}

/// Build an `INT64` tensor initializer. Used for `Reshape`'s `shape`
/// input — ONNX requires the shape tensor's element type be INT64,
/// and prost's `TensorProto.int64_data` holds the values directly
/// (no raw bytes route).
fn int64_tensor(name: &str, dims: &[i64], data: Vec<i64>) -> Result<pb::TensorProto, String> {
    let expected: i64 = dims.iter().product();
    if data.len() as i64 != expected {
        return Err(format!(
            "tensor {name}: expected {expected} elems for shape {dims:?}, got {}",
            data.len()
        ));
    }
    Ok(pb::TensorProto {
        dims: dims.to_vec(),
        data_type: TENSOR_INT64,
        int64_data: data,
        name: name.into(),
        ..Default::default()
    })
}

fn gemm_node(name: &str, x: &str, w: &str, b: &str, y: &str) -> pb::NodeProto {
    let trans_b_attr = pb::AttributeProto {
        name: "transB".into(),
        r#type: ATTR_INT,
        i: 1,
        ..Default::default()
    };
    let alpha_attr = pb::AttributeProto {
        name: "alpha".into(),
        r#type: ATTR_FLOAT,
        f: 1.0,
        ..Default::default()
    };
    let beta_attr = pb::AttributeProto {
        name: "beta".into(),
        r#type: ATTR_FLOAT,
        f: 1.0,
        ..Default::default()
    };
    pb::NodeProto {
        name: name.into(),
        input: vec![x.into(), w.into(), b.into()],
        output: vec![y.into()],
        op_type: "Gemm".into(),
        attribute: vec![alpha_attr, beta_attr, trans_b_attr],
        ..Default::default()
    }
}

fn softmax_node(name: &str, x: &str, y: &str) -> pb::NodeProto {
    // axis=-1 — softmax over the class dimension. ONNX opset-13's
    // Softmax defaults to `axis = -1`, but spelling it explicitly
    // makes the export self-documenting and immune to opset bumps.
    // Attribute kind is INT (singular), not INTS — tract verifies
    // the attribute type tag and refuses INTS for `axis`.
    let axis_attr = pb::AttributeProto {
        name: "axis".into(),
        r#type: ATTR_INT,
        i: -1,
        ..Default::default()
    };
    pb::NodeProto {
        name: name.into(),
        input: vec![x.into()],
        output: vec![y.into()],
        op_type: "Softmax".into(),
        attribute: vec![axis_attr],
        ..Default::default()
    }
}

/// One axis-shape declaration. Concrete dims use [`Dim::Value`];
/// symbolic dims (e.g. the GRU's variable `seq_length`) use
/// [`Dim::Param`] with a named symbol — tract requires the symbol to
/// be `DimParam` (not `DimValue(-1)`) or it complains the source has
/// no determined fact at `into_optimized` time.
enum Dim {
    Value(i64),
    Param(&'static str),
}

fn value_info_sym(name: &str, shape: &[Dim]) -> pb::ValueInfoProto {
    let dims = shape
        .iter()
        .map(|d| {
            let value = match d {
                Dim::Value(v) => pb::tensor_shape_proto::dimension::Value::DimValue(*v),
                Dim::Param(s) => pb::tensor_shape_proto::dimension::Value::DimParam((*s).into()),
            };
            pb::tensor_shape_proto::Dimension {
                value: Some(value),
                denotation: String::new(),
            }
        })
        .collect();
    pb::ValueInfoProto {
        name: name.into(),
        r#type: Some(pb::TypeProto {
            denotation: String::new(),
            value: Some(pb::type_proto::Value::TensorType(pb::type_proto::Tensor {
                elem_type: TENSOR_FLOAT,
                shape: Some(pb::TensorShapeProto { dim: dims }),
            })),
        }),
        doc_string: String::new(),
    }
}

fn value_info(name: &str, shape: &[i64]) -> pb::ValueInfoProto {
    let dims: Vec<Dim> = shape.iter().map(|d| Dim::Value(*d)).collect();
    value_info_sym(name, &dims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::log::SCHEMA_ID;
    use crate::power::snapshot::{Snapshot, SnapshotRaw, SCHEMA_ID as SNAP_SCHEMA_ID};
    use chrono::{TimeZone, Utc};
    use tempfile::TempDir;

    /// 300-row synthetic stream the converges/round-trip tests share.
    const SYNTH_ROWS: usize = 300;

    /// Build one synthetic NDJSON row with `features` and `arm`. The
    /// schema fields are stamped manually so the test isn't coupled to
    /// any specific daemon constructor.
    fn synth_line(features: [f32; FEATURE_LEN], arm: &str, seq: i64) -> String {
        synth_line_with_shield(features, arm, seq, None)
    }

    /// [`synth_line`] with an explicit `shield_state` — the
    /// BUG-20260723-2352 label-poisoning tests need HOT / BATTERY_LOW
    /// rows.
    fn synth_line_with_shield(
        features: [f32; FEATURE_LEN],
        arm: &str,
        seq: i64,
        shield: Option<&str>,
    ) -> String {
        let ts =
            Utc.with_ymd_and_hms(2026, 5, 19, 0, 0, 0).unwrap() + chrono::Duration::seconds(seq);
        let snapshot = Snapshot {
            schema: SNAP_SCHEMA_ID,
            ts,
            features,
            raw: SnapshotRaw::default(),
            snapshot_hash: "0".repeat(64),
        };
        let entry = AuditEntry {
            schema: SCHEMA_ID,
            snapshot,
            applied_arm: Some(arm.into()),
            shield_state: shield.map(String::from),
            reason_chain: Vec::new(),
            ranked_actions: Vec::new(),
            conservative_alpha: 0.0,
        };
        serde_json::to_string(&entry).expect("serialise synthetic entry")
    }

    /// Three feature "centroids", one per class the synthetic test
    /// exercises (`idle`, `build`, `call`). Picked deliberately
    /// linearly separable in 12-d space so the GRU can fit them in
    /// `EPOCHS` iterations.
    fn class_centroid(class: &str) -> [f32; FEATURE_LEN] {
        let mut v = [0.0f32; FEATURE_LEN];
        match class {
            "idle" => {
                v[4] = 0.9; // battery_soc_pct high
                v[5] = 0.0; // ac_online off
                v[9] = 0.8; // user_idle_s high
            }
            "build" => {
                v[0] = 0.8; // tctl_c high
                v[1] = 0.7; // package_power_w high
                v[7] = 0.6; // psi_cpu spike
            }
            "call" => {
                v[8] = 1.0; // call_active
                v[2] = 0.3; // igpu_busy
            }
            _ => {}
        }
        v
    }

    /// Apply a small deterministic perturbation so every row inside a
    /// class differs in feature space — keeps the gradient signal
    /// non-degenerate without pulling in a PRNG dep.
    fn perturb(mut v: [f32; FEATURE_LEN], step: i64) -> [f32; FEATURE_LEN] {
        let jitter = (step as f32 * 0.013).sin() * 0.05;
        for slot in v.iter_mut() {
            *slot += jitter;
        }
        v
    }

    /// Write the SYNTH_ROWS-line NDJSON corpus to `dir/telemetry.ndjson`
    /// with all five activity classes round-robin-interleaved (Step T3
    /// coverage gate). Round-robin guarantees both the training split
    /// (first 80%) and the held-out validation split (last 20%) see
    /// every class, so the new per-class recall floor doesn't reject
    /// the trainer when an entire class only appeared at the tail end
    /// of a temporally-ordered corpus.
    fn write_synth_corpus(dir: &Path) -> PathBuf {
        let path = dir.join("telemetry.ndjson");
        let mut f = fs::File::create(&path).expect("create synthetic NDJSON");
        let arms = ["idle", "browse", "call", "code", "build"];
        for seq in 0..SYNTH_ROWS {
            let arm = arms[seq % arms.len()];
            let mut feats = class_centroid(arm);
            // `browse` / `code` aren't in `class_centroid`'s table —
            // synthesize distinct centroids so the trainer can
            // separate every class.
            match arm {
                "browse" => {
                    feats[3] = 0.85;
                    feats[6] = 0.5;
                }
                "code" => {
                    feats[10] = 0.9;
                    feats[11] = 0.4;
                }
                _ => {}
            }
            let feats = perturb(feats, seq as i64);
            writeln!(f, "{}", synth_line(feats, arm, seq as i64)).expect("write line");
        }
        path
    }

    /// Step P2-1 DoD: post-training cross-entropy loss drops well
    /// below the pre-training random-init level. With 5 classes the
    /// random baseline is `−ln(1/5) ≈ 1.61`; we want the trained
    /// model significantly below that.
    #[test]
    fn train_on_synthetic_ndjson_converges() {
        let tmp = TempDir::new().expect("tempdir");
        let in_path = write_synth_corpus(tmp.path());
        let out_path = tmp.path().join("model.onnx");
        let report = retrain_gru(&in_path, &out_path).expect("retrain_gru should converge");
        // Random init over 5 classes carries cross-entropy ≈ 1.61;
        // converged trainer should be well below 0.8. The 0.8 floor
        // mirrors the SPEC §6 risk-table CI gate; tighter than that
        // and the GRU's smaller per-batch update needs careful LR
        // tuning to keep the test deflake-proof on slow CI runners.
        const CONVERGED_FLOOR: f32 = 0.8;
        assert!(
            report.final_loss < CONVERGED_FLOOR,
            "expected final_loss < {CONVERGED_FLOOR}, got {}",
            report.final_loss,
        );
        assert!(
            out_path.exists(),
            "ONNX should land at {}",
            out_path.display()
        );
        assert_eq!(report.epochs, EPOCHS);
        assert_eq!(report.rows_used, SYNTH_ROWS);
    }

    /// Step P2-1 DoD: trained model loads in tract and runs one
    /// inference without panicking.
    #[test]
    fn onnx_round_trips_through_tract_with_gru_op() {
        let tmp = TempDir::new().expect("tempdir");
        let in_path = write_synth_corpus(tmp.path());
        let out_path = tmp.path().join("model.onnx");
        let _report = retrain_gru(&in_path, &out_path).expect("retrain_gru");
        // Re-open the saved ONNX through the production loader to
        // prove the file we wrote is the file tract validated.
        let bytes = fs::read(&out_path).expect("read saved ONNX");
        let model = Model::from_onnx_bytes(&bytes).expect("tract decodes saved ONNX");
        // Sanity: one inference through the daemon-shape entry point.
        let features = [0.0f32; FEATURE_LEN];
        let forecast = crate::power::forecast::gru::infer(&model, &features)
            .expect("tract runs one inference");
        assert_eq!(forecast.class_probs.len(), FORECAST_CLASS_COUNT);
        // ONNX bytes must contain the literal `GRU` op type — the
        // SPEC §3 architecture compliance gate. We byte-scan rather
        // than re-parsing the protobuf because the GRU op name appears
        // verbatim in the encoded `NodeProto.op_type` string.
        assert!(
            bytes.windows(3).any(|w| w == b"GRU"),
            "ONNX bytes must contain a GRU op",
        );
    }

    /// Step P2-1 DoD: the GRU's temporal structure is actually learned.
    /// Builds a corpus where the class label is *only* recoverable
    /// from the early-window feature pattern — the last snapshot in
    /// the window carries no signal. A pure feedforward classifier
    /// hits ~chance; a GRU's hidden state can carry the cue forward.
    #[test]
    fn trains_gru_on_synthetic_temporal_pattern() {
        const TEMPORAL_WINDOWS: usize = 200;
        const TAIL_HOLDOUT: usize = 50;
        // Build a corpus directly in window form (skip the NDJSON
        // round-trip — this test is about the GRU dynamics, not the
        // ingest pipeline). Each window carries a class-specific cue
        // in step 0 and uniform-noise steps 1..7.
        let mut rng = Lcg::new(0xCAFE_BABE_DEAD_BEEF);
        let mut windows: Vec<LabelledWindow> = Vec::with_capacity(TEMPORAL_WINDOWS);
        for i in 0..TEMPORAL_WINDOWS {
            let class_idx = i % 3; // rotate through idle / browse / call
            let mut seq = [[0.0f32; FEATURE_LEN]; WINDOW_LEN];
            // Cue at step 0: a class-specific spike on a fixed feature
            // index. Subsequent steps are zero-mean noise across all
            // 12 features — a feedforward on step 7 alone sees no
            // signal.
            let cue_idx = match class_idx {
                0 => 4, // idle ~ battery
                1 => 1, // build ~ power
                _ => 8, // call ~ call_active
            };
            seq[0][cue_idx] = 1.0;
            for t in 1..WINDOW_LEN {
                for f in 0..FEATURE_LEN {
                    seq[t][f] = rng.next_uniform(0.2);
                }
            }
            windows.push(LabelledWindow { seq, class_idx });
        }
        let split = TEMPORAL_WINDOWS - TAIL_HOLDOUT;
        let train = windows[..split].to_vec();
        let validate = windows[split..].to_vec();
        let trained = train_gru(&train).expect("train_gru converges");
        // Direct accuracy through the hand-rolled forward pass — this
        // proves the GRU + head actually learned the temporal cue,
        // independent of any ONNX-export discrepancy.
        let mut native_hits = 0usize;
        for w in &validate {
            let cache = forward(&trained.weights, &w.seq);
            let argmax = cache
                .probs
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(i, _)| i)
                .unwrap_or(0);
            if argmax == w.class_idx {
                native_hits += 1;
            }
        }
        let native_accuracy = native_hits as f32 / validate.len() as f32;
        // Export + reload through tract — the validation accuracy
        // assertion below depends on the same GRU op tract decodes
        // from the emitted ONNX.
        let bytes = export_onnx(&trained.weights, &FeatureStats::identity()).expect("export_onnx");
        let model = Model::from_onnx_bytes(&bytes).expect("tract decodes trained GRU ONNX");
        // Run the FULL sequence through tract (not just the last
        // snapshot) — the GRU's temporal signal is what we're
        // probing, so the held-out windows are fed end-to-end.
        let accuracy =
            run_sequence_validation(&model, &validate).expect("sequence validation through tract");
        const TEMPORAL_ACC_FLOOR: f32 = 0.7;
        assert!(
            accuracy > TEMPORAL_ACC_FLOOR,
            "expected accuracy > {TEMPORAL_ACC_FLOOR}, got {accuracy} \
             (native forward accuracy = {native_accuracy}, loss = {})",
            trained.last_loss,
        );
    }

    /// Sink that drops the trained bytes on the floor before
    /// validation — emulates the SPEC §6 risk-table "freshly-trained
    /// ONNX fails the tract gate" scenario.
    struct TruncatingSink;

    impl TrainerExportSink for TruncatingSink {
        fn write(&mut self, _bytes: &[u8]) -> std::io::Result<()> {
            Ok(())
        }
        fn validation_bytes(&self) -> Vec<u8> {
            Vec::new()
        }
    }

    /// Step 25 DoD: the trainer aborts with
    /// [`TrainerError::ValidationFailed`] when the tract gate
    /// refuses the exported bytes. With [`TruncatingSink`] the
    /// "validation bytes" are 0-length so tract's protobuf parser
    /// rejects them immediately; the existing model on disk (if any)
    /// is preserved.
    #[test]
    fn abort_when_validation_fails() {
        let tmp = TempDir::new().expect("tempdir");
        let in_path = write_synth_corpus(tmp.path());
        let mut sink = TruncatingSink;
        let err = retrain_with_sink(&in_path, &mut sink)
            .expect_err("truncated sink must fail validation");
        assert!(
            matches!(err, TrainerError::ValidationFailed(_)),
            "expected ValidationFailed, got {err:?}",
        );
    }

    /// Step 25 DoD: the trainer refuses an underpopulated NDJSON
    /// corpus instead of crashing on an empty tensor. The error is
    /// the documented `InsufficientData` variant the daemon's
    /// retrain scheduler (Step 26) checks for to keep the
    /// rules-baseline warmup active.
    #[test]
    fn rejects_corpus_below_min_rows() {
        let tmp = TempDir::new().expect("tempdir");
        let in_path = tmp.path().join("telemetry.ndjson");
        let mut f = fs::File::create(&in_path).expect("create file");
        for i in 0..(MIN_TRAINING_ROWS - 1) {
            let feats = perturb(class_centroid("idle"), i as i64);
            writeln!(f, "{}", synth_line(feats, "idle", i as i64)).expect("write line");
        }
        let out_path = tmp.path().join("model.onnx");
        let err = retrain_gru(&in_path, &out_path).expect_err("too few rows");
        match err {
            TrainerError::InsufficientData { required, found } => {
                assert_eq!(required, MIN_TRAINING_ROWS);
                assert_eq!(found, MIN_TRAINING_ROWS - 1);
            }
            other => panic!("expected InsufficientData, got {other:?}"),
        }
    }

    /// Write a full round-robin corpus (all five classes) into
    /// `dir/<name>`, splitting the `SYNTH_ROWS` rows so `seq` values
    /// stay globally monotonic across segments (`start`..`start+count`).
    /// Mirrors `write_synth_corpus` but lets a test lay the same corpus
    /// down across several daily-segmented files.
    fn write_named_segment(dir: &Path, name: &str, start: usize, count: usize) -> PathBuf {
        let path = dir.join(name);
        let mut f = fs::File::create(&path).expect("create segment");
        let arms = ["idle", "browse", "call", "code", "build"];
        for seq in start..start + count {
            let arm = arms[seq % arms.len()];
            let mut feats = class_centroid(arm);
            match arm {
                "browse" => {
                    feats[3] = 0.85;
                    feats[6] = 0.5;
                }
                "code" => {
                    feats[10] = 0.9;
                    feats[11] = 0.4;
                }
                _ => {}
            }
            let feats = perturb(feats, seq as i64);
            writeln!(f, "{}", synth_line(feats, arm, seq as i64)).expect("write line");
        }
        path
    }

    /// BUG-20260712-1545 part 1: live telemetry is daily-segmented, so
    /// the trainer must accept the state *directory* and read every
    /// `telemetry-*.ndjson` segment. The corpus split across two daily
    /// files must train exactly as if it were one file, and unrelated
    /// files (checkpoint.json, forecaster.onnx) must be skipped.
    #[test]
    fn reads_directory_corpus_across_segments() {
        let tmp = TempDir::new().expect("tempdir");
        let half = SYNTH_ROWS / 2;
        write_named_segment(tmp.path(), "telemetry-2026-07-11.ndjson", 0, half);
        write_named_segment(
            tmp.path(),
            "telemetry-2026-07-12.ndjson",
            half,
            SYNTH_ROWS - half,
        );
        // Decoys that must be skipped, not parsed as telemetry.
        fs::write(tmp.path().join("checkpoint.json"), b"{}").expect("decoy checkpoint");
        fs::write(tmp.path().join("forecaster.onnx"), b"not onnx").expect("decoy model");
        fs::create_dir(tmp.path().join("reports")).expect("decoy reports dir");

        let out_path = tmp.path().join("model.onnx");
        let report = retrain_gru(tmp.path(), &out_path).expect("directory corpus trains");
        assert_eq!(report.rows_used, SYNTH_ROWS);
        assert!(out_path.exists(), "model should land at {}", out_path.display());
    }

    /// BUG-20260712-1545 part 1: overflow segments of one day are named
    /// `telemetry-<date>.<N>.ndjson`. Plain lexicographic order sorts
    /// `.10` before `.2`; the trainer must order them numerically
    /// (base file first, then 1, 2, … 10) so the corpus is chronological.
    #[test]
    fn reads_directory_corpus_orders_numeric_overflow() {
        // Deliberately create the segments out of order and with a
        // numeric-overflow suffix that would mis-sort lexicographically.
        let names = [
            "telemetry-2026-07-12.10.ndjson",
            "telemetry-2026-07-12.2.ndjson",
            "telemetry-2026-07-12.ndjson",
            "telemetry-2026-07-12.1.ndjson",
        ];
        let expected = [
            "telemetry-2026-07-12.ndjson",
            "telemetry-2026-07-12.1.ndjson",
            "telemetry-2026-07-12.2.ndjson",
            "telemetry-2026-07-12.10.ndjson",
        ];
        let tmp = TempDir::new().expect("tempdir");
        for name in names {
            fs::write(tmp.path().join(name), b"").expect("touch segment");
        }
        let ordered = telemetry_segments(tmp.path()).expect("collect segments");
        let ordered_names: Vec<String> = ordered
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(ordered_names, expected);
    }

    /// BUG-20260712-1545 part 1: single-file paths must keep working
    /// (the `sy power train --telemetry <file>` CLI + every existing
    /// trainer test pass a file, not a directory).
    #[test]
    fn single_file_corpus_still_trains() {
        let tmp = TempDir::new().expect("tempdir");
        let in_path = write_synth_corpus(tmp.path());
        assert!(in_path.is_file(), "fixture must be a single file");
        let out_path = tmp.path().join("model.onnx");
        let report = retrain_gru(&in_path, &out_path).expect("single-file corpus trains");
        assert_eq!(report.rows_used, SYNTH_ROWS);
    }

    /// BUG-20260712-1545 part 1: a directory with zero matching
    /// segments must flow into the documented `InsufficientData` path,
    /// never panic on an empty tensor.
    #[test]
    fn empty_directory_is_insufficient_data_not_panic() {
        let tmp = TempDir::new().expect("tempdir");
        // Only non-telemetry files present.
        fs::write(tmp.path().join("checkpoint.json"), b"{}").expect("decoy");
        let out_path = tmp.path().join("model.onnx");
        let err = retrain_gru(tmp.path(), &out_path).expect_err("empty dir has no rows");
        match err {
            TrainerError::InsufficientData { found, .. } => assert_eq!(found, 0),
            other => panic!("expected InsufficientData, got {other:?}"),
        }
    }

    /// Step T3 DoD: when the corpus is missing entire classes, the
    /// BUG-20260723-2210: a corpus with solid coverage on some classes
    /// but zero rows on others must TRAIN on the covered classes and
    /// report the rest as excluded — the all-or-nothing gate deadlocked
    /// the live daemon (rules baseline only ever applies the arms its
    /// heuristics reach, so `call`/`build` never accrue rows, so the
    /// model never trains, so the drift alarm never clears). Mirrors
    /// the live corpus shape: `idle` / `browse` / `code` covered,
    /// `call` / `build` at zero.
    #[test]
    fn trains_when_uncovered_classes_can_be_excluded() {
        let tmp = TempDir::new().expect("tempdir");
        let in_path = tmp.path().join("telemetry.ndjson");
        let mut f = fs::File::create(&in_path).expect("create file");
        let arms = ["browse", "idle", "code"];
        for i in 0..200usize {
            let arm = arms[i % arms.len()];
            let mut feats = class_centroid(arm);
            match arm {
                "browse" => {
                    feats[3] = 0.85;
                    feats[6] = 0.5;
                }
                "code" => {
                    feats[10] = 0.9;
                    feats[11] = 0.4;
                }
                _ => {}
            }
            let feats = perturb(feats, i as i64);
            writeln!(f, "{}", synth_line(feats, arm, i as i64)).expect("write line");
        }
        let out_path = tmp.path().join("model.onnx");
        let report = retrain_gru(&in_path, &out_path)
            .expect("covered classes must train despite zero-row classes");
        assert_eq!(
            report.excluded_classes,
            vec!["call", "build"],
            "zero-row classes must be reported as excluded",
        );
        assert_eq!(report.rows_used, 200, "all covered-class rows train");
        assert!(
            out_path.exists(),
            "ONNX must land at {}",
            out_path.display(),
        );
    }

    /// BUG-20260723-2210: exclusion has a floor — a softmax classifier
    /// over fewer than [`MIN_COVERED_CLASSES`] covered classes is
    /// degenerate, so a corpus where only one class clears the row
    /// floor still rejects with `InsufficientClassCoverage`.
    #[test]
    fn rejects_when_fewer_than_two_classes_covered() {
        let tmp = TempDir::new().expect("tempdir");
        let in_path = tmp.path().join("telemetry.ndjson");
        let mut f = fs::File::create(&in_path).expect("create file");
        for i in 0..100usize {
            let feats = perturb(class_centroid("idle"), i as i64);
            writeln!(f, "{}", synth_line(feats, "browse", i as i64)).expect("write line");
        }
        let out_path = tmp.path().join("model.onnx");
        let err = retrain_gru(&in_path, &out_path).expect_err("single covered class must reject");
        match err {
            TrainerError::InsufficientClassCoverage {
                missing, required, ..
            } => {
                assert_eq!(required, MIN_ROWS_PER_CLASS);
                assert!(
                    missing.contains(&"call") && missing.contains(&"build"),
                    "uncovered classes must be named, got {missing:?}",
                );
            }
            other => panic!("expected InsufficientClassCoverage, got {other:?}"),
        }
    }

    /// Step T3 DoD: the coverage gate enforces a per-class ROW count,
    /// not just class presence. A corpus with 100 browse rows + 5 code
    /// rows + zero call/build/idle must reject — `code` is below the
    /// row floor, leaving only `browse` covered, which is under the
    /// [`MIN_COVERED_CLASSES`] floor (BUG-20260723-2210).
    #[test]
    fn rejects_when_class_has_too_few_rows() {
        let tmp = TempDir::new().expect("tempdir");
        let in_path = tmp.path().join("telemetry.ndjson");
        let mut f = fs::File::create(&in_path).expect("create file");
        for i in 0..100usize {
            let feats = perturb(class_centroid("idle"), i as i64);
            writeln!(f, "{}", synth_line(feats, "browse", i as i64)).expect("write browse");
        }
        for i in 0..5usize {
            let feats = perturb(class_centroid("build"), (100 + i) as i64);
            writeln!(f, "{}", synth_line(feats, "code", (100 + i) as i64)).expect("write code");
        }
        let out_path = tmp.path().join("model.onnx");
        let err = retrain_gru(&in_path, &out_path).expect_err("under-floor class must reject");
        match err {
            TrainerError::InsufficientClassCoverage {
                missing, counts, ..
            } => {
                assert!(
                    missing.contains(&"code"),
                    "code (5 < 16) must be in missing list, got {missing:?}",
                );
                let code_idx = ACTIVITY_CLASSES
                    .iter()
                    .position(|c| *c == "code")
                    .expect("code class");
                assert_eq!(counts[code_idx], 5, "counts must reflect actual row count");
            }
            other => panic!("expected InsufficientClassCoverage, got {other:?}"),
        }
    }

    /// Step T3 DoD: when an observed class scores recall below
    /// [`MIN_PER_CLASS_RECALL`] on the held-out set, the validation
    /// gate rejects the model — even if argmax accuracy passes. The
    /// trainer builds the validation set directly so we can pin a
    /// fixture where every "code" window's argmax is "browse".
    #[test]
    fn rejects_when_per_class_recall_below_floor() {
        // Two classes: one with high accuracy (browse, all-zero
        // features) and one with zero recall (code, same all-zero
        // features but different label). The trainer fits "always
        // browse"; recall on code is 0.0.
        let browse_idx = ACTIVITY_CLASSES
            .iter()
            .position(|c| *c == "browse")
            .unwrap();
        let code_idx = ACTIVITY_CLASSES.iter().position(|c| *c == "code").unwrap();
        let mut windows: Vec<LabelledWindow> = Vec::new();
        for _ in 0..40 {
            windows.push(LabelledWindow {
                seq: [[0.0; FEATURE_LEN]; WINDOW_LEN],
                class_idx: browse_idx,
            });
        }
        for _ in 0..20 {
            windows.push(LabelledWindow {
                seq: [[0.0; FEATURE_LEN]; WINDOW_LEN],
                class_idx: code_idx,
            });
        }
        let trained = train_gru(&windows).expect("train_gru on noise-only");
        let bytes = export_onnx(&trained.weights, &FeatureStats::identity()).expect("export_onnx");
        let model = Model::from_onnx_bytes(&bytes).expect("tract decodes");
        let err = run_validation(&model, &windows).expect_err("recall floor must reject");
        assert!(
            err.contains("code recall"),
            "expected error to name code recall, got {err}",
        );
    }

    /// Step T3 DoD: a corpus that ticks every coverage box trains
    /// cleanly through the new gates. 200 rows balanced across all 5
    /// classes with linearly-separable centroids — well above the
    /// per-class floor and far enough above the recall floor that a
    /// converged GRU sails through both checks.
    #[test]
    fn accepts_when_all_observed_classes_meet_floor() {
        let tmp = TempDir::new().expect("tempdir");
        let in_path = tmp.path().join("telemetry.ndjson");
        let mut f = fs::File::create(&in_path).expect("create file");
        let arms = ["idle", "browse", "call", "code", "build"];
        for i in 0..200usize {
            let arm = arms[i % arms.len()];
            // Use the arm name itself as a centroid key so each class
            // gets a distinct feature signature. `browse` / `code`
            // aren't in `class_centroid`'s table — synthesize a
            // distinct centroid here so the trainer can separate
            // them.
            let mut feats = class_centroid(arm);
            match arm {
                "browse" => {
                    feats[3] = 0.85;
                    feats[6] = 0.5;
                }
                "code" => {
                    feats[10] = 0.9;
                    feats[11] = 0.4;
                }
                _ => {}
            }
            let feats = perturb(feats, i as i64);
            writeln!(f, "{}", synth_line(feats, arm, i as i64)).expect("write row");
        }
        let out_path = tmp.path().join("model.onnx");
        let report = retrain_gru(&in_path, &out_path).expect("balanced corpus must train cleanly");
        assert_eq!(report.epochs, EPOCHS);
        assert!(
            report.excluded_classes.is_empty(),
            "full-coverage corpus must exclude nothing, got {:?}",
            report.excluded_classes,
        );
        assert!(
            report.validation_accuracy >= 0.8,
            "expected accuracy ≥ 0.8 on a balanced separable corpus, got {}",
            report.validation_accuracy,
        );
    }

    /// Guard-rail companion to BUG-20260723-2352: a live-shaped class
    /// imbalance (browse ≈ 92%, idle ≈ 6%, code ≈ 2%) over separable
    /// 0-1 centroids must train and clear the per-class recall floor.
    #[test]
    fn trains_through_recall_floor_on_imbalanced_corpus() {
        let tmp = TempDir::new().expect("tempdir");
        let in_path = tmp.path().join("telemetry.ndjson");
        let mut f = fs::File::create(&in_path).expect("create file");
        // ~92% browse / ~6% idle / ~2% code, interleaved so the
        // temporally-last validation split still sees every class:
        // in each 50-row block, 46 browse + 3 idle + 1 code.
        let mut seq = 0i64;
        for _block in 0..20 {
            for slot in 0..50usize {
                let arm = match slot {
                    0 | 20 | 40 => "idle",
                    10 => "code",
                    _ => "browse",
                };
                let mut feats = class_centroid(arm);
                match arm {
                    "browse" => {
                        feats[3] = 0.85;
                        feats[6] = 0.5;
                    }
                    "code" => {
                        feats[10] = 0.9;
                        feats[11] = 0.4;
                    }
                    _ => {}
                }
                let feats = perturb(feats, seq);
                writeln!(f, "{}", synth_line(feats, arm, seq)).expect("write line");
                seq += 1;
            }
        }
        let out_path = tmp.path().join("model.onnx");
        let report = retrain_gru(&in_path, &out_path)
            .expect("imbalanced-but-separable corpus must train and clear the recall floor");
        assert_eq!(report.excluded_classes, vec!["call", "build"]);
        assert!(
            report.validation_accuracy >= 0.8,
            "expected accuracy ≥ 0.8, got {}",
            report.validation_accuracy,
        );
    }

    /// BUG-20260723-2352: the daemon logs RAW sensor features
    /// (tctl ≈ 39-93 °C, package power ≈ 3-54 W, user_idle_s up to
    /// ~32,000 s) but the trainer consumed them unnormalized — Glorot
    /// init × a 4-digit feature saturates every GRU gate, gradients
    /// vanish, the model collapses to the majority class, and the
    /// recall floor (correctly) rejects it. The daemon then stays on
    /// the rules baseline forever. Live-scaled centroids + live-shaped
    /// imbalance must train and ship.
    #[test]
    fn trains_on_raw_scale_live_features() {
        let tmp = TempDir::new().expect("tempdir");
        let in_path = tmp.path().join("telemetry.ndjson");
        let mut f = fs::File::create(&in_path).expect("create file");
        // Raw-scale centroids modelled on the live corpus ranges.
        fn raw_centroid(arm: &str) -> [f32; FEATURE_LEN] {
            let mut v = [0.0f32; FEATURE_LEN];
            v[4] = 90.0; // battery_soc_pct (raw percent)
            v[5] = 1.0; // ac_online
            v[7] = 15.0; // constant on the live host
            match arm {
                "idle" => {
                    v[0] = 44.0; // tctl_c
                    v[1] = 4.0; // package_power_w
                    v[9] = 15_000.0; // user_idle_s — the 4-digit feature
                }
                "browse" => {
                    v[0] = 52.0;
                    v[1] = 7.0;
                    v[2] = 12.0; // igpu busy %
                    v[9] = 30.0;
                }
                "code" => {
                    v[0] = 66.0;
                    v[1] = 22.0;
                    v[6] = 8.0; // psi cpu
                    v[9] = 5.0;
                }
                _ => {}
            }
            v
        }
        // Live-shaped imbalance: 46 browse + 3 idle + 1 code per
        // 50-row block, interleaved so validation sees every class.
        let mut seq = 0i64;
        for _block in 0..20 {
            for slot in 0..50usize {
                let arm = match slot {
                    0 | 20 | 40 => "idle",
                    10 => "code",
                    _ => "browse",
                };
                let mut feats = raw_centroid(arm);
                // Proportional jitter — ±5% of each feature's own
                // magnitude, so raw scales stay realistic.
                let jitter = ((seq as f32) * 0.013).sin() * 0.05;
                for slot in feats.iter_mut() {
                    *slot += *slot * jitter;
                }
                writeln!(f, "{}", synth_line(feats, arm, seq)).expect("write line");
                seq += 1;
            }
        }
        let out_path = tmp.path().join("model.onnx");
        let report = retrain_gru(&in_path, &out_path)
            .expect("raw-scale separable corpus must train and clear the recall floor");
        assert_eq!(report.excluded_classes, vec!["call", "build"]);
        assert!(
            report.validation_accuracy >= 0.8,
            "expected accuracy ≥ 0.8 on separable raw-scale corpus, got {}",
            report.validation_accuracy,
        );
    }

    /// BUG-20260723-2352 facet 2: rows applied under a
    /// condition-reactive shield (`HOT`, `BATTERY_LOW`) carry the
    /// shield's clamped arm, not the user's activity — on the live
    /// host every `idle`-arm row sat at >70 °C under `shield:HOT`
    /// (the thermal clamp), poisoning the idle class with flat-out
    /// feature vectors. Such rows must be dropped from the corpus;
    /// nominal (`COOL_AC`) and activity-derived (`MEETING`) rows are
    /// kept. Here the shielded "idle" rows wear code-shaped features —
    /// keeping them would poison training; dropping them leaves idle
    /// under-covered and excluded.
    #[test]
    fn drops_rows_clamped_by_reactive_shields() {
        let tmp = TempDir::new().expect("tempdir");
        let in_path = tmp.path().join("telemetry.ndjson");
        let mut f = fs::File::create(&in_path).expect("create file");
        let mut seq = 0i64;
        // 60 poisoned "idle" rows: HOT-clamped, code-shaped features.
        for _ in 0..30 {
            let feats = perturb(class_centroid("build"), seq);
            writeln!(
                f,
                "{}",
                synth_line_with_shield(feats, "idle", seq, Some("HOT")),
            )
            .expect("write hot row");
            seq += 1;
            let feats = perturb(class_centroid("call"), seq);
            writeln!(
                f,
                "{}",
                synth_line_with_shield(feats, "whisper", seq, Some("BATTERY_LOW")),
            )
            .expect("write battery row");
            seq += 1;
        }
        // 200 nominal browse/code rows under COOL_AC.
        for _ in 0..100 {
            let mut feats = class_centroid("browse");
            feats[3] = 0.85;
            feats[6] = 0.5;
            let feats = perturb(feats, seq);
            writeln!(
                f,
                "{}",
                synth_line_with_shield(feats, "browse", seq, Some("COOL_AC")),
            )
            .expect("write browse row");
            seq += 1;
            let mut feats = class_centroid("code");
            feats[10] = 0.9;
            feats[11] = 0.4;
            let feats = perturb(feats, seq);
            writeln!(
                f,
                "{}",
                synth_line_with_shield(feats, "code", seq, Some("COOL_AC")),
            )
            .expect("write code row");
            seq += 1;
        }
        let out_path = tmp.path().join("model.onnx");
        let report = retrain_gru(&in_path, &out_path)
            .expect("nominal rows must train once shield-clamped rows are dropped");
        assert_eq!(
            report.rows_used, 200,
            "only the 200 nominal COOL_AC rows may train",
        );
        assert!(
            report.excluded_classes.contains(&"idle"),
            "idle must be excluded once its only rows are shield-clamped, got {:?}",
            report.excluded_classes,
        );
    }

    /// Mapping smoke-test — covers every branch of `arm_to_class_idx`
    /// so the documented label table doesn't drift from the code.
    #[test]
    fn arm_to_class_idx_covers_every_canonical_arm() {
        let cases = [
            ("idle", "idle"),
            ("whisper", "idle"),
            ("browse", "browse"),
            ("call", "call"),
            ("code", "code"),
            ("build", "build"),
            ("flat-out", "build"),
            ("npu-burst", "build"),
        ];
        for (arm, expected) in cases {
            let idx = arm_to_class_idx(arm).unwrap_or_else(|| panic!("arm {arm} should map"));
            assert_eq!(
                ACTIVITY_CLASSES[idx], expected,
                "arm {arm} should map to {expected}",
            );
        }
        assert!(arm_to_class_idx("ludicrous").is_none());
    }
}
