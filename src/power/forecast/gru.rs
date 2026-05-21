//! tract-backed GRU inference. Per SPEC §2 ("GRU on CPU via tract,
//! not NPU"): a sub-ms forward pass on the optimised graph. The hot
//! path is `Snapshot.features` (12 f32) → `Forecast` (5 class
//! probabilities + horizon).
//!
//! Inference is stateless — the runnable tract model carries any
//! internal hidden-state buffers it needs across `.run()` calls. The
//! 1 Hz daemon tick re-feeds the latest 12-channel window each tick;
//! Step 25's trainer learns the temporal structure from the rolled-up
//! NDJSON log, not from a runtime-held hidden state.
//!
//! The Step-24 warmup model is a Constant emitter — every call
//! returns uniform `0.2` probabilities. Later trained models will
//! return non-uniform distributions; the wire shape stays the same so
//! the bandit + audit log don't notice the swap (Step 25 + Step 26).

use anyhow::{anyhow, Context, Result};
use tract_onnx::prelude::*;

use super::model::{Model, ACTIVITY_CLASSES, FORECAST_CLASS_COUNT};
use super::Forecast;

/// Run one forward pass against `model` over `features`. Returns a
/// [`Forecast`] over [`ACTIVITY_CLASSES`] tagged with the model's
/// configured horizon.
///
/// Errors:
///
/// - `features.len()` differs from `model.input_dim` — the daemon
///   should never feed a partial window, but Step 24 surfaces the
///   error rather than masking it.
/// - The underlying tract `.run()` errored (rare on optimised graphs;
///   bubbled up so the daemon can fall back to rules).
pub fn infer(model: &Model, features: &[f32]) -> Result<Forecast> {
    if features.len() != model.input_dim {
        return Err(anyhow!(
            "forecast::infer: expected {} features, got {}",
            model.input_dim,
            features.len(),
        ));
    }
    // Per Step P2-1: the trainer's ONNX is a GRU consuming `[seq,
    // batch, input]`. The daemon feeds one snapshot per tick so seq=1
    // and the GRU runs a single step (h_0 = 0). Wire shape stays the
    // same on the function boundary — callers still pass a flat
    // `&[f32]` of length `model.input_dim`.
    let input = tract_ndarray::Array3::from_shape_vec((1, 1, features.len()), features.to_vec())
        .context("build [1, 1, N] input tensor")?
        .into_tensor();
    let outputs = model
        .runnable()
        .run(tvec!(input.into()))
        .context("tract: run")?;
    let probs = decode_probs(&outputs)?;
    Ok(Forecast {
        horizon_s: model.horizon_s,
        class_probs: ACTIVITY_CLASSES
            .iter()
            .zip(probs.iter())
            .map(|(name, p)| ((*name).to_string(), *p))
            .collect(),
    })
}

/// Extract a `[FORECAST_CLASS_COUNT]` probability vector from the
/// tract output bundle. The warmup model emits `[1, 5]` directly; a
/// future trained model is free to emit a longer head as long as the
/// last axis stays five-wide.
fn decode_probs(outputs: &TVec<TValue>) -> Result<[f32; FORECAST_CLASS_COUNT]> {
    let raw = outputs
        .first()
        .ok_or_else(|| anyhow!("tract returned no outputs"))?;
    let view = raw
        .to_array_view::<f32>()
        .map_err(|e| anyhow!("tract output not f32: {e}"))?;
    let flat: Vec<f32> = view.iter().copied().collect();
    if flat.len() < FORECAST_CLASS_COUNT {
        return Err(anyhow!(
            "tract output has {} elems, expected ≥ {}",
            flat.len(),
            FORECAST_CLASS_COUNT,
        ));
    }
    let mut out = [0f32; FORECAST_CLASS_COUNT];
    out.copy_from_slice(&flat[..FORECAST_CLASS_COUNT]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Number of inference iterations sampled for the p99 bench. 512
    /// is enough samples for a stable p99 on a CPU model that should
    /// hover in the single-digit µs range; small enough to keep
    /// `cargo test --ignored` runs snappy.
    const BENCH_ITERS: usize = 512;
    const P99_BUDGET_US: u128 = 1_000;
    const FEATURE_LEN: usize = 12;

    /// Step 24 DoD: warmup ONNX loads and inference returns the
    /// documented uniform distribution.
    #[test]
    fn warmup_model_loads() {
        let model = Model::warmup().expect("warmup");
        let features = [0.0f32; FEATURE_LEN];
        let forecast = infer(&model, &features).expect("infer");
        assert_eq!(forecast.class_probs.len(), FORECAST_CLASS_COUNT);
        let names: Vec<&str> = forecast
            .class_probs
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(names, ACTIVITY_CLASSES);
        let total: f32 = forecast.class_probs.iter().map(|(_, p)| *p).sum();
        assert!(
            (total - 1.0).abs() < 1e-5,
            "class probs must sum to 1.0, got {total}",
        );
        for (_, p) in &forecast.class_probs {
            assert!(
                (p - 0.2).abs() < 1e-5,
                "warmup must be uniform 0.2, got {p}"
            );
        }
    }

    /// Step 24 DoD (gated): inference latency p99 stays under 1 ms on
    /// Zen5. `#[ignore]` gates the assertion behind a `--ignored`
    /// flag for laptops with cold caches per the roadmap note.
    #[test]
    #[ignore = "bench-style p99 — run with `cargo test -- --ignored`"]
    fn infer_under_1ms_p99() {
        let model = Model::warmup().expect("warmup");
        let features = [0.0f32; FEATURE_LEN];
        // Discard one warm-up call so allocator / tract lazy paths
        // don't pollute the first sample.
        let _ = infer(&model, &features).expect("warmup infer");
        let mut samples: Vec<u128> = Vec::with_capacity(BENCH_ITERS);
        for _ in 0..BENCH_ITERS {
            let t0 = Instant::now();
            let _ = infer(&model, &features).expect("infer");
            samples.push(t0.elapsed().as_micros());
        }
        samples.sort_unstable();
        let p99 = samples[(BENCH_ITERS * 99) / 100];
        assert!(
            p99 <= P99_BUDGET_US,
            "p99 latency {p99} µs exceeds {P99_BUDGET_US} µs",
        );
    }

    /// Surface the input-shape mismatch as a real error — the daemon's
    /// onboarding gate (Step 26) refuses to call `infer` with a
    /// half-built feature window, but Step 24 must still treat the
    /// wrong-size case as `Err`, not a panic.
    #[test]
    fn infer_rejects_wrong_feature_width() {
        let model = Model::warmup().expect("warmup");
        let too_short = [0.0f32; FEATURE_LEN - 1];
        let err = infer(&model, &too_short).expect_err("too short must fail");
        assert!(
            err.to_string().contains("expected 12"),
            "error must mention expected width, got {err}",
        );
    }
}
