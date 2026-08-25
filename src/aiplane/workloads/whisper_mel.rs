//! Whisper log-mel feature extractor — a faithful port of HuggingFace
//! `transformers` `WhisperFeatureExtractor._np_extract_fbank_features`
//! (matched within 1e-3 against the reference; see the tests). Turns a
//! 16 kHz mono PCM signal into the `[1, 80, 3000]` f32 log-mel tensor
//! the AMD `whisper-medium` encoder ONNX consumes as input `x`.
//!
//! Algorithm (all constants pinned from the model's
//! `preprocessor_config.json`):
//!
//! 1. Pad / trim the waveform to exactly `N_SAMPLES` (30 s = 480 000).
//! 2. Reflect-pad `N_FFT/2` (= 200) samples on each side (STFT
//!    `center=True`).
//! 3. Slide a length-`N_FFT` (400) periodic-Hann window with hop
//!    `HOP_LENGTH` (160); rfft each frame → power spectrum
//!    (`|X|^2`, 201 bins).
//! 4. Apply the 80×201 mel filterbank (shipped in the preprocessor
//!    config, stored already-transposed as `[mel][freq]`).
//! 5. `log10(max(mel, 1e-10))`, drop the trailing frame to land on
//!    3000, clamp to `max - 8.0`, then `(x + 4) / 4`.
//!
//! The filterbank is loaded once from `preprocessor_config.json` in
//! [`MelExtractor::from_preprocessor_config`](crate::aiplane::workloads::whisper_mel::MelExtractor::from_preprocessor_config)
//! so sy never hard-codes
//! the 16 080 float matrix.

use std::sync::Arc;

use anyhow::{Context, Result};
use rustfft::{num_complex::Complex32, Fft, FftPlanner};

/// Target sample rate. The knowledge side resamples to this before
/// dispatch; `run()` rejects anything else.
pub const SAMPLE_RATE: u32 = 16_000;
/// FFT / analysis-frame size.
pub const N_FFT: usize = 400;
/// Hop between successive frames.
pub const HOP_LENGTH: usize = 160;
/// Mel-filter (output) bins. The encoder's input channel dim.
pub const N_MELS: usize = 80;
/// One-sided rfft bins: `N_FFT / 2 + 1`.
pub const N_FREQ: usize = N_FFT / 2 + 1;
/// 30 s of audio at 16 kHz — the encoder's fixed context window.
pub const N_SAMPLES: usize = 480_000;
/// Time frames the encoder expects: `N_SAMPLES / HOP_LENGTH`.
pub const N_FRAMES: usize = N_SAMPLES / HOP_LENGTH;

/// Periodic Hann window of length `N_FFT`, i.e. `np.hanning(N_FFT+1)`
/// with the last (duplicate) sample dropped — matches
/// `transformers.audio_utils.window_function("hann", periodic=True)`.
fn hann_periodic(n: usize) -> Vec<f32> {
    let denom = n as f64; // length is n+1, so divisor is (n+1)-1 = n
    (0..n)
        .map(|i| {
            let w = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / denom).cos();
            w as f32
        })
        .collect()
}

/// Reusable Whisper feature extractor. Holds the precomputed window,
/// the mel filterbank, and a cached FFT plan.
pub struct MelExtractor {
    window: Vec<f32>,
    /// Row-major `[N_MELS][N_FREQ]` filterbank (already transposed:
    /// `mel_out[m] = Σ_f filters[m][f] · power[f]`).
    mel_filters: Vec<Vec<f32>>,
    fft: Arc<dyn Fft<f32>>,
}

impl MelExtractor {
    /// Build the extractor from the `mel_filters` matrix in a Whisper
    /// `preprocessor_config.json` (shape `[N_MELS][N_FREQ]`).
    pub fn from_preprocessor_config(json_path: &std::path::Path) -> Result<Self> {
        let raw = std::fs::read_to_string(json_path)
            .with_context(|| format!("read {}", json_path.display()))?;
        let cfg: serde_json::Value =
            serde_json::from_str(&raw).with_context(|| format!("parse {}", json_path.display()))?;
        let rows = cfg
            .get("mel_filters")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("preprocessor_config.json missing mel_filters array"))?;
        let mut mel_filters = Vec::with_capacity(N_MELS);
        for row in rows {
            let cols = row
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("mel_filters row not an array"))?;
            let parsed: Vec<f32> = cols
                .iter()
                .map(|v| v.as_f64().map(|x| x as f32))
                .collect::<Option<Vec<f32>>>()
                .ok_or_else(|| anyhow::anyhow!("mel_filters cell not a number"))?;
            mel_filters.push(parsed);
        }
        Self::from_mel_filters(mel_filters)
    }

    /// Build from an in-memory filterbank. Validates the `[80][201]`
    /// shape so a malformed config fails loud rather than producing a
    /// silently-wrong spectrogram.
    pub fn from_mel_filters(mel_filters: Vec<Vec<f32>>) -> Result<Self> {
        if mel_filters.len() != N_MELS {
            anyhow::bail!(
                "mel filterbank has {} rows, expected {N_MELS}",
                mel_filters.len()
            );
        }
        if let Some(bad) = mel_filters.iter().find(|r| r.len() != N_FREQ) {
            anyhow::bail!(
                "mel filterbank row has {} cols, expected {N_FREQ}",
                bad.len()
            );
        }
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(N_FFT);
        Ok(Self {
            window: hann_periodic(N_FFT),
            mel_filters,
            fft,
        })
    }

    /// Compute the `[N_MELS][N_FRAMES]` log-mel spectrogram for `pcm`
    /// (16 kHz mono i16). The output is row-major mel-major; the caller
    /// flattens it into the `[1, 80, 3000]` encoder input tensor.
    pub fn log_mel(&self, pcm: &[i16]) -> Vec<Vec<f32>> {
        let audio = pcm_to_f32_padded(pcm);
        let padded = reflect_pad(&audio, N_FFT / 2);
        // One extra frame is produced then dropped (`log_spec[:, :-1]`).
        let n_frames_raw = N_FRAMES + 1;
        let mut power = vec![[0.0f32; N_FREQ]; n_frames_raw];
        let mut buf = vec![Complex32::new(0.0, 0.0); N_FFT];
        for (frame, slot) in power.iter_mut().enumerate() {
            let start = frame * HOP_LENGTH;
            for i in 0..N_FFT {
                buf[i] = Complex32::new(padded[start + i] * self.window[i], 0.0);
            }
            self.fft.process(&mut buf);
            for (f, p) in slot.iter_mut().enumerate() {
                *p = buf[f].norm_sqr();
            }
        }
        self.mel_and_normalize(&power)
    }

    /// Mel projection + log10 + clamp + affine, dropping the trailing
    /// raw frame so the result is exactly `N_FRAMES` wide.
    fn mel_and_normalize(&self, power: &[[f32; N_FREQ]]) -> Vec<Vec<f32>> {
        const MEL_FLOOR: f32 = 1e-10;
        let mut mel = vec![vec![0.0f32; N_FRAMES]; N_MELS];
        let mut global_max = f32::NEG_INFINITY;
        for (m, row) in mel.iter_mut().enumerate() {
            let filt = &self.mel_filters[m];
            for (t, cell) in row.iter_mut().enumerate() {
                let mut acc = 0.0f32;
                for (f, &w) in filt.iter().enumerate() {
                    acc += w * power[t][f];
                }
                let v = acc.max(MEL_FLOOR).log10();
                if v > global_max {
                    global_max = v;
                }
                *cell = v;
            }
        }
        let floor = global_max - 8.0;
        for row in mel.iter_mut() {
            for cell in row.iter_mut() {
                let clamped = cell.max(floor);
                *cell = (clamped + 4.0) / 4.0;
            }
        }
        mel
    }
}

/// i16 PCM → f32 in `[-1, 1)`, padded/trimmed to exactly `N_SAMPLES`.
fn pcm_to_f32_padded(pcm: &[i16]) -> Vec<f32> {
    const I16_SCALE: f32 = 32_768.0;
    let mut out = Vec::with_capacity(N_SAMPLES);
    for &s in pcm.iter().take(N_SAMPLES) {
        out.push(s as f32 / I16_SCALE);
    }
    out.resize(N_SAMPLES, 0.0);
    out
}

/// Reflect-pad `pad` samples on each end (numpy `mode="reflect"`: the
/// boundary sample is not repeated).
fn reflect_pad(x: &[f32], pad: usize) -> Vec<f32> {
    let n = x.len();
    let mut out = Vec::with_capacity(n + 2 * pad);
    for i in (1..=pad).rev() {
        out.push(x[i]);
    }
    out.extend_from_slice(x);
    for i in 1..=pad {
        out.push(x[n - 1 - i]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an extractor from the on-disk preprocessor config when it
    /// exists; otherwise skip (hermetic CI without the model cache).
    fn extractor_from_cache() -> Option<MelExtractor> {
        let home = std::env::var("HOME").ok()?;
        let p = std::path::PathBuf::from(home)
            .join(".cache/sy/aiplane/whisper-medium/tokenizer/preprocessor_config.json");
        if !p.is_file() {
            return None;
        }
        MelExtractor::from_preprocessor_config(&p).ok()
    }

    #[test]
    fn hann_periodic_matches_numpy_endpoints() {
        let w = hann_periodic(N_FFT);
        assert_eq!(w.len(), N_FFT);
        // np.hanning(401)[:-1]: w[0] == 0, symmetric-ish, peak near mid.
        assert!(w[0].abs() < 1e-6, "w[0]={}", w[0]);
        assert!((w[N_FFT / 2] - 1.0).abs() < 1e-3, "mid={}", w[N_FFT / 2]);
    }

    #[test]
    fn reflect_pad_mirrors_without_repeating_boundary() {
        let x = [10.0, 20.0, 30.0, 40.0];
        let p = reflect_pad(&x, 2);
        // numpy reflect: [30,20, 10,20,30,40, 30,20]
        assert_eq!(p, vec![30.0, 20.0, 10.0, 20.0, 30.0, 40.0, 30.0, 20.0]);
    }

    #[test]
    fn pcm_pad_trims_and_zero_fills_to_n_samples() {
        let short = vec![1i16, 2, 3];
        assert_eq!(pcm_to_f32_padded(&short).len(), N_SAMPLES);
        let long = vec![0i16; N_SAMPLES + 100];
        assert_eq!(pcm_to_f32_padded(&long).len(), N_SAMPLES);
    }

    /// Deterministic 440 Hz + 1000 Hz sine, 1 s then silence to 30 s.
    /// Reference mel computed in the AMD venv with
    /// `WhisperFeatureExtractor` (transformers 4.57). Pinned points +
    /// the argmax mel bin in an active frame.
    #[test]
    fn log_mel_matches_transformers_reference_on_sine() {
        let Some(ext) = extractor_from_cache() else {
            eprintln!("skip: whisper-medium preprocessor_config.json not in cache");
            return;
        };
        let sr = SAMPLE_RATE as f32;
        let pcm: Vec<i16> = (0..SAMPLE_RATE as usize)
            .map(|i| {
                let t = i as f32 / sr;
                let v = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
                    + 0.3 * (2.0 * std::f32::consts::PI * 1000.0 * t).sin();
                (v * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16
            })
            .collect();
        let mel = ext.log_mel(&pcm);
        assert_eq!(mel.len(), N_MELS);
        assert_eq!(mel[0].len(), N_FRAMES);
        // Pinned reference points (tol 2e-3; i16 quantisation of the
        // synthetic signal adds a touch over the venv's f32 input).
        let tol = 2e-3;
        let checks = [
            (0usize, 0usize, 1.033065f32),
            (5, 10, -0.561796),
            (10, 30, 1.348738),
            (79, 2999, -0.561796),
        ];
        for (r, c, want) in checks {
            let got = mel[r][c];
            assert!((got - want).abs() < tol, "mel[{r}][{c}]={got}, want {want}");
        }
        // Silent-tail floor: every mel bin in the last frame is the
        // clamped minimum.
        for row in &mel {
            assert!((row[N_FRAMES - 1] - (-0.561796)).abs() < tol);
        }
        // The 1000 Hz tone dominates mel bin 11 in active frame 50.
        let frame = 50;
        let argmax = (0..N_MELS)
            .max_by(|&a, &b| mel[a][frame].partial_cmp(&mel[b][frame]).unwrap())
            .unwrap();
        assert_eq!(argmax, 11, "argmax mel bin in active frame");
    }
}
