//! Multi-channel, arbitrary-sample-rate audio normalizer targeting 16 kHz mono f32.

use mimir_core::{MimirError, Result, WHISTLE_SAMPLE_RATE};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

/// Downmixes interleaved N-channel PCM and resamples to 16 kHz mono `f32`.
pub struct AudioNormalizer {
    channels: usize,
    input_rate: u32,
    resampler: Option<SincFixedIn<f32>>,
    pending_mono: Vec<f32>,
}

impl AudioNormalizer {
    pub fn new(input_rate: u32, channels: u16) -> Result<Self> {
        let channels = channels.max(1) as usize;
        let resampler = if input_rate == WHISTLE_SAMPLE_RATE {
            None
        } else {
            let params = SincInterpolationParameters {
                sinc_len: 64,
                f_cutoff: 0.95,
                interpolation: SincInterpolationType::Linear,
                oversampling_factor: 128,
                window: WindowFunction::BlackmanHarris2,
            };
            let ratio = WHISTLE_SAMPLE_RATE as f64 / input_rate as f64;
            let chunk_size = (input_rate as usize / 50).max(160); // ~20 ms blocks
            Some(
                SincFixedIn::<f32>::new(ratio, 2.0, params, chunk_size, 1)
                    .map_err(|e| MimirError::Audio(format!("failed to create resampler: {e}")))?,
            )
        };

        Ok(Self {
            channels,
            input_rate,
            resampler,
            pending_mono: Vec::new(),
        })
    }

    pub fn input_rate(&self) -> u32 {
        self.input_rate
    }

    /// Pushes interleaved `f32` samples and returns 16 kHz mono `f32` samples.
    pub fn process(&mut self, interleaved: &[f32]) -> Result<Vec<f32>> {
        if interleaved.is_empty() {
            return Ok(Vec::new());
        }

        // Downmix to mono
        if self.channels == 1 {
            self.pending_mono.extend_from_slice(interleaved);
        } else {
            let inv = 1.0 / self.channels as f32;
            for frame in interleaved.chunks_exact(self.channels) {
                let sum: f32 = frame.iter().copied().sum();
                self.pending_mono.push(sum * inv);
            }
        }

        let Some(resampler) = &mut self.resampler else {
            return Ok(std::mem::take(&mut self.pending_mono));
        };

        let needed = resampler.input_frames_next();
        let mut out = Vec::new();
        while self.pending_mono.len() >= needed {
            let block: Vec<f32> = self.pending_mono.drain(..needed).collect();
            let resampled = resampler
                .process(&[block], None)
                .map_err(|e| MimirError::Audio(format!("resample failed: {e}")))?;
            if let Some(ch0) = resampled.first() {
                out.extend_from_slice(ch0);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmixes_stereo_16khz_without_resampling() {
        let mut norm = AudioNormalizer::new(16_000, 2).unwrap();
        let out = norm.process(&[1.0, 0.0, 0.5, 0.5, -1.0, 1.0]).unwrap();
        assert_eq!(out, vec![0.5, 0.5, 0.0]);
    }

    #[test]
    fn resamples_48khz_to_16khz_preserving_duration_ratio() {
        let mut norm = AudioNormalizer::new(48_000, 1).unwrap();
        // 1 second of 48 kHz sine wave at 440 Hz
        let input: Vec<f32> = (0..48_000)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin())
            .collect();
        let out = norm.process(&input).unwrap();
        // Should be close to 16_000 samples (minus small sinc filter block remainder)
        assert!(out.len() > 14_500 && out.len() <= 16_000, "got {}", out.len());
    }
}
