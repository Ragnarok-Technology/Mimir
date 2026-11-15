//! Pure-Rust ONNX Silero VAD v5 inference via `tract-onnx`.

use mimir_core::{MimirError, Result};
use std::path::Path;
use tract_onnx::prelude::*;

const SILERO_WINDOW: usize = 512;
const SILERO_CONTEXT: usize = 64;
const SILERO_STATE_LEN: usize = 2 * 128;

type RunnableModel = std::sync::Arc<TypedRunnableModel>;

/// Stateful Silero VAD v5 ONNX runner for 16 kHz mono audio.
pub struct SileroVadOnnx {
    model: RunnableModel,
    state: Vec<f32>,
    context: [f32; SILERO_CONTEXT],
    pending: Vec<f32>,
    threshold: f32,
    last_prob: f32,
}

impl SileroVadOnnx {
    /// Loads a `silero_vad.onnx` model from disk.
    pub fn load(path: &Path, threshold: f32) -> Result<Self> {
        let model = tract_onnx::onnx()
            .model_for_path(path)
            .map_err(|e| MimirError::Audio(format!("failed to load Silero VAD model: {e}")))?
            .into_optimized()
            .map_err(|e| MimirError::Audio(format!("failed to optimize Silero VAD model: {e}")))?
            .into_runnable()
            .map_err(|e| MimirError::Audio(format!("failed to build Silero VAD plan: {e}")))?;

        Ok(Self {
            model,
            state: vec![0.0_f32; SILERO_STATE_LEN],
            context: [0.0_f32; SILERO_CONTEXT],
            pending: Vec::with_capacity(SILERO_WINDOW * 2),
            threshold: threshold.clamp(0.05, 0.95),
            last_prob: 0.0,
        })
    }

    /// Resets the recurrent hidden state between utterances or streams.
    pub fn reset(&mut self) {
        self.state.fill(0.0);
        self.context.fill(0.0);
        self.pending.clear();
        self.last_prob = 0.0;
    }

    /// Feeds arbitrary 16 kHz mono samples and returns true if speech probability >= threshold.
    pub fn is_speech(&mut self, samples: &[f32]) -> Result<bool> {
        self.pending.extend_from_slice(samples);
        let mut max_prob = self.last_prob;
        let mut ran_window = false;

        while self.pending.len() >= SILERO_WINDOW {
            let chunk: Vec<f32> = self.pending.drain(..SILERO_WINDOW).collect();
            let prob = self.infer_window(&chunk)?;
            if !ran_window {
                max_prob = prob;
                ran_window = true;
            } else if prob > max_prob {
                max_prob = prob;
            }
            self.last_prob = prob;
        }

        Ok(max_prob >= self.threshold)
    }

    fn infer_window(&mut self, window: &[f32]) -> Result<f32> {
        let mut input = Vec::with_capacity(SILERO_CONTEXT + SILERO_WINDOW);
        input.extend_from_slice(&self.context);
        input.extend_from_slice(window);
        self.context
            .copy_from_slice(&window[SILERO_WINDOW - SILERO_CONTEXT..]);

        let audio_tensor: Tensor =
            tract_ndarray::Array2::from_shape_vec((1, input.len()), input)
                .map_err(|e| MimirError::Audio(format!("Silero input shape error: {e}")))?
                .into();
        let state_tensor: Tensor =
            tract_ndarray::Array3::from_shape_vec((2, 1, 128), self.state.clone())
                .map_err(|e| MimirError::Audio(format!("Silero state shape error: {e}")))?
                .into();
        let sr_tensor: Tensor = tract_ndarray::arr1(&[16000_i64]).into();

        let outputs = self
            .model
            .run(tvec!(
                audio_tensor.into(),
                state_tensor.into(),
                sr_tensor.into()
            ))
            .map_err(|e| MimirError::Audio(format!("Silero VAD inference failed: {e}")))?;

        let prob = outputs[0]
            .to_plain_array_view::<f32>()
            .map_err(|e| MimirError::Audio(format!("Silero prob output error: {e}")))?
            .iter()
            .next()
            .copied()
            .unwrap_or(0.0);

        if outputs.len() > 1 {
            let next_state = outputs[1]
                .to_plain_array_view::<f32>()
                .map_err(|e| MimirError::Audio(format!("Silero state output error: {e}")))?;
            for (dst, src) in self.state.iter_mut().zip(next_state.iter()) {
                *dst = *src;
            }
        }

        Ok(prob)
    }
}
