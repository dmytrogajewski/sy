//! sy-power Step 24 DoD: the shipped `warmup.onnx` is reproducible.
//!
//! `examples/gen_warmup_gru.rs` is the canonical generator; this test
//! pulls in the same function via `#[path]` and asserts the bytes it
//! produces are byte-identical to the file checked in under
//! `src/power/forecast/fixtures/warmup.onnx`. Touching the generator
//! without re-running `cargo run --example gen_warmup_gru` leaves the
//! repo inconsistent — this test surfaces that immediately.
//!
//! The dual-import pattern (example + test both pulling the same file
//! via `#[path]`) is intentional: the example is the human-facing
//! entry point (it writes the file), and the test is the machine-
//! facing guard (it refuses CI on drift).

#[path = "../examples/gen_warmup_gru.rs"]
#[allow(dead_code)]
mod gen;

const SHIPPED: &[u8] = include_bytes!("../src/power/forecast/fixtures/warmup.onnx");

#[test]
fn warmup_onnx_is_byte_identical_to_generator() {
    let regenerated = gen::build_warmup_onnx();
    assert_eq!(
        regenerated.len(),
        SHIPPED.len(),
        "warmup.onnx length drifted: shipped={} generated={} — \
         re-run `cargo run --example gen_warmup_gru` to refresh the fixture",
        SHIPPED.len(),
        regenerated.len(),
    );
    assert_eq!(
        regenerated.as_slice(),
        SHIPPED,
        "warmup.onnx bytes drifted — \
         re-run `cargo run --example gen_warmup_gru` to refresh the fixture",
    );
}
