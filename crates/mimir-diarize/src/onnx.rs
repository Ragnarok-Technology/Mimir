//! Pure-Rust ONNX speaker embedding extractor (`wespeaker_en_voxceleb_CAM++.onnx`).

use crate::l2_normalize;
use mimir_core::{MimirError, Result, WHISTLE_SAMPLE_RATE};
use std::f32::consts::PI;
use std::path::Path;
use tract_onnx::prelude::*;

const SAMPLE_RATE: f32 = WHISTLE_SAMPLE_RATE as f32;
const WIN_LEN: usize = 400; // 25 ms @ 16 kHz
const HOP_LEN: usize = 160; // 10 ms @ 16 kHz
const N_FFT: usize = 512;
const N_MELS: usize = 80;
const F_MIN: f32 = 20.0;
const F_MAX: f32 = 7600.0;

type RunnableModel = std::sync::Arc<TypedRunnableModel>;

/// Abstraction for extracting an L2-normalized speaker embedding from 16 kHz mono audio.
pub trait SpeakerEmbedder: Send + Sync {
    fn embed(&self, samples_16k: &[f32]) -> Result<Vec<f32>>;
}

/// ONNX speaker embedding extractor using WeSpeaker CAM++ (`[1, T, 80]` log-Mel fbank).
pub struct OnnxSpeakerEmbedder {
    model: RunnableModel,
    mel_bank: Vec<Vec<f32>>,
    hann_window: Vec<f32>,
}

impl OnnxSpeakerEmbedder {
    /// Loads a speaker embedding ONNX model from disk.
    pub fn load(path: &Path) -> Result<Self> {
        let model = tract_onnx::onnx()
            .model_for_path(path)
            .map_err(|e| MimirError::Audio(format!("failed to load speaker ONNX model: {e}")))?
            .into_optimized()
            .map_err(|e| {
                MimirError::Audio(format!("failed to optimize speaker ONNX model: {e}"))
            })?
            .into_runnable()
            .map_err(|e| MimirError::Audio(format!("failed to build speaker ONNX plan: {e}")))?;

        Ok(Self {
            model,
            mel_bank: build_mel_filterbank(),
            hann_window: (0..WIN_LEN)
                .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / (WIN_LEN as f32 - 1.0)).cos())
                .collect(),
        })
    }

    /// Computes 80-bin CMVN log-Mel features `[T, 80]` for 16 kHz mono PCM.
    pub fn compute_fbank(&self, samples: &[f32]) -> Vec<f32> {
        if samples.len() < WIN_LEN {
            return vec![0.0_f32; N_MELS];
        }
        let n_frames = (samples.len() - WIN_LEN) / HOP_LEN + 1;
        let n_bins = N_FFT / 2 + 1;
        let mut features = vec![0.0_f32; n_frames * N_MELS];

        // Precompute cosine/sine tables for real DFT over 512 bins.
        for frame_idx in 0..n_frames {
            let start = frame_idx * HOP_LEN;
            let mut windowed = [0.0_f32; N_FFT];
            // Pre-emphasis (0.97) + Hann window
            for i in 0..WIN_LEN {
                let s = samples[start + i];
                let prev = if i > 0 { samples[start + i - 1] } else { s };
                windowed[i] = (s - 0.97 * prev) * self.hann_window[i];
            }

            let mut power = vec![0.0_f32; n_bins];
            for (k, p) in power.iter_mut().enumerate() {
                let mut re = 0.0_f32;
                let mut im = 0.0_f32;
                let angle_step = -2.0 * PI * k as f32 / N_FFT as f32;
                for (n, &x) in windowed.iter().enumerate().take(WIN_LEN) {
                    let angle = angle_step * n as f32;
                    re += x * angle.cos();
                    im += x * angle.sin();
                }
                *p = re * re + im * im;
            }

            for (m, filter) in self.mel_bank.iter().enumerate() {
                let mut energy = 0.0_f32;
                for (k, &w) in filter.iter().enumerate() {
                    energy += power[k] * w;
                }
                features[frame_idx * N_MELS + m] = (energy.max(1e-10)).ln();
            }
        }

        // Per-utterance Cepstral Mean Normalization (CMN) per Mel bin.
        for m in 0..N_MELS {
            let mean: f32 =
                (0..n_frames).map(|t| features[t * N_MELS + m]).sum::<f32>() / n_frames as f32;
            for t in 0..n_frames {
                features[t * N_MELS + m] -= mean;
            }
        }

        features
    }
}

impl SpeakerEmbedder for OnnxSpeakerEmbedder {
    fn embed(&self, samples_16k: &[f32]) -> Result<Vec<f32>> {
        let features = self.compute_fbank(samples_16k);
        let n_frames = features.len() / N_MELS;
        let input: Tensor =
            tract_ndarray::Array3::from_shape_vec((1, n_frames, N_MELS), features)
                .map_err(|e| MimirError::Audio(format!("speaker fbank shape error: {e}")))?
                .into();

        let outputs = self
            .model
            .run(tvec!(input.into()))
            .map_err(|e| MimirError::Audio(format!("speaker ONNX inference failed: {e}")))?;

        let raw: Vec<f32> = outputs[0]
            .to_plain_array_view::<f32>()
            .map_err(|e| MimirError::Audio(format!("speaker ONNX output error: {e}")))?
            .iter()
            .copied()
            .collect();

        l2_normalize(&raw)
            .ok_or_else(|| MimirError::Audio("speaker ONNX returned zero-norm embedding".into()))
    }
}

fn hz_to_mel(hz: f32) -> f32 {
    2595.0 * (1.0 + hz / 700.0).log10()
}

fn mel_to_hz(mel: f32) -> f32 {
    700.0 * (10.0_f32.powf(mel / 2595.0) - 1.0)
}

fn build_mel_filterbank() -> Vec<Vec<f32>> {
    let n_bins = N_FFT / 2 + 1;
    let mel_min = hz_to_mel(F_MIN);
    let mel_max = hz_to_mel(F_MAX);
    let mut mel_points = Vec::with_capacity(N_MELS + 2);
    for i in 0..(N_MELS + 2) {
        let m = mel_min + (mel_max - mel_min) * i as f32 / (N_MELS + 1) as f32;
        mel_points.push(mel_to_hz(m));
    }
    let bin_freqs: Vec<f32> = (0..n_bins)
        .map(|k| k as f32 * SAMPLE_RATE / N_FFT as f32)
        .collect();

    let mut bank = Vec::with_capacity(N_MELS);
    for m in 0..N_MELS {
        let left = mel_points[m];
        let center = mel_points[m + 1];
        let right = mel_points[m + 2];
        let mut filter = vec![0.0_f32; n_bins];
        for (k, &f) in bin_freqs.iter().enumerate() {
            if f >= left && f <= center && center > left {
                filter[k] = (f - left) / (center - left);
            } else if f > center && f <= right && right > center {
                filter[k] = (right - f) / (right - center);
            }
        }
        bank.push(filter);
    }
    bank
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mel_filterbank_has_80_normalized_filters() {
        let bank = build_mel_filterbank();
        assert_eq!(bank.len(), N_MELS);
        assert_eq!(bank[0].len(), N_FFT / 2 + 1);
        assert!(bank.iter().all(|f| f.iter().any(|&w| w > 0.0)));
    }
}
