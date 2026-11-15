//! Per-channel Voice Activity Detection (VAD) and <= 28s chunking for Whistle.

use crate::bleed::frame_rms;
use crate::silero::SileroVadOnnx;
use mimir_core::{AudioChunk, ChannelId, WHISTLE_SAMPLE_RATE};
use std::collections::VecDeque;

const FRAME_SAMPLES: usize = 320; // 20 ms at 16 kHz
const PRE_ROLL_FRAMES: usize = 10; // 200 ms pre-roll
const HANGOVER_FRAMES: usize = 25; // 500 ms silence pause ends utterance
const MIN_SPEECH_FRAMES: usize = 10; // 200 ms minimum speech to emit
const MAX_CHUNK_SAMPLES: usize = WHISTLE_SAMPLE_RATE as usize * 28; // < 30s Whistle limit
const OVERLAP_SAMPLES: usize = WHISTLE_SAMPLE_RATE as usize / 2; // 0.5s overlap on hard cap

/// Segments a continuous 16 kHz mono stream into voiced `AudioChunk`s ready for Whistle.
pub struct VadChunker {
    channel: ChannelId,
    energy_threshold: f32,
    silero: Option<SileroVadOnnx>,
    sample_cursor: u64,
    pre_roll: VecDeque<f32>,
    active_buf: Vec<f32>,
    active_start_sample: u64,
    in_speech: bool,
    silence_frames: usize,
    voiced_frames: usize,
    pending_frame: Vec<f32>,
}

impl VadChunker {
    pub fn new(channel: ChannelId, energy_threshold: f32) -> Self {
        Self {
            channel,
            energy_threshold,
            silero: None,
            sample_cursor: 0,
            pre_roll: VecDeque::with_capacity(PRE_ROLL_FRAMES * FRAME_SAMPLES),
            active_buf: Vec::new(),
            active_start_sample: 0,
            in_speech: false,
            silence_frames: 0,
            voiced_frames: 0,
            pending_frame: Vec::with_capacity(FRAME_SAMPLES),
        }
    }

    /// Creates a VAD chunker backed by Silero VAD v5 ONNX, falling back to RMS threshold on error.
    pub fn with_silero(
        channel: ChannelId,
        energy_threshold: f32,
        silero: SileroVadOnnx,
    ) -> Self {
        let mut chunker = Self::new(channel, energy_threshold);
        chunker.silero = Some(silero);
        chunker
    }

    /// Feeds 16 kHz mono samples and returns any completed speech chunks.
    pub fn push(&mut self, samples: &[f32]) -> Vec<AudioChunk> {
        let mut emitted = Vec::new();
        self.pending_frame.extend_from_slice(samples);

        while self.pending_frame.len() >= FRAME_SAMPLES {
            let frame: Vec<f32> = self.pending_frame.drain(..FRAME_SAMPLES).collect();
            let rms_voiced = frame_rms(&frame) >= self.energy_threshold;
            let is_voiced = match self.silero.as_mut() {
                Some(vad) => vad.is_speech(&frame).unwrap_or(rms_voiced),
                None => rms_voiced,
            };

            if !self.in_speech {
                if is_voiced {
                    self.in_speech = true;
                    self.silence_frames = 0;
                    self.voiced_frames = 1;
                    let pre_len = self.pre_roll.len() as u64;
                    self.active_start_sample = self.sample_cursor.saturating_sub(pre_len);
                    self.active_buf.extend(self.pre_roll.drain(..));
                    self.active_buf.extend_from_slice(&frame);
                } else {
                    self.push_pre_roll(&frame);
                }
            } else {
                self.active_buf.extend_from_slice(&frame);
                if is_voiced {
                    self.voiced_frames += 1;
                    self.silence_frames = 0;
                } else {
                    self.silence_frames += 1;
                }

                if self.active_buf.len() >= MAX_CHUNK_SAMPLES {
                    if let Some(chunk) = self.emit_hard_split() {
                        emitted.push(chunk);
                    }
                } else if self.silence_frames >= HANGOVER_FRAMES {
                    if let Some(chunk) = self.flush_active() {
                        emitted.push(chunk);
                    }
                }
            }

            self.sample_cursor += FRAME_SAMPLES as u64;
        }

        emitted
    }

    /// Flushes any remaining voiced audio when the session stops.
    pub fn flush(&mut self) -> Option<AudioChunk> {
        if !self.pending_frame.is_empty() && self.in_speech {
            self.active_buf.extend(self.pending_frame.drain(..));
        } else {
            self.pending_frame.clear();
        }
        self.flush_active()
    }

    fn push_pre_roll(&mut self, frame: &[f32]) {
        let max = PRE_ROLL_FRAMES * FRAME_SAMPLES;
        for &s in frame {
            if self.pre_roll.len() >= max {
                self.pre_roll.pop_front();
            }
            self.pre_roll.push_back(s);
        }
    }

    fn flush_active(&mut self) -> Option<AudioChunk> {
        self.in_speech = false;
        self.silence_frames = 0;
        let voiced = std::mem::replace(&mut self.voiced_frames, 0);
        let samples = std::mem::take(&mut self.active_buf);
        if voiced < MIN_SPEECH_FRAMES || samples.is_empty() {
            return None;
        }
        Some(AudioChunk {
            channel: self.channel,
            start_ms: self.active_start_sample * 1000 / WHISTLE_SAMPLE_RATE as u64,
            sample_rate: WHISTLE_SAMPLE_RATE,
            samples,
        })
    }

    fn emit_hard_split(&mut self) -> Option<AudioChunk> {
        let tail_start = self.active_buf.len().saturating_sub(OVERLAP_SAMPLES);
        let overlap = self.active_buf[tail_start..].to_vec();
        let chunk = AudioChunk {
            channel: self.channel,
            start_ms: self.active_start_sample * 1000 / WHISTLE_SAMPLE_RATE as u64,
            sample_rate: WHISTLE_SAMPLE_RATE,
            samples: std::mem::take(&mut self.active_buf),
        };
        self.active_start_sample += (chunk.samples.len() - overlap.len()) as u64;
        self.active_buf = overlap;
        self.silence_frames = 0;
        Some(chunk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_speech_after_silence_hangover() {
        let mut vad = VadChunker::new(ChannelId::Mic, 0.02);

        // 0.4s silence, 1.0s speech, 0.6s silence
        let mut stream = vec![0.0f32; 6_400];
        stream.extend(vec![0.25f32; 16_000]);
        stream.extend(vec![0.0f32; 9_600]);

        let chunks = vad.push(&stream);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].channel, ChannelId::Mic);
        assert!(chunks[0].samples.len() >= 16_000);
        // Pre-roll starts ~200 ms before speech at 400 ms -> ~200 ms
        assert!(chunks[0].start_ms <= 250);
    }

    #[test]
    fn hard_caps_continuous_speech_under_30_seconds() {
        let mut vad = VadChunker::new(ChannelId::System, 0.02);
        let long_speech = vec![0.2f32; WHISTLE_SAMPLE_RATE as usize * 31];
        let chunks = vad.push(&long_speech);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].samples.len() <= WHISTLE_SAMPLE_RATE as usize * 29);
        assert!(vad.flush().is_some());
    }
}
