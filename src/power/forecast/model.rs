//! GRU model wrapper + hot-reload primitives.
//!
//! The daemon (Step 26) holds an `ArcSwap<Model>` that the trainer
//! (Step 25) replaces on a successful retrain. Inference paths (Step
//! 24's [`super::gru::infer`]) borrow the live `Model` via
//! [`arc_swap::ArcSwap::load`], which is wait-free — the hot path
//! never blocks the trainer's swap and the trainer never blocks the
//! daemon's tick.
//!
//! ## Anatomy
//!
//! A [`Model`] bundles the runnable tract graph with the metadata the
//! audit log + `sy power status --json` need to identify it:
//!
//! - `version_sha` — short prefix of the BLAKE3 of the source ONNX
//!   bytes; appears verbatim under `model.version_sha` in
//!   `sy.power.status/v1`. The warmup fixture's SHA is pinned in
//!   [`WARMUP_VERSION_SHA`] for tests that need to assert the daemon
//!   is running with the shipped baseline.
//! - `input_dim` — pulled from the ONNX input shape so the daemon can
//!   refuse a feature-window of the wrong width before calling
//!   `run()`.
//! - `horizon_s` — the projection horizon the model was trained for
//!   (Step 25 will plumb this through the ONNX metadata; the warmup
//!   uses [`DEFAULT_HORIZON_S`]).
//!
//! ## Warmup fixture
//!
//! The shipped `fixtures/warmup.onnx` is embedded via `include_bytes!`
//! so the daemon can always cold-start without a pre-trained model on
//! disk — matches SPEC §3 "Onboarding under rules-only control".

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tract_onnx::prelude::*;
use tract_onnx::tract_hir::internal::DimLike;

/// Bytes of the shipped warmup ONNX. Regenerate via `cargo run
/// --example gen_warmup_gru`; the byte-identity contract is enforced
/// by `tests/forecast_reproducibility.rs`.
pub const WARMUP_ONNX: &[u8] = include_bytes!("fixtures/warmup.onnx");

/// Five activity classes the bandit picks between, in the canonical
/// order used everywhere the [`super::Forecast::class_probs`] vector
/// is consumed (audit log, MCP, status command). Pinned forever:
/// reordering breaks the audit replay (Step 23) the same way
/// reordering the snapshot feature vec would.
pub const ACTIVITY_CLASSES: [&str; 5] = ["idle", "browse", "call", "code", "build"];

/// Number of activity classes — convenience constant so callers don't
/// repeat `ACTIVITY_CLASSES.len()`.
pub const FORECAST_CLASS_COUNT: usize = ACTIVITY_CLASSES.len();

/// Default projection horizon for the warmup fixture (SPEC §2
/// "30-120 s"; midpoint). Trained models from Step 25 onward carry
/// their own horizon in the ONNX metadata.
pub const DEFAULT_HORIZON_S: u32 = 90;

/// Stable identifier for the shipped warmup ONNX — used in
/// `sy.power.status/v1` to advertise the rules-baseline floor. The
/// real SHA is computed at load time, but the daemon's onboarding gate
/// (Step 26) checks for this literal to know it's still on rules.
pub const WARMUP_VERSION_SHA: &str = "rules-baseline";

/// Runnable tract graph + metadata. Construct via [`Model::from_onnx_bytes`]
/// or [`Model::warmup`].
pub struct Model {
    runnable: SimpleTractModel,
    pub version_sha: String,
    pub input_dim: usize,
    pub horizon_s: u32,
}

/// Type alias matching tract 0.22's optimised runnable model. The
/// nested generics make every call-site noisy; pinning the alias here
/// keeps Step 24 + Step 25's trainer-side validation in sync.
pub type SimpleTractModel =
    RunnableModel<TypedFact, Box<dyn TypedOp>, Graph<TypedFact, Box<dyn TypedOp>>>;

impl std::fmt::Debug for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Model")
            .field("version_sha", &self.version_sha)
            .field("input_dim", &self.input_dim)
            .field("horizon_s", &self.horizon_s)
            .finish_non_exhaustive()
    }
}

impl Model {
    /// Load the shipped warmup ONNX. Never fails on a clean checkout —
    /// the fixture is byte-stable per the reproducibility test.
    pub fn warmup() -> Result<Self> {
        let mut model = Self::from_onnx_bytes(WARMUP_ONNX).context("decode shipped warmup.onnx")?;
        model.version_sha = WARMUP_VERSION_SHA.into();
        model.horizon_s = DEFAULT_HORIZON_S;
        Ok(model)
    }

    /// Decode an ONNX byte buffer into a runnable [`Model`]. The
    /// fingerprint defaults to the BLAKE3-hex prefix of the input
    /// bytes; callers (Step 25's trainer) may overwrite it.
    pub fn from_onnx_bytes(bytes: &[u8]) -> Result<Self> {
        let mut cursor = std::io::Cursor::new(bytes);
        let runnable = tract_onnx::onnx()
            .model_for_read(&mut cursor)
            .context("tract: parse onnx")?
            .into_optimized()
            .context("tract: optimise")?
            .into_runnable()
            .context("tract: into_runnable")?;
        let input_dim = runnable_input_dim(&runnable)?;
        let version_sha = blake3::hash(bytes).to_hex()[..12].to_string();
        Ok(Self {
            runnable,
            version_sha,
            input_dim,
            horizon_s: DEFAULT_HORIZON_S,
        })
    }

    /// Borrow the underlying runnable graph. Used by
    /// [`super::gru::infer`] only — kept on the impl so the field stays
    /// private (callers can't poke at tract internals out from under
    /// the metadata wrapper).
    pub(crate) fn runnable(&self) -> &SimpleTractModel {
        &self.runnable
    }
}

/// Pull the (last-axis) input width from the optimised tract model.
/// The warmup ONNX declares `[1, FEATURE_LEN]` so this returns 12.
fn runnable_input_dim(model: &SimpleTractModel) -> Result<usize> {
    let fact = model
        .model()
        .input_fact(0)
        .map_err(|e| anyhow!("tract: input_fact(0): {e}"))?;
    let shape = &fact.shape;
    let last = shape
        .dims()
        .last()
        .ok_or_else(|| anyhow!("tract input has zero-rank shape"))?;
    let n = last
        .to_usize()
        .map_err(|e| anyhow!("tract input dim non-concrete: {e}"))?;
    Ok(n)
}

/// Thin newtype around `ArcSwap<Arc<Model>>`. Exposed so the daemon
/// (Step 26) and `examples/`/tests can share a single hot-reload
/// primitive without re-deriving the `Arc<Arc<_>>` shape.
pub struct ModelStore(arc_swap::ArcSwap<Model>);

impl ModelStore {
    /// Seed the store with an initial model. Typically the warmup —
    /// the trainer (Step 25) calls [`Self::store`] later.
    pub fn new(initial: Model) -> Self {
        Self(arc_swap::ArcSwap::from_pointee(initial))
    }

    /// Snapshot the live model. Wait-free per `arc-swap`'s contract;
    /// safe to call from the daemon's 1 Hz tick.
    pub fn load(&self) -> arc_swap::Guard<Arc<Model>> {
        self.0.load()
    }

    /// Replace the live model. The trainer (Step 25) calls this after
    /// a successful retrain + tract-validation; existing readers keep
    /// their `Arc<Model>` until they drop it.
    pub fn store(&self, next: Model) {
        self.0.store(Arc::new(next));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Step 24 DoD: the shipped warmup ONNX loads through the
    /// production path, fixes `input_dim` to the feature-vec width,
    /// and advertises the rules-baseline version SHA so the daemon's
    /// onboarding gate can recognise it.
    #[test]
    fn warmup_model_loads() {
        let model = Model::warmup().expect("warmup loads");
        assert_eq!(model.input_dim, 12);
        assert_eq!(model.version_sha, WARMUP_VERSION_SHA);
        assert_eq!(model.horizon_s, DEFAULT_HORIZON_S);
    }

    /// Step 24 DoD: ArcSwap-backed hot reload. Load model A, infer;
    /// store model B; the next `load()` returns B. We use two
    /// distinguishable Models: the warmup (real ONNX) and a second
    /// instance with a hand-overwritten `version_sha` so the swap is
    /// observable without needing a second ONNX file.
    #[test]
    fn arc_swap_hot_reload() {
        let store = ModelStore::new(Model::warmup().expect("warmup A"));
        let before = store.load().version_sha.clone();
        assert_eq!(before, WARMUP_VERSION_SHA);

        let mut next = Model::warmup().expect("warmup B");
        next.version_sha = "test-swap".into();
        store.store(next);

        let after = store.load().version_sha.clone();
        assert_eq!(after, "test-swap");
        assert_ne!(before, after);
    }
}
