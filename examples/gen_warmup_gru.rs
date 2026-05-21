//! Reproducible builder for `src/power/forecast/fixtures/warmup.onnx`.
//!
//! Per `specs/roadmaps/sy-power/ROADMAP.md` Step 24: the daemon needs
//! a "rules-equivalent" forecaster to hot-load before the first
//! offline retrain (Step 25). The warmup model takes the pinned
//! 12-channel feature window and emits uniform class probabilities
//! across the five activity buckets — the documented rules-baseline
//! floor (SPEC §3 "Onboarding under rules-only control").
//!
//! ## Run
//!
//! ```bash
//! cargo run --example gen_warmup_gru
//! ```
//!
//! Re-runs are byte-identical: the integration test
//! `tests/forecast_reproducibility.rs` regenerates these bytes in-process
//! and asserts equality against the shipped fixture. Touching the
//! generator without re-running it leaves CI red — the intended gate.

use std::fs;
use std::path::PathBuf;

use prost::Message;
use tract_onnx::pb;

/// Width of the feature window — must equal `power::snapshot::FEATURE_LEN`
/// so the ONNX input shape matches the daemon's snapshot vector.
const FEATURE_LEN: i64 = 12;

/// Number of activity classes the bandit picks between
/// (`idle | browse | call | code | build`).
const NUM_CLASSES: i64 = 5;

/// Uniform-probability floor over `NUM_CLASSES` buckets. Encoded as
/// the constant emission of the warmup graph — every inference returns
/// this distribution until the trainer (Step 25) lands a personalised
/// ONNX.
const UNIFORM_PROB: f32 = 1.0 / NUM_CLASSES as f32;

/// ONNX IR version 7 — chosen because tract 0.22 imports IRv7
/// natively (no opset upgrade), and burn 0.20's exporter (Step 25)
/// also targets the same IR major. Higher IRs add functions/training
/// fields the warmup model doesn't need.
const IR_VERSION: i64 = 7;

/// ONNX standard opset version 13 — the lowest version that contains
/// `Constant` with the `value` attribute as a `TensorProto`. The
/// `Identity` op used to keep the input referenced (so the input slot
/// isn't dead in the graph) is stable since opset 1.
const OPSET_VERSION: i64 = 13;

/// `TensorProto.data_type` enum value for FLOAT (IEEE 754 single).
/// Mirrors `onnx.proto` `TensorProto.DataType.FLOAT = 1`.
const TENSOR_FLOAT: i32 = 1;

/// `AttributeProto.type` enum value for TENSOR. Matches
/// `onnx.proto` `AttributeProto.AttributeType.TENSOR = 4`.
const ATTR_TENSOR: i32 = 4;

fn main() -> std::io::Result<()> {
    let bytes = build_warmup_onnx();
    let out =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/power/forecast/fixtures/warmup.onnx");
    fs::create_dir_all(out.parent().expect("fixtures parent"))?;
    fs::write(&out, &bytes)?;
    println!("wrote {} ({} bytes)", out.display(), bytes.len());
    Ok(())
}

/// Build the rules-equivalent warmup ONNX graph in memory and return
/// its protobuf-serialised bytes. Public for the reproducibility test
/// in `tests/forecast_reproducibility.rs`.
pub fn build_warmup_onnx() -> Vec<u8> {
    let probs_tensor = pb::TensorProto {
        dims: vec![1, NUM_CLASSES],
        data_type: TENSOR_FLOAT,
        float_data: vec![UNIFORM_PROB; NUM_CLASSES as usize],
        name: "warmup_uniform".into(),
        ..Default::default()
    };
    let const_attr = pb::AttributeProto {
        name: "value".into(),
        r#type: ATTR_TENSOR,
        t: Some(probs_tensor),
        ..Default::default()
    };
    // `Identity(features) -> features_kept` keeps the input live in
    // the graph so tract doesn't trim the input slot during
    // `into_optimized()`. The output is otherwise unused — the
    // warmup model's true output comes from the Constant node.
    let identity_node = pb::NodeProto {
        input: vec!["features".into()],
        output: vec!["features_kept".into()],
        op_type: "Identity".into(),
        ..Default::default()
    };
    let const_node = pb::NodeProto {
        output: vec!["probs".into()],
        op_type: "Constant".into(),
        attribute: vec![const_attr],
        ..Default::default()
    };
    // The warmup graph carries a rank-3 input shape `[seq=1, batch=1,
    // features=12]` to match the trainer-emitted ONNX (Step P2-1 GRU).
    // The trainer's `GRU` op consumes `[seq, batch, input]`; declaring
    // the same shape here lets `Model::from_onnx_bytes` succeed on both
    // ONNX flavours without a special-case in
    // `runnable_input_dim` — the last-axis-as-input-width contract is
    // preserved.
    let graph = pb::GraphProto {
        node: vec![identity_node, const_node],
        name: "warmup_gru".into(),
        input: vec![value_info("features", &[1, 1, FEATURE_LEN])],
        output: vec![value_info("probs", &[1, NUM_CLASSES])],
        ..Default::default()
    };
    let model = pb::ModelProto {
        ir_version: IR_VERSION,
        opset_import: vec![pb::OperatorSetIdProto {
            domain: String::new(),
            version: OPSET_VERSION,
        }],
        producer_name: "sy-power".into(),
        producer_version: env!("CARGO_PKG_VERSION").into(),
        graph: Some(graph),
        ..Default::default()
    };
    model.encode_to_vec()
}

fn value_info(name: &str, shape: &[i64]) -> pb::ValueInfoProto {
    let dims = shape
        .iter()
        .map(|d| pb::tensor_shape_proto::Dimension {
            value: Some(pb::tensor_shape_proto::dimension::Value::DimValue(*d)),
            denotation: String::new(),
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
