use std::path::Path;

use candle_core::quantized::gguf_file;
use candle_core::{Device, Tensor};
use candle_transformers::generation::LogitsProcessor;
use candle_transformers::models::quantized_qwen2::ModelWeights;
use mimir_core::{MimirError, Result};
use tokenizers::Tokenizer;

/// In-process local Qwen 2.5 0.5B Instruct runner backed by pure-Rust Candle.
pub struct QwenCleanupModel {
    weights: ModelWeights,
    tokenizer: Tokenizer,
    eos_token_id: u32,
    im_end_token_id: Option<u32>,
}

impl QwenCleanupModel {
    /// Loads quantized Qwen2 GGUF weights and tokenizer from disk.
    pub fn load(weights_path: &Path, tokenizer_path: &Path) -> Result<Self> {
        let mut file = std::fs::File::open(weights_path).map_err(|e| {
            MimirError::Whistle(format!(
                "failed to open Qwen weights at {}: {e}",
                weights_path.display()
            ))
        })?;
        let content = gguf_file::Content::read(&mut file)
            .map_err(|e| MimirError::Whistle(format!("failed to parse Qwen GGUF: {e}")))?;
        let weights = ModelWeights::from_gguf(content, &mut file, &Device::Cpu)
            .map_err(|e| MimirError::Whistle(format!("failed to load Qwen weights: {e}")))?;

        let tokenizer = Tokenizer::from_file(tokenizer_path).map_err(|e| {
            MimirError::Whistle(format!(
                "failed to load Qwen tokenizer at {}: {e}",
                tokenizer_path.display()
            ))
        })?;
        let eos_token_id = tokenizer.token_to_id("<|endoftext|>").unwrap_or(151643);
        let im_end_token_id = tokenizer.token_to_id("<|im_end|>");

        Ok(Self {
            weights,
            tokenizer,
            eos_token_id,
            im_end_token_id,
        })
    }

    /// Runs deterministic greedy cleanup generation for a single chat prompt.
    pub fn complete(&mut self, system: &str, user: &str, max_new_tokens: usize) -> Result<String> {
        let prompt = format!(
            "<|im_start|>system\n{system}<|im_end|>\n<|im_start|>user\n{user}<|im_end|>\n<|im_start|>assistant\n"
        );
        let encoding = self
            .tokenizer
            .encode(prompt, true)
            .map_err(|e| MimirError::Whistle(format!("Qwen tokenization failed: {e}")))?;
        let prompt_tokens = encoding.get_ids();
        if prompt_tokens.is_empty() {
            return Ok(String::new());
        }

        let input = Tensor::new(prompt_tokens, &Device::Cpu)
            .and_then(|t| t.unsqueeze(0))
            .map_err(|e| MimirError::Whistle(format!("Qwen tensor error: {e}")))?;

        let mut logits_processor = LogitsProcessor::new(42, None, None);
        let logits = self
            .weights
            .forward(&input, 0)
            .map_err(|e| MimirError::Whistle(format!("Qwen forward failed: {e}")))?;
        let logits = logits
            .squeeze(0)
            .map_err(|e| MimirError::Whistle(format!("Qwen squeeze failed: {e}")))?;
        let mut next_token = logits_processor
            .sample(&logits)
            .map_err(|e| MimirError::Whistle(format!("Qwen sample failed: {e}")))?;

        let mut generated = Vec::new();
        let mut pos = prompt_tokens.len();

        for _ in 0..max_new_tokens {
            if next_token == self.eos_token_id || Some(next_token) == self.im_end_token_id {
                break;
            }
            generated.push(next_token);

            let step_input = Tensor::new(&[next_token], &Device::Cpu)
                .and_then(|t| t.unsqueeze(0))
                .map_err(|e| MimirError::Whistle(format!("Qwen step tensor failed: {e}")))?;
            let logits = self
                .weights
                .forward(&step_input, pos)
                .map_err(|e| MimirError::Whistle(format!("Qwen step forward failed: {e}")))?;
            let logits = logits
                .squeeze(0)
                .map_err(|e| MimirError::Whistle(format!("Qwen step squeeze failed: {e}")))?;
            next_token = logits_processor
                .sample(&logits)
                .map_err(|e| MimirError::Whistle(format!("Qwen step sample failed: {e}")))?;
            pos += 1;
        }

        let decoded = self
            .tokenizer
            .decode(&generated, true)
            .map_err(|e| MimirError::Whistle(format!("Qwen decode failed: {e}")))?;
        Ok(decoded.trim().to_string())
    }
}
