//! Cross-channel acoustic bleed suppression for speaker-to-microphone leakage.

use std::collections::VecDeque;

/// Suppresses `Mic` frames that are likely acoustic bleed from recent `System` output.
pub struct BleedGate {
    /// Recent system frame RMS values (e.g. 20 ms frames over a 160 ms window).
    recent_sys_rms: VecDeque<f32>,
    window_frames: usize,
    /// Minimum system RMS to consider system audio active.
    sys_active_threshold: f32,
    /// If `mic_rms < sys_rms * bleed_ratio`, treat mic frame as bleed and attenuate.
    bleed_ratio: f32,
}

impl BleedGate {
    pub fn new(window_frames: usize, sys_active_threshold: f32, bleed_ratio: f32) -> Self {
        Self {
            recent_sys_rms: VecDeque::with_capacity(window_frames),
            window_frames: window_frames.max(1),
            sys_active_threshold,
            bleed_ratio,
        }
    }

    /// Default configuration tuned for 20 ms frames over a 160 ms acoustic delay window.
    pub fn default_20ms() -> Self {
        Self::new(8, 0.015, 0.85)
    }

    /// Records a `System` frame's energy into the acoustic delay window.
    pub fn observe_system(&mut self, samples: &[f32]) {
        let rms = frame_rms(samples);
        if self.recent_sys_rms.len() >= self.window_frames {
            self.recent_sys_rms.pop_front();
        }
        self.recent_sys_rms.push_back(rms);
    }

    /// Evaluates a `Mic` frame; returns `true` if the frame was gated (suppressed as bleed).
    pub fn filter_mic(&self, samples: &mut [f32]) -> bool {
        let max_sys = self
            .recent_sys_rms
            .iter()
            .copied()
            .fold(0.0f32, f32::max);
        if max_sys < self.sys_active_threshold {
            return false;
        }

        let mic_rms = frame_rms(samples);
        if mic_rms < max_sys * self.bleed_ratio {
            samples.fill(0.0);
            true
        } else {
            false
        }
    }
}

pub fn frame_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gates_quiet_mic_bleed_when_system_is_loud() {
        let mut gate = BleedGate::default_20ms();
        gate.observe_system(&vec![0.4; 320]);

        let mut mic_bleed = vec![0.1; 320];
        assert!(gate.filter_mic(&mut mic_bleed));
        assert!(mic_bleed.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn passes_loud_host_speech_even_when_system_is_active() {
        let mut gate = BleedGate::default_20ms();
        gate.observe_system(&vec![0.15; 320]);

        let mut host_speech = vec![0.45; 320];
        assert!(!gate.filter_mic(&mut host_speech));
        assert!((host_speech[0] - 0.45).abs() < 1e-6);
    }
}
