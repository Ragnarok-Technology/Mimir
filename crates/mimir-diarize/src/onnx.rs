//! Optional local ONNX speaker-embedding inference (WeSpeaker / CAM++ compatible) via `tract-onnx`.

use mimir_core::{MimirError, Result};
use std::f32::consts::PI;
use std::path::Path;
use tract_onnx::prelude::*;

type RunnableModel = std::sync::Arc<TypedRunnableModel>;

/// Interface for extracting fixed-length L2-normalized speaker embeddings from 16 kHz mono PCM.
pub trait SpeakerEmbedder: Send + Sync {
    fn embed_chunk(&self, samples_16khz: &[f32]) -> Result<Vec<f32>>;
}

/// Pure-Rust ONNX speaker embedder expecting `[1, T, 80]` log-Mel filterbank input.
pub struct OnnxSpeakerEmbedder {
    plan: RunnableModel,
}

impl OnnxSpeakerEmbedder {
    /// Loads a speaker-embedding `.onnx` model (e.g., CAM++ or WeSpeaker ResNet34).
    pub fn load(path: &Path) -> Result<Self> {
        let model = tract_onnx::onnx()
            .model_for_path(path)
            .map_err(|e| MimirError::Whistle(format!("failed to load speaker ONNX model: {e}")))?
            .into_optimized()
            .map_err(|e| MimirError::Whistle(format!("failed to optimize speaker ONNX model: {e}")))?
            .into_runnable()
            .map_err(|e| MimirError::Whistle(format!("failed to compile speaker ONNX model: {e}")))?;
        Ok(Self { plan: model })
    }
}

impl SpeakerEmbedder for OnnxSpeakerEmbedder {
    fn embed_chunk(&self, samples_16khz: &[f32]) -> Result<Vec<f32>> {
        let (frames, fbank) = extract_fbank_80(samples_16khz);
        if frames == 0 {
            return Err(MimirError::Audio("audio chunk too short for speaker embedding".into()));
        }
        let tensor = tract_ndarray::Array3::from_shape_vec((1, frames, 80), fbank)
            .map_err(|e| MimirError::Audio(format!("fbank shape error: {e}")))?
            .into_tensor();
        let outputs = self
            .plan
            .run(tvec!(tensor.into()))
            .map_err(|e| MimirError::Whistle(format!("speaker ONNX inference failed: {e}")))?;
        let view = outputs[0]
            .to_plain_array_view::<f32>()
            .map_err(|e| MimirError::Whistle(format!("speaker ONNX output type error: {e}")))?;
        let mut emb: Vec<f32> = view.iter().copied().collect();
        l2_normalize(&mut emb);
        Ok(emb)
    }
}

fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-8 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

fn hz_to_mel(hz: f32) -> f32 {
    2595.0 * (1.0 + hz / 700.0).log10()
}

fn mel_to_hz(mel: f32) -> f32 {
    700.0 * (10.0f32.powf(mel / 2595.0) - 1.0)
}

/// Extracts `(num_frames, flattened [num_frames * 80])` CMVN-normalized 80-bin log-Mel features.
pub fn extract_fbank_80(samples: &[f32]) -> (usize, Vec<f32>) {
    const WIN_LEN: usize = 400; // 25 ms @ 16 kHz
    const HOP_LEN: usize = 160; // 10 ms @ 16 kHz
    const N_FFT: usize = 512;
    const N_BINS: usize = N_FFT / 2 + 1; // 257
    const N_MELS: usize = 80;
    const SAMPLE_RATE: f32 = 16_000.0;

    if samples.len() < WIN_LEN {
        return (0, Vec::new());
    }

    let hann: Vec<f32> = (0..WIN_LEN)
        .map(|n| 0.5 - 0.5 * (2.0 * PI * n as f32 / WIN_LEN as f32).cos())
        .collect();

    // Precompute Mel filterbank weights
    let mel_min = hz_to_mel(20.0);
    let mel_max = hz_to_mel(7600.0);
    let mel_pts: Vec<f32> = (0..=(N_MELS + 1))
        .map(|i| {
            let m = mel_min + (mel_max - mel_min) * (i as f32) / ((N_MELS + 1) as f32);
            mel_to_hz(m)
        })
        .collect();
    let bin_freqs: Vec<f32> = (0..N_BINS)
        .map(|k| k as f32 * SAMPLE_RATE / N_FFT as f32)
        .collect();

    let mut filters = vec![0.0f32; N_MELS * N_BINS];
    for m in 0..N_MELS {
        let f_left = mel_pts[m];
        let f_center = mel_pts[m + 1];
        let f_right = mel_pts[m + 2];
        for (k, &freq) in bin_freqs.iter().enumerate() {
            let w = if freq >= f_left && freq <= f_center && f_center > f_left {
                (freq - f_left) / (f_center - f_left)
            } else if freq > f_center && freq <= f_right && f_right > f_center {
                (f_right - freq) / (f_right - f_center)
            } else {
                0.0
            };
            filters[m * N_BINS + k] = w;
        }
    }

    let num_frames = (samples.len() - WIN_LEN) / HOP_LEN + 1;
    let mut out = vec![0.0f32; num_frames * N_MELS];
    let mut power = vec![0.0f32; N_BINS];

    for t in 0..num_frames {
        let start = t * HOP_LEN;
        let frame = &samples[start..start + WIN_LEN];
        for (k, p_out) in power.iter_mut().enumerate() {
            let mut re = 0.0f32;
            let mut im = 0.0f32;
            let angle_step = -2.0 * PI * k as f32 / N_FFT as f32;
            for (n, &s) in frame.iter().enumerate() {
                let x = s * hann[n];
                let ang = angle_step * n as f32;
                re += x * ang.cos();
                im += x * ang.sin();
            }
            *p_out = re * re + im * im;
        }

        for m in 0..N_MELS {
            let row = &filters[m * N_BINS..(m + 1) * N_BINS];
            let mut energy = 0.0f32;
            for (k, &w) in row.iter().enumerate() {
                energy += w * power[k];
            }
            out[t * N_MELS + m] = energy.max(1e-10).ln();
        }
    }

    // Per-bin Cepstral Mean Normalization (CMN)
    if num_frames > 1 {
        for m in 0..N_MELS {
            let mean = (0..num_frames).map(|t| out[t * N_MELS + m]).sum::<f32>() / num_frames as f32;
            for t in 0..num_frames {
                out[t * N_MELS + m] -= mean;
            }
        }
    }

    (num_frames, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fbank_80_produces_expected_shape_and_finite_values() {
        let pcm: Vec<f32> = (0..1600)
            .map(|i| (2.0 * PI * 440.0 * i as f32 / 16_000.0).sin() * 0.25)
            .collect();
        let (frames, feats) = extract_fbank_80(&pcm);
        assert_eq!(frames, (1600 - 400) / 160 + 1);
        assert_eq!(feats.len(), frames * 80);
        assert!(feats.iter().all(|v| v.is_finite()));
    }
}
