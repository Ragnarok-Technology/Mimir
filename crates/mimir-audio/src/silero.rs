//! Optional local Silero VAD ONNX inference via `tract-onnx`.

use mimir_core::{MimirError, Result};
use std::path::Path;
use tract_onnx::prelude::*;

type RunnableModel = std::sync::Arc<TypedRunnableModel>;

/// Stateful Silero VAD v5 ONNX runner operating on 512-sample (32 ms) 16 kHz frames.
pub struct SileroVadOnnx {
    plan: RunnableModel,
    state: Vec<f32>, // [2, 1, 128]
}

impl SileroVadOnnx {
    /// Loads a `silero_vad.onnx` model from disk.
    pub fn load(path: &Path) -> Result<Self> {
        let plan = tract_onnx::onnx()
            .model_for_path(path)
            .map_err(|e| MimirError::Audio(format!("failed to load Silero VAD ONNX: {e}")))?
            .into_optimized()
            .map_err(|e| MimirError::Audio(format!("failed to optimize Silero VAD ONNX: {e}")))?
            .into_runnable()
            .map_err(|e| MimirError::Audio(format!("failed to compile Silero VAD ONNX: {e}")))?;
        Ok(Self {
            plan,
            state: vec![0.0f32; 2 * 128],
        })
    }

    /// Resets recurrent hidden state (e.g., on stream reset).
    pub fn reset(&mut self) {
        self.state.fill(0.0);
    }

    /// Scores a 16 kHz mono frame (padded/truncated to 512 samples) and returns speech probability `[0, 1]`.
    pub fn speech_probability(&mut self, samples_16khz: &[f32]) -> Result<f32> {
        let mut frame = vec![0.0f32; 512];
        let copy_len = samples_16khz.len().min(512);
        frame[..copy_len].copy_from_slice(&samples_16khz[..copy_len]);

        let input = tract_ndarray::Array2::from_shape_vec((1, 512), frame)
            .map_err(|e| MimirError::Audio(e.to_string()))?
            .into_tensor();
        let state = tract_ndarray::Array3::from_shape_vec((2, 1, 128), self.state.clone())
            .map_err(|e| MimirError::Audio(e.to_string()))?
            .into_tensor();
        let sr = tract_ndarray::Array1::from_vec(vec![16_000i64]).into_tensor();

        let outputs = self
            .plan
            .run(tvec!(input.into(), state.into(), sr.into()))
            .map_err(|e| MimirError::Audio(format!("Silero VAD inference error: {e}")))?;

        let prob = outputs[0]
            .to_plain_array_view::<f32>()
            .map_err(|e| MimirError::Audio(e.to_string()))?
            .iter()
            .next()
            .copied()
            .unwrap_or(0.0);

        if outputs.len() > 1
            && let Ok(next_state) = outputs[1].to_plain_array_view::<f32>()
            && next_state.len() == self.state.len()
            && let Some(slice) = next_state.as_slice()
        {
            self.state.copy_from_slice(slice);
        }
        Ok(prob.clamp(0.0, 1.0))
    }
}
