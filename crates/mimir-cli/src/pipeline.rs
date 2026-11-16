//! Live and testable dual-channel audio -> Whistle -> Diarize -> Store pipeline.

use chrono::Utc;
use mimir_audio::{BleedGate, CapturedFrame, SileroVadOnnx, VadChunker};
use mimir_core::{AudioChunk, ChannelId, Result, Session, Utterance};
use mimir_diarize::{
    Diarizer, DiarizerConfig, OnnxSpeakerEmbedder, SpeakerEmbedder, insert_utterance_chronological,
    mean_pool_frames,
};
use mimir_store::{SessionStore, sync_to_drop_dir};
use mimir_whistle::{TranscribeOptions, TranscribeResult, WhistleEngine};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use tokio::sync::broadcast;
use uuid::Uuid;

/// Optional local accuracy enhancers (ONNX Silero VAD, ONNX Speaker Embeddings).
#[derive(Default)]
pub struct PipelineExtras {
    pub silero_vad_path: Option<PathBuf>,
    pub speaker_embedder: Option<Arc<OnnxSpeakerEmbedder>>,
}

/// Abstraction over Whistle inference so the full pipeline can be integration-tested deterministically.
pub trait AsrBackend {
    fn transcribe_with_embedding(
        &self,
        chunk: &AudioChunk,
        opts: &TranscribeOptions,
    ) -> Result<(TranscribeResult, Option<Vec<f32>>)>;
}

impl AsrBackend for WhistleEngine {
    fn transcribe_with_embedding(
        &self,
        chunk: &AudioChunk,
        opts: &TranscribeOptions,
    ) -> Result<(TranscribeResult, Option<Vec<f32>>)> {
        let res = self.transcribe(&chunk.samples, opts)?;
        let emb = if !res.text.trim().is_empty() {
            // Best-effort embedding extraction for speaker diarization.
            self.embed(&chunk.samples).ok().filter(|v| !v.is_empty())
        } else {
            None
        };
        Ok((res, emb))
    }
}

/// Processes a stream of `CapturedFrame`s until the channel closes, persisting utterances to `store`.
pub fn run_pipeline<A: AsrBackend>(
    rx: mpsc::Receiver<CapturedFrame>,
    asr: &A,
    store: &SessionStore,
    session: &mut Session,
    opts: &TranscribeOptions,
    diarizer_cfg: DiarizerConfig,
    live_tx: Option<&broadcast::Sender<Utterance>>,
    drop_dir: Option<&Path>,
    print_tail: bool,
) -> Result<()> {
    run_pipeline_with_wav(
        rx,
        asr,
        store,
        session,
        opts,
        diarizer_cfg,
        live_tx,
        drop_dir,
        None,
        print_tail,
    )
}

/// Processes `CapturedFrame`s with optional local `.wav` retention (`FR-4.4`).
pub fn run_pipeline_with_wav<A: AsrBackend>(
    rx: mpsc::Receiver<CapturedFrame>,
    asr: &A,
    store: &SessionStore,
    session: &mut Session,
    opts: &TranscribeOptions,
    diarizer_cfg: DiarizerConfig,
    live_tx: Option<&broadcast::Sender<Utterance>>,
    drop_dir: Option<&Path>,
    wav_dir: Option<&Path>,
    print_tail: bool,
) -> Result<()> {
    run_pipeline_with_extras(
        rx,
        asr,
        store,
        session,
        opts,
        diarizer_cfg,
        live_tx,
        drop_dir,
        wav_dir,
        print_tail,
        PipelineExtras::default(),
    )
}

/// Processes `CapturedFrame`s with optional `.wav` retention and ONNX/Qwen accuracy enhancers.
#[allow(clippy::too_many_arguments)]
pub fn run_pipeline_with_extras<A: AsrBackend>(
    rx: mpsc::Receiver<CapturedFrame>,
    asr: &A,
    store: &SessionStore,
    session: &mut Session,
    opts: &TranscribeOptions,
    diarizer_cfg: DiarizerConfig,
    live_tx: Option<&broadcast::Sender<Utterance>>,
    drop_dir: Option<&Path>,
    wav_dir: Option<&Path>,
    print_tail: bool,
    extras: PipelineExtras,
) -> Result<()> {
    let mut bleed_gate = BleedGate::default_20ms();
    let mut mic_vad = extras
        .silero_vad_path
        .as_deref()
        .and_then(|p| SileroVadOnnx::load(p, 0.45).ok())
        .map(|s| VadChunker::with_silero(ChannelId::Mic, 0.015, s))
        .unwrap_or_else(|| VadChunker::new(ChannelId::Mic, 0.015));
    let mut sys_vad = extras
        .silero_vad_path
        .as_deref()
        .and_then(|p| SileroVadOnnx::load(p, 0.45).ok())
        .map(|s| VadChunker::with_silero(ChannelId::System, 0.012, s))
        .unwrap_or_else(|| VadChunker::new(ChannelId::System, 0.012));
    let enrolled = store
        .list_voiceprints()
        .unwrap_or_default()
        .into_iter()
        .map(|v| mimir_diarize::Voiceprint {
            id: v.label.clone(),
            label: v.label,
            centroid: v.embedding,
        })
        .collect();
    let mut diarizer = Diarizer::new(diarizer_cfg, enrolled);
    let mut recent_sys: Vec<Utterance> = Vec::new();
    let mut langs: HashSet<String> = HashSet::new();
    let mut mic_pcm: Vec<i16> = Vec::new();
    let mut sys_pcm: Vec<i16> = Vec::new();

    for mut frame in rx {
        if wav_dir.is_some() {
            let target = match frame.channel {
                ChannelId::Mic => &mut mic_pcm,
                ChannelId::System => &mut sys_pcm,
            };
            target.extend(
                frame
                    .samples
                    .iter()
                    .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16),
            );
        }
        let chunks = match frame.channel {
            ChannelId::System => {
                bleed_gate.observe_system(&frame.samples);
                sys_vad.push(&frame.samples)
            }
            ChannelId::Mic => {
                bleed_gate.filter_mic(&mut frame.samples);
                mic_vad.push(&frame.samples)
            }
        };

        for chunk in chunks {
            handle_chunk(
                &chunk,
                asr,
                store,
                session,
                opts,
                &mut diarizer,
                &mut recent_sys,
                &mut langs,
                live_tx,
                print_tail,
                &extras,
            )?;
        }
    }

    for chunk in [mic_vad.flush(), sys_vad.flush()].into_iter().flatten() {
        handle_chunk(
            &chunk,
            asr,
            store,
            session,
            opts,
            &mut diarizer,
            &mut recent_sys,
            &mut langs,
            live_tx,
            print_tail,
            &extras,
        )?;
    }

    session.ended_at = Some(Utc::now());
    session.languages_detected = langs.into_iter().collect();
    session.speakers = diarizer.speakers_map();
    store.finish_session(
        session.id,
        session.ended_at.unwrap(),
        &session.languages_detected,
    )?;

    if let Some(dir) = drop_dir {
        sync_to_drop_dir(session, dir)?;
    }

    if let Some(wdir) = wav_dir {
        std::fs::create_dir_all(wdir)?;
        if !mic_pcm.is_empty() {
            write_pcm16_wav(&wdir.join(format!("{}_mic.wav", session.id)), 16_000, &mic_pcm)?;
        }
        if !sys_pcm.is_empty() {
            write_pcm16_wav(&wdir.join(format!("{}_sys.wav", session.id)), 16_000, &sys_pcm)?;
        }
    }

    Ok(())
}

fn write_pcm16_wav(path: &Path, sample_rate: u32, samples: &[i16]) -> Result<()> {
    use std::io::Write;
    let data_len = (samples.len() * 2) as u32;
    let mut f = std::fs::File::create(path)?;
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_len).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?; // PCM chunk size
    f.write_all(&1u16.to_le_bytes())?;  // PCM format
    f.write_all(&1u16.to_le_bytes())?;  // Mono
    f.write_all(&sample_rate.to_le_bytes())?;
    f.write_all(&(sample_rate * 2).to_le_bytes())?; // Byte rate
    f.write_all(&2u16.to_le_bytes())?;  // Block align
    f.write_all(&16u16.to_le_bytes())?; // Bits per sample
    f.write_all(b"data")?;
    f.write_all(&data_len.to_le_bytes())?;
    for &s in samples {
        f.write_all(&s.to_le_bytes())?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_chunk<A: AsrBackend>(
    chunk: &AudioChunk,
    asr: &A,
    store: &SessionStore,
    session: &mut Session,
    opts: &TranscribeOptions,
    diarizer: &mut Diarizer,
    recent_sys: &mut Vec<Utterance>,
    langs: &mut HashSet<String>,
    live_tx: Option<&broadcast::Sender<Utterance>>,
    print_tail: bool,
    extras: &PipelineExtras,
) -> Result<()> {
    let (res, raw_emb) = asr.transcribe_with_embedding(chunk, opts)?;
    let trimmed = res.text.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    let onnx_emb = extras
        .speaker_embedder
        .as_ref()
        .and_then(|emb| emb.embed(&chunk.samples).ok());
    let pooled_emb = onnx_emb.or_else(|| {
        raw_emb.as_deref().and_then(|v| {
            // Whistle encoder produces 80 ms frames; if flat length is unknown dim, treat as 1 frame if <= 1024.
            if v.len() <= 512 {
                Some(v.to_vec())
            } else {
                mean_pool_frames(v, 384).or_else(|| Some(v.to_vec()))
            }
        })
    });

    let words = res.into_word_tokens(chunk.start_ms);
    let end_ms = words
        .last()
        .map(|w| w.end_ms)
        .unwrap_or(chunk.start_ms + (chunk.samples.len() as u64 * 1000 / 16_000));
    let avg_conf = if words.is_empty() {
        1.0
    } else {
        words.iter().map(|w| w.probability).sum::<f32>() / words.len() as f32
    };

    let speaker = diarizer.assign_speaker(chunk.channel, pooled_emb.as_deref());
    let utterance = Utterance {
        id: Uuid::new_v4(),
        speaker_id: speaker.id.clone(),
        channel: chunk.channel,
        start_ms: chunk.start_ms,
        end_ms,
        text: trimmed.to_string(),
        raw_text: None,
        language: res.language.clone(),
        avg_confidence: avg_conf,
        words,
    };

    if chunk.channel == ChannelId::Mic && diarizer.is_ghost_bleed(&utterance, recent_sys) {
        return Ok(());
    }

    if chunk.channel == ChannelId::System {
        recent_sys.push(utterance.clone());
        if recent_sys.len() > 20 {
            recent_sys.remove(0);
        }
    }

    if !res.language.is_empty() {
        langs.insert(res.language);
    }

    store.upsert_speaker(session.id, &speaker)?;
    store.append_utterance(session.id, &utterance)?;
    session.speakers.insert(speaker.id.clone(), speaker.clone());
    insert_utterance_chronological(&mut session.utterances, utterance.clone());

    if let Some(tx) = live_tx {
        let _ = tx.send(utterance.clone());
    }
    if print_tail {
        println!(
            "{}",
            mimir_bridge::mcp::format_live_utterance_md(&speaker.label, &utterance)
        );
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mimir_core::TranscriptFormat;
    use mimir_store::export::render_session;
    use tempfile::TempDir;

    struct FakeAsr;

    impl AsrBackend for FakeAsr {
        fn transcribe_with_embedding(
            &self,
            chunk: &AudioChunk,
            _opts: &TranscribeOptions,
        ) -> Result<(TranscribeResult, Option<Vec<f32>>)> {
            let text = match chunk.channel {
                ChannelId::System => "Welcome to the Mimir design review",
                ChannelId::Mic => "Thanks, let's check the requirements",
            };
            Ok((
                TranscribeResult {
                    text: text.to_string(),
                    language: "en".to_string(),
                    ttft_ms: 10.0,
                    decode_tps: 90.0,
                    words: vec![],
                },
                Some(vec![1.0, 0.0, 0.0, 0.0]),
            ))
        }
    }

    #[test]
    fn end_to_end_dual_channel_pipeline_produces_usable_markdown_and_mcp() {
        let dir = TempDir::new().unwrap();
        let drop_dir = dir.path().join("drop");
        let store = SessionStore::open(dir.path()).unwrap();
        let mut session = Session::new(Some("E2E Test".into()), vec!["Mimir".into()]);
        store.create_session(&session).unwrap();

        let (tx, rx) = mpsc::channel();
        // 1) System speaks for 1s, then 0.6s silence
        tx.send(CapturedFrame {
            channel: ChannelId::System,
            samples: vec![0.2; 16_000],
        })
        .unwrap();
        tx.send(CapturedFrame {
            channel: ChannelId::System,
            samples: vec![0.0; 9_600],
        })
        // 2) Host speaks for 1s, then 0.6s silence
        .unwrap();
        tx.send(CapturedFrame {
            channel: ChannelId::Mic,
            samples: vec![0.3; 16_000],
        })
        .unwrap();
        tx.send(CapturedFrame {
            channel: ChannelId::Mic,
            samples: vec![0.0; 9_600],
        })
        .unwrap();
        drop(tx);

        run_pipeline(
            rx,
            &FakeAsr,
            &store,
            &mut session,
            &TranscribeOptions::default(),
            DiarizerConfig::default(),
            None,
            Some(&drop_dir),
            false,
        )
        .unwrap();

        let loaded = store.latest_session().unwrap().unwrap();
        assert_eq!(loaded.utterances.len(), 2);
        let md = render_session(&loaded, TranscriptFormat::Markdown).unwrap();
        assert!(md.contains("**Speaker 1:** Welcome to the Mimir design review"));
        assert!(md.contains("**Host:** Thanks, let's check the requirements"));

        // Verify MCP tool call can read the session immediately
        let mcp_req = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_transcript","arguments":{"session_id":"latest"}}}"#;
        let resp = mimir_bridge::mcp::handle_jsonrpc_line(&store, mcp_req).unwrap();
        let resp_json = serde_json::to_string(&resp).unwrap();
        assert!(resp_json.contains("Welcome to the Mimir design review"));
        assert!(resp_json.contains("<transcript_data"));
    }
}
