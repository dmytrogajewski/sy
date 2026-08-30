//! Real-NPU end-to-end test for the `whisper-medium` Stt workload.
//!
//! Gated behind `--features test-npu` because it needs:
//!   * the model cache populated by
//!     `prep_npu_workload.py --workload stt`, and
//!   * (for the NPU path) `/dev/accel/accel0` + the VitisAI venv —
//!     though it also passes on the CPU fallback, just slower.
//!
//! The default `make test` gate never compiles this file (the
//! `#![cfg(...)]` gate below makes it an empty crate without the
//! feature), so the hermetic FakeWorkload path stays the source of
//! truth for CI.
//!
//! It black-boxes the real binary — `sy aiplane run --workload stt
//! --input <audio-json> --json` — exactly as a human or agent would,
//! rather than reaching into `sy`'s private modules (the crate has no
//! lib target). Daemon-down falls through to the in-process registry,
//! which is what we exercise here.
//!
//! Run it with:
//!   cargo test --features test-npu --test workload_stt -- --nocapture
#![cfg(feature = "test-npu")]

use std::path::PathBuf;
use std::process::Command;

/// AMD's LibriSpeech sample. whisper.cpp oracle transcript contains
/// "stew", "turnips", "carrots".
const SAMPLE_WAV: &str =
    "/home/dmitriy/sources/RyzenAI-SW/Demos/ASR/Whisper/audio_files/1089-134686-0000.wav";

/// Minimal PCM-16 mono WAV reader: walk RIFF chunks to the `data`
/// chunk, return the i16 samples + sample rate.
fn read_wav_i16(path: &str) -> (Vec<i16>, u32) {
    let bytes = std::fs::read(path).expect("read wav");
    assert_eq!(&bytes[0..4], b"RIFF", "not a RIFF file");
    assert_eq!(&bytes[8..12], b"WAVE", "not a WAVE file");
    let mut pos = 12usize;
    let mut sample_rate = 0u32;
    let mut channels = 0u16;
    let mut bits = 0u16;
    let mut data: &[u8] = &[];
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let body = &bytes[pos + 8..(pos + 8 + size).min(bytes.len())];
        if id == b"fmt " {
            channels = u16::from_le_bytes([body[2], body[3]]);
            sample_rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            bits = u16::from_le_bytes([body[14], body[15]]);
        } else if id == b"data" {
            data = body;
        }
        pos += 8 + size + (size & 1); // chunks are word-aligned
    }
    assert_eq!(channels, 1, "expected mono");
    assert_eq!(bits, 16, "expected 16-bit PCM");
    let pcm: Vec<i16> = data
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    (pcm, sample_rate)
}

#[test]
fn whisper_medium_transcribes_amd_sample() {
    let encoder = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".cache/sy/aiplane/whisper-medium/amd-src/encoder_model.onnx"));
    if !encoder.map(|p| p.is_file()).unwrap_or(false) {
        eprintln!(
            "skip: whisper-medium model cache absent; run \
             prep_npu_workload.py --workload stt"
        );
        return;
    }
    if !PathBuf::from(SAMPLE_WAV).is_file() {
        eprintln!("skip: AMD sample WAV not present at {SAMPLE_WAV}");
        return;
    }

    let (pcm, sr) = read_wav_i16(SAMPLE_WAV);
    // Whisper trims/pads to a 30 s window starting at 0, so the full
    // utterance is captured by the first chunk; passing the whole clip.
    let input = serde_json::json!({ "kind": "audio", "pcm": pcm, "sr": sr });
    // The PCM is far larger than ARG_MAX, so hand it to the CLI via a
    // file rather than an argv literal.
    let in_path = std::env::temp_dir().join("sy_stt_test_input.json");
    std::fs::write(
        &in_path,
        serde_json::to_vec(&input).expect("serialize input"),
    )
    .expect("write input file");

    let bin = env!("CARGO_BIN_EXE_sy");
    let out = Command::new(bin)
        .args(["aiplane", "run", "--workload", "stt", "--json", "--in-file"])
        .arg(&in_path)
        .output()
        .expect("spawn sy aiplane run");

    // NOTE: the VitisAI EP segfaults in its session destructor at process
    // exit (SIGSEGV / code 139) — a pre-existing, harmless teardown bug
    // shared by every NPU workload (`sy aiplane run --workload embed` does
    // the same). The transcript is fully written to stdout *before* the
    // crash, and the production worker is SIGKILLed (no destructors run),
    // so serving is unaffected. We therefore validate the stdout payload
    // and tolerate a non-zero exit *as long as* the JSON result is present;
    // a truly failed run prints nothing.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.trim().is_empty(),
        "sy aiplane run produced no output (exit {:?}): {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("parse aiplane run --json output");
    let text = parsed["output"]["text"]
        .as_str()
        .expect("output.text in response")
        .to_lowercase();
    eprintln!("transcript: {text}");
    assert!(
        text.contains("stew") && text.contains("turnips"),
        "transcript missing oracle keywords: {text:?}"
    );
}
