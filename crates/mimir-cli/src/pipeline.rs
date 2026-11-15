//! Live and testable dual-channel audio -> Whistle -> Diarize -> Store pipeline.

use chrono::Utc;
use mimir_audio::{BleedGate, CapturedFrame, VadChunker};
use mimir_core::{AudioChunk, ChannelId, Result, Session, Utterance};
use mimir_diarize::{Diarizer, DiarizerConfig, insert_utterance_chronological, mean_pool_frames};
use mimir_store::{SessionStore, sync_to_drop_dir};
use mimir_whistle::{TranscribeOptions, TranscribeResult, WhistleEngine};
use std::collections::HashSet;
use std::path::Path;
use std::sync::mpsc;
use tokio::sync::broadcast;
use uuid::Uuid;

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
    let mut bleed_gate = BleedGate::default_20ms();
    let mut mic_vad = VadChunker::new(ChannelId::Mic, 0.015);
    let mut sys_vad = VadChunker::new(ChannelId::System, 0.012);
    let mut diarizer = Diarizer::new(diarizer_cfg, Vec::new());
    let mut recent_sys: Vec<Utterance> = Vec::new();
    let mut langs: HashSet<String> = HashSet::new();

    for mut frame in rx {
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
) -> Result<()> {
    let (res, raw_emb) = asr.transcribe_with_embedding(chunk, opts)?;
    let trimmed = res.text.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    let pooled_emb = raw_emb.as_deref().and_then(|v| {
        // Whistle encoder produces 80 ms frames; if flat length is unknown dim, treat as 1 frame if <= 1024.
        if v.len() <= 512 {
            Some(v.to_vec())
        } else {
            mean_pool_frames(v, 384).or_else(|| Some(v.to_vec()))
        }
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
