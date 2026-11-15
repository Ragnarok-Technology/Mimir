//! Cross-platform dual-channel audio capture via `cpal` (mic + loopback/monitor discovery).

use crate::resampler::AudioNormalizer;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use mimir_core::{ChannelId, MimirError, Result};
use std::sync::mpsc;
use tracing::{info, warn};

/// Raw normalized 16 kHz mono frame emitted by an audio capture stream.
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub channel: ChannelId,
    pub samples: Vec<f32>,
}

/// Active OS audio capture session holding the underlying `cpal` streams.
pub struct CaptureHandle {
    _streams: Vec<Stream>,
}

/// Lists available input and loopback/monitor device names for diagnostics.
pub fn list_audio_devices() -> Result<Vec<String>> {
    let host = cpal::default_host();
    let mut names = Vec::new();
    if let Ok(inputs) = host.input_devices() {
        for dev in inputs {
            if let Ok(name) = dev.name() {
                names.push(format!("input: {name}"));
            }
        }
    }
    if let Ok(outputs) = host.output_devices() {
        for dev in outputs {
            if let Ok(name) = dev.name() {
                names.push(format!("output: {name}"));
            }
        }
    }
    Ok(names)
}

/// Starts dual-channel capture (default mic + best-effort system loopback) forwarding 16 kHz mono frames.
pub fn start_dual_capture(tx: mpsc::Sender<CapturedFrame>) -> Result<CaptureHandle> {
    let host = cpal::default_host();
    let mut streams = Vec::new();

    if let Some(mic_dev) = host.default_input_device() {
        let name = mic_dev.name().unwrap_or_else(|_| "default-mic".into());
        info!("opening mic capture device: {name}");
        streams.push(build_input_stream(&mic_dev, ChannelId::Mic, tx.clone())?);
    } else {
        warn!("no default microphone device found");
    }

    if let Some(sys_stream) = try_open_system_loopback(&host, tx)? {
        streams.push(sys_stream);
    } else {
        warn!(
            "no system loopback device detected; capturing microphone only (on macOS install BlackHole/loopback or use CoreAudio tap; on Windows WASAPI loopback is automatic; on Linux use a .monitor source)"
        );
    }

    if streams.is_empty() {
        return Err(MimirError::Audio(
            "no audio capture devices could be opened".into(),
        ));
    }

    Ok(CaptureHandle { _streams: streams })
}

fn try_open_system_loopback(
    host: &cpal::Host,
    tx: mpsc::Sender<CapturedFrame>,
) -> Result<Option<Stream>> {
    // 1. On Linux/macOS, look for an explicit monitor or loopback input device.
    if let Ok(inputs) = host.input_devices() {
        for dev in inputs {
            let name = dev.name().unwrap_or_default();
            let lower = name.to_ascii_lowercase();
            if lower.contains("monitor")
                || lower.contains("blackhole")
                || lower.contains("loopback")
                || lower.contains("soundflower")
                || lower.contains("stereo mix")
            {
                info!("opening system loopback input device: {name}");
                return Ok(Some(build_input_stream(&dev, ChannelId::System, tx)?));
            }
        }
    }

    // 2. On Windows (WASAPI), output devices can be opened directly as loopback input streams.
    #[cfg(target_os = "windows")]
    if let Some(out_dev) = host.default_output_device() {
        let name = out_dev.name().unwrap_or_else(|_| "default-loopback".into());
        info!("opening WASAPI loopback on output device: {name}");
        return Ok(Some(build_input_stream(&out_dev, ChannelId::System, tx)?));
    }

    Ok(None)
}

fn build_input_stream(
    device: &cpal::Device,
    channel: ChannelId,
    tx: mpsc::Sender<CapturedFrame>,
) -> Result<Stream> {
    let supported = device
        .default_input_config()
        .map_err(|e| MimirError::Audio(format!("failed to get input config: {e}")))?;

    let sample_format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let mut normalizer = AudioNormalizer::new(config.sample_rate.0, config.channels)?;

    let err_fn = move |err| {
        warn!("audio stream error on {:?}: {err}", channel);
    };

    let stream = match sample_format {
        SampleFormat::F32 => device.build_input_stream(
            &config,
            move |data: &[f32], _| {
                if let Ok(mono) = normalizer.process(data)
                    && !mono.is_empty()
                {
                    let _ = tx.send(CapturedFrame {
                        channel,
                        samples: mono,
                    });
                }
            },
            err_fn,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream(
            &config,
            move |data: &[i16], _| {
                let floats: Vec<f32> = data.iter().map(|&s| s as f32 / 32768.0).collect();
                if let Ok(mono) = normalizer.process(&floats)
                    && !mono.is_empty()
                {
                    let _ = tx.send(CapturedFrame {
                        channel,
                        samples: mono,
                    });
                }
            },
            err_fn,
            None,
        ),
        SampleFormat::U16 => device.build_input_stream(
            &config,
            move |data: &[u16], _| {
                let floats: Vec<f32> = data
                    .iter()
                    .map(|&s| (s as f32 - 32768.0) / 32768.0)
                    .collect();
                if let Ok(mono) = normalizer.process(&floats)
                    && !mono.is_empty()
                {
                    let _ = tx.send(CapturedFrame {
                        channel,
                        samples: mono,
                    });
                }
            },
            err_fn,
            None,
        ),
        other => {
            return Err(MimirError::Audio(format!(
                "unsupported sample format: {other:?}"
            )));
        }
    }
    .map_err(|e| MimirError::Audio(format!("failed to build input stream: {e}")))?;

    stream
        .play()
        .map_err(|e| MimirError::Audio(format!("failed to start audio stream: {e}")))?;

    Ok(stream)
}
