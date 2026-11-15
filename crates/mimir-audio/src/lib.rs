//! Cross-platform dual-channel audio capture, 16 kHz resampling, bleed gate, and VAD.

pub mod bleed;
pub mod capture;
pub mod resampler;
pub mod vad;

pub use bleed::BleedGate;
pub use capture::{CaptureHandle, CapturedFrame, list_audio_devices, start_dual_capture};
pub use mimir_core::{AudioChunk, ChannelId, WHISTLE_SAMPLE_RATE};
pub use resampler::AudioNormalizer;
pub use vad::VadChunker;
