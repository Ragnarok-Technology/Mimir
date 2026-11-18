mod calendar_apps;
mod ui_kit;
mod updater;

use chrono::Utc;
use eframe::egui;
use mimir_audio::{BleedGate, SileroVadOnnx, VadChunker, start_dual_capture};
use mimir_bridge::connect::connect_detected_assistants;
use mimir_core::calendar::{CalendarEvent, CalendarSourceStatus, open_url_in_system_browser};
use mimir_core::{
    ActionItem, AudioRetention, CalDavAccount, ChannelId, KeywordPreset, MimirConfig, Session,
    Utterance,
};
use mimir_diarize::{
    Diarizer, DiarizerConfig, OnnxSpeakerEmbedder, SpeakerEmbedder, insert_utterance_chronological,
    mean_pool_frames,
};
use mimir_store::cleanup::{
    basic_heuristic_cleanup, build_qwen_cleanup_prompt_with_context, sanitize_cleanup_edit,
};
use mimir_store::{
    SearchHit, SessionStore, SessionSummary, decrypt_bytes_at_rest, encrypt_bytes_at_rest,
    ensure_private_dir,
    export::{TranscriptView, render_markdown_with_view},
    sync_session_to_drop_dir_if_enabled,
};
use mimir_whistle::artifact::{
    delete_qwen_model, delete_silero_vad, delete_speaker_embed, fetch_qwen_model, fetch_silero_vad,
    fetch_speaker_embed, fetch_whistle_weights, model_statuses, resolve_lib_path,
    resolve_qwen_paths, resolve_silero_vad_path, resolve_speaker_embed_path, resolve_weights_path,
};
use mimir_whistle::qwen::QwenCleanupModel;
use mimir_whistle::{TranscribeOptions, WhistleEngine};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use ui_kit::{
    ButtonVariant, action_button, apply_obsidian_prism_theme, badge_pill, empty_state, palette,
    pill_tab, section_card, session_card_row, styled_password_input, styled_text_input,
    toggle_switch,
};
use updater::{UpdateStatus, check_for_updates};
use uuid::Uuid;

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkspaceMode {
    Capture,
    SessionView,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsSection {
    General,
    Calendars,
    Vocabulary,
    ModelsAndMcp,
}

#[derive(Clone)]
struct LiveTurn {
    speaker_label: String,
    channel: ChannelId,
    start_ms: u64,
    end_ms: u64,
    text: String,
}

enum UiEvent {
    LiveTurn(LiveTurn),
    Status(String),
    CleanupProgress(usize, usize),
    CleanupDone,
    AutoTitleDone(Uuid, Option<String>),
    CalendarRefreshed(calendar_apps::CalendarRefreshResult),
    SessionFinished(Option<Uuid>),
    UpdateResult(UpdateStatus),
}

struct MimirDesktopApp {
    cfg: MimirConfig,
    mode: WorkspaceMode,
    settings_section: SettingsSection,
    mini_mode: bool,
    session_title: String,
    keywords_input: String,
    preset_name_input: String,
    selected_preset_name: Option<String>,
    diarize_mic: bool,
    save_wav: bool,
    encrypt_at_rest: bool,
    is_recording: bool,
    recording_started_at: Option<std::time::Instant>,
    stop_flag: Option<Arc<AtomicBool>>,
    mic_level: Arc<AtomicU32>,
    sys_level: Arc<AtomicU32>,
    live_turns: Vec<LiveTurn>,
    sessions: Vec<SessionSummary>,
    show_archived: bool,
    selected_session_id: Option<Uuid>,
    selected_session: Option<Session>,
    selected_title_edit: String,
    confirm_delete_id: Option<Uuid>,
    selected_markdown: String,
    selected_summary: Option<String>,
    selected_summary_template: Option<String>,
    selected_action_items: Vec<ActionItem>,
    selected_has_cleaned: bool,
    selected_has_summary: bool,
    show_summary_panel: bool,
    transcript_view: TranscriptView,
    search_query: String,
    search_results: Vec<SearchHit>,
    active_speaker_rename: Option<(String, String)>,
    status_line: String,
    cleanup_progress: Option<(usize, usize)>,
    auto_titling_id: Option<Uuid>,
    calendar_events: Vec<CalendarEvent>,
    calendar_statuses: Vec<CalendarSourceStatus>,
    apple_calendar_status: Option<String>,
    calendar_ics_input: String,
    show_caldav_form: bool,
    caldav_name_input: String,
    caldav_url_input: String,
    caldav_user_input: String,
    caldav_pass_input: String,
    last_calendar_poll: std::time::Instant,
    last_store_poll: std::time::Instant,
    last_db_stamp: Option<std::time::SystemTime>,
    pending_calendar_meta: Option<(String, Option<String>, Vec<String>)>,
    connect_report: String,
    update_line: String,
    event_tx: mpsc::Sender<UiEvent>,
    event_rx: mpsc::Receiver<UiEvent>,
}

impl MimirDesktopApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply_obsidian_prism_theme(&cc.egui_ctx);
        let cfg = MimirConfig::load_or_default().unwrap_or_default();
        let (event_tx, event_rx) = mpsc::channel();
        let save_wav = matches!(cfg.audio_retention, AudioRetention::ArchiveWav);
        let encrypt_at_rest = cfg.encrypt_at_rest;
        let diarize_mic = cfg.diarize_mic;
        let keywords_input = cfg.keywords.join(", ");
        let initial_db_stamp = db_modified_stamp(&cfg.data_dir);
        let mut app = Self {
            cfg,
            mode: WorkspaceMode::Capture,
            settings_section: SettingsSection::General,
            mini_mode: false,
            session_title: String::new(),
            keywords_input,
            preset_name_input: String::new(),
            selected_preset_name: None,
            diarize_mic,
            save_wav,
            encrypt_at_rest,
            is_recording: false,
            recording_started_at: None,
            stop_flag: None,
            mic_level: Arc::new(AtomicU32::new(0.0f32.to_bits())),
            sys_level: Arc::new(AtomicU32::new(0.0f32.to_bits())),
            live_turns: Vec::new(),
            sessions: Vec::new(),
            show_archived: false,
            selected_session_id: None,
            selected_session: None,
            selected_title_edit: String::new(),
            confirm_delete_id: None,
            selected_markdown: String::new(),
            selected_summary: None,
            selected_summary_template: None,
            selected_action_items: Vec::new(),
            selected_has_cleaned: false,
            selected_has_summary: false,
            show_summary_panel: false,
            transcript_view: TranscriptView::Cleaned,
            search_query: String::new(),
            search_results: Vec::new(),
            active_speaker_rename: None,
            status_line: "Ready · 100% Local · Zero Telemetry".to_string(),
            cleanup_progress: None,
            auto_titling_id: None,
            calendar_events: Vec::new(),
            calendar_statuses: Vec::new(),
            apple_calendar_status: None,
            calendar_ics_input: String::new(),
            show_caldav_form: false,
            caldav_name_input: String::new(),
            caldav_url_input: String::new(),
            caldav_user_input: String::new(),
            caldav_pass_input: String::new(),
            last_calendar_poll: std::time::Instant::now(),
            last_store_poll: std::time::Instant::now(),
            last_db_stamp: initial_db_stamp,
            pending_calendar_meta: None,
            connect_report: String::new(),
            update_line: String::new(),
            event_tx,
            event_rx,
        };
        app.refresh_sessions();
        app.refresh_calendar_events();
        app
    }

    fn toggle_mini_mode(&mut self, ctx: &egui::Context) {
        self.mini_mode = !self.mini_mode;
        if self.mini_mode {
            self.mode = WorkspaceMode::Capture;
            ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(340.0, 190.0)));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(380.0, 220.0)));
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                egui::WindowLevel::AlwaysOnTop,
            ));
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                egui::WindowLevel::Normal,
            ));
            ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(760.0, 520.0)));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(1080.0, 700.0)));
        }
    }

    fn refresh_sessions(&mut self) {
        if let Ok(store) = SessionStore::open(&self.cfg.data_dir)
            && let Ok(list) = store.list_sessions_filtered(100, self.show_archived)
        {
            self.sessions = list;
            if let Some(sel) = self.selected_session_id {
                if self.sessions.iter().any(|s| s.id == sel) {
                    self.load_session(sel);
                } else {
                    self.clear_selected_session();
                }
            }
        }
    }

    fn clear_selected_session(&mut self) {
        self.selected_session_id = None;
        self.selected_session = None;
        self.selected_markdown.clear();
        self.selected_title_edit.clear();
        self.selected_summary = None;
        self.selected_summary_template = None;
        self.selected_action_items.clear();
        self.selected_has_cleaned = false;
        self.selected_has_summary = false;
        self.show_summary_panel = false;
        self.active_speaker_rename = None;
    }

    fn load_session(&mut self, id: Uuid) {
        let switching_session = self.selected_session_id != Some(id);
        self.selected_session_id = Some(id);
        self.confirm_delete_id = None;
        if switching_session {
            self.active_speaker_rename = None;
        }
        if let Some(s) = self.sessions.iter().find(|s| s.id == id) {
            self.selected_title_edit = s.title.clone().unwrap_or_default();
        }
        if let Ok(store) = SessionStore::open(&self.cfg.data_dir)
            && let Ok(Some(session)) = store.get_session(id)
        {
            self.selected_title_edit = session.title.clone().unwrap_or_default();
            self.selected_has_cleaned = session.utterances.iter().any(|u| u.raw_text.is_some());
            self.selected_summary = session.summary_markdown.clone();
            self.selected_summary_template = session.summary_template.clone();
            self.selected_has_summary = self
                .selected_summary
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty());
            if !self.selected_has_cleaned && self.transcript_view != TranscriptView::Cleaned {
                self.transcript_view = TranscriptView::Cleaned;
            }
            if !self.selected_has_summary && self.show_summary_panel {
                self.show_summary_panel = false;
            }
            self.selected_markdown = render_markdown_with_view(&session, self.transcript_view);
            self.selected_action_items = session
                .action_items_json
                .as_deref()
                .and_then(|raw| serde_json::from_str::<Vec<ActionItem>>(raw).ok())
                .unwrap_or_default();
            self.selected_session = Some(session);
        }
    }

    fn rename_selected_session(&mut self) {
        if let Some(id) = self.selected_session_id
            && let Ok(store) = SessionStore::open(&self.cfg.data_dir)
            && store
                .rename_session(id, &self.selected_title_edit)
                .unwrap_or(false)
        {
            self.status_line = "Saved session title.".into();
            self.refresh_sessions();
            self.load_session(id);
        }
    }

    fn toggle_archive_selected_session(&mut self) {
        if let Some(id) = self.selected_session_id
            && let Some(cur) = self.sessions.iter().find(|s| s.id == id)
            && let Ok(store) = SessionStore::open(&self.cfg.data_dir)
        {
            let next_archived = !cur.archived;
            if store.set_session_archived(id, next_archived).unwrap_or(false) {
                self.status_line = if next_archived {
                    "Archived session.".into()
                } else {
                    "Restored session from archive.".into()
                };
                if next_archived && !self.show_archived {
                    self.clear_selected_session();
                    self.mode = WorkspaceMode::Capture;
                }
                self.refresh_sessions();
            }
        }
    }

    fn delete_selected_session(&mut self, id: Uuid) {
        if let Ok(store) = SessionStore::open(&self.cfg.data_dir)
            && store.delete_session(id).unwrap_or(false)
        {
            self.confirm_delete_id = None;
            self.clear_selected_session();
            self.status_line = "Deleted session permanently.".into();
            self.refresh_sessions();
            if let Some(first) = self.sessions.first() {
                let first_id = first.id;
                self.load_session(first_id);
            } else {
                self.mode = WorkspaceMode::Capture;
            }
        }
    }

    fn start_recording(&mut self) {
        if self.is_recording {
            return;
        }
        let lib_path = resolve_lib_path(&self.cfg);
        let weights_path = resolve_weights_path(&self.cfg);
        if !lib_path.exists() {
            self.status_line = format!("Missing libcactus at {}", lib_path.display());
            return;
        }
        if !weights_path.exists() {
            self.status_line =
                "Whistle ASR model not installed — open Settings → Models & MCP to download.".into();
            self.mode = WorkspaceMode::Settings;
            self.settings_section = SettingsSection::ModelsAndMcp;
            return;
        }

        let stop = Arc::new(AtomicBool::new(false));
        self.stop_flag = Some(stop.clone());
        self.is_recording = true;
        self.recording_started_at = Some(std::time::Instant::now());
        self.live_turns.clear();
        self.mode = WorkspaceMode::Capture;
        self.status_line = "Recording locally via Whistle AI…".into();

        let cfg = self.cfg.clone();
        let title = if self.session_title.trim().is_empty() {
            None
        } else {
            Some(self.session_title.trim().to_string())
        };
        let mut keywords = cfg.keywords.clone();
        keywords.extend(
            self.keywords_input
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        );
        let diarize_mic = self.diarize_mic;
        let save_wav = self.save_wav;
        let encrypt_at_rest = self.encrypt_at_rest;
        let mic_level = self.mic_level.clone();
        let sys_level = self.sys_level.clone();
        let tx_ui = self.event_tx.clone();
        let cal_meta = self.pending_calendar_meta.take();

        let kws_for_opts = keywords.clone();
        std::thread::spawn(move || {
            let mut created_session_id: Option<Uuid> = None;
            let result = (|| -> anyhow::Result<()> {
                let engine = WhistleEngine::load(&lib_path, &weights_path)?;
                let store = SessionStore::open_with_encryption(&cfg.data_dir, encrypt_at_rest)?;
                let mut session = Session::new(title, kws_for_opts.clone());
                if let Some((uid, url, attendees)) = cal_meta {
                    session.calendar_event_uid = Some(uid);
                    session.meeting_url = url;
                    if !attendees.is_empty() {
                        session.attendees_json = serde_json::to_string(&attendees).ok();
                    }
                }
                store.create_session(&session)?;
                created_session_id = Some(session.id);

                let (audio_tx, audio_rx) = mpsc::channel();
                #[cfg(target_os = "macos")]
                let mut sc_tap_child: Option<std::process::Child> = if cfg.capture_system_audio {
                    spawn_macos_sc_tap(audio_tx.clone(), stop.clone(), tx_ui.clone())
                } else {
                    None
                };
                let mut capture = start_dual_capture(audio_tx)?;

                let mut bleed_gate = BleedGate::default_20ms();
                let silero_path = resolve_silero_vad_path(&cfg);
                let mut mic_vad = silero_path
                    .exists()
                    .then(|| SileroVadOnnx::load(&silero_path, 0.45).ok())
                    .flatten()
                    .map(|s| VadChunker::with_silero(ChannelId::Mic, 0.015, s))
                    .unwrap_or_else(|| VadChunker::new(ChannelId::Mic, 0.015));
                let mut sys_vad = silero_path
                    .exists()
                    .then(|| SileroVadOnnx::load(&silero_path, 0.45).ok())
                    .flatten()
                    .map(|s| VadChunker::with_silero(ChannelId::System, 0.012, s))
                    .unwrap_or_else(|| VadChunker::new(ChannelId::System, 0.012));
                let speaker_path = resolve_speaker_embed_path(&cfg);
                let speaker_embedder = if speaker_path.is_file() {
                    OnnxSpeakerEmbedder::load(&speaker_path).ok()
                } else {
                    None
                };
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
                let mut diarizer = Diarizer::new(
                    DiarizerConfig {
                        host_label: cfg.host_label.clone(),
                        diarize_mic,
                        ..Default::default()
                    },
                    enrolled,
                );
                let mut recent_sys: Vec<Utterance> = Vec::new();
                let mut langs: HashSet<String> = HashSet::new();
                let mut mic_pcm: Vec<i16> = Vec::new();
                let mut sys_pcm: Vec<i16> = Vec::new();
                let opts = TranscribeOptions {
                    language: cfg.default_language.clone(),
                    keywords,
                    word_timestamps: true,
                };

                while !stop.load(Ordering::Relaxed) {
                    match audio_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                        Ok(mut frame) => {
                            let rms = if frame.samples.is_empty() {
                                0.0f32
                            } else {
                                (frame.samples.iter().map(|s| s * s).sum::<f32>()
                                    / frame.samples.len() as f32)
                                    .sqrt()
                            };
                            let norm = (rms * 8.0).clamp(0.0, 1.0);
                            match frame.channel {
                                ChannelId::Mic => {
                                    mic_level.store(norm.to_bits(), Ordering::Relaxed)
                                }
                                ChannelId::System => {
                                    sys_level.store(norm.to_bits(), Ordering::Relaxed)
                                }
                            }
                            if save_wav {
                                let target = match frame.channel {
                                    ChannelId::Mic => &mut mic_pcm,
                                    ChannelId::System => &mut sys_pcm,
                                };
                                for &s in &frame.samples {
                                    target.push((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
                                }
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
                                if let Ok(res) = engine.transcribe(&chunk.samples, &opts) {
                                    let trimmed = res.text.trim();
                                    if trimmed.is_empty() {
                                        continue;
                                    }
                                    let onnx_emb = speaker_embedder
                                        .as_ref()
                                        .and_then(|s| s.embed(&chunk.samples).ok());
                                    let emb = onnx_emb.or_else(|| {
                                        engine.embed(&chunk.samples).ok().and_then(|v| {
                                            if v.len() <= 512 {
                                                Some(v)
                                            } else {
                                                mean_pool_frames(&v, 384).or(Some(v))
                                            }
                                        })
                                    });
                                    let words = res.into_word_tokens(chunk.start_ms);
                                    let duration_ms = (chunk.samples.len() as u64 * 1000) / 16_000;
                                    let end_ms = words
                                        .last()
                                        .map(|w| w.end_ms)
                                        .unwrap_or(chunk.start_ms + duration_ms);
                                    let spk = diarizer.assign_speaker_with_duration(
                                        chunk.channel,
                                        emb.as_deref(),
                                        duration_ms,
                                    );
                                    let utt = Utterance {
                                        id: Uuid::new_v4(),
                                        speaker_id: spk.id.clone(),
                                        channel: chunk.channel,
                                        start_ms: chunk.start_ms,
                                        end_ms,
                                        text: trimmed.to_string(),
                                        raw_text: None,
                                        language: res.language.clone(),
                                        avg_confidence: 1.0,
                                        words,
                                    };
                                    if chunk.channel == ChannelId::Mic
                                        && diarizer.is_ghost_bleed(&utt, &recent_sys)
                                    {
                                        continue;
                                    }
                                    if chunk.channel == ChannelId::System {
                                        recent_sys.push(utt.clone());
                                    }
                                    if !res.language.is_empty() {
                                        langs.insert(res.language);
                                    }
                                    let _ = store.upsert_speaker(session.id, &spk);
                                    let _ = store.append_utterance(session.id, &utt);
                                    insert_utterance_chronological(
                                        &mut session.utterances,
                                        utt.clone(),
                                    );
                                    let _ = tx_ui.send(UiEvent::LiveTurn(LiveTurn {
                                        speaker_label: spk.label,
                                        channel: utt.channel,
                                        start_ms: utt.start_ms,
                                        end_ms: utt.end_ms,
                                        text: utt.text,
                                    }));
                                }
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            capture.check_and_reopen();
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }

                drop(capture);
                #[cfg(target_os = "macos")]
                if let Some(mut child) = sc_tap_child.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                mic_level.store(0.0f32.to_bits(), Ordering::Relaxed);
                sys_level.store(0.0f32.to_bits(), Ordering::Relaxed);
                if save_wav {
                    let audio_dir = cfg.data_dir.join("audio");
                    let _ = ensure_private_dir(&audio_dir);
                    if !mic_pcm.is_empty() {
                        save_pcm_wav_maybe_encrypted(
                            &cfg.data_dir,
                            &audio_dir,
                            &format!("{}_mic", session.id),
                            &mic_pcm,
                            encrypt_at_rest,
                        );
                    }
                    if !sys_pcm.is_empty() {
                        save_pcm_wav_maybe_encrypted(
                            &cfg.data_dir,
                            &audio_dir,
                            &format!("{}_sys", session.id),
                            &sys_pcm,
                            encrypt_at_rest,
                        );
                    }
                }
                let _ = store.finish_session(
                    session.id,
                    Utc::now(),
                    &langs.into_iter().collect::<Vec<_>>(),
                );
                if cfg.cleanup_enabled {
                    let _ = tx_ui.send(UiEvent::Status("Cleaning up transcript…".into()));
                    let (qwen_w, qwen_t) = resolve_qwen_paths(&cfg);
                    let mut qwen = if qwen_w.is_file() && qwen_t.is_file() {
                        QwenCleanupModel::load(&qwen_w, &qwen_t).ok()
                    } else {
                        None
                    };
                    let _ = store.cleanup_session_with_context_and_progress(
                        session.id,
                        |utt, prev, next| {
                            let src = utt.raw_text.as_deref().unwrap_or(&utt.text);
                            let base = basic_heuristic_cleanup(src, &kws_for_opts);
                            let cleaned = if let Some(q) = qwen.as_mut() {
                                let (sys, usr) = build_qwen_cleanup_prompt_with_context(
                                    prev,
                                    &base,
                                    next,
                                    &kws_for_opts,
                                );
                                match q.complete(&sys, &usr, 128) {
                                    Ok(cand) => sanitize_cleanup_edit(&base, &cand),
                                    Err(_) => base,
                                }
                            } else {
                                base
                            };
                            Ok(Some(cleaned))
                        },
                        |done, total| {
                            let _ = tx_ui.send(UiEvent::CleanupProgress(done, total));
                        },
                    );
                    if cfg.auto_title_enabled {
                        let _ = store.auto_title_session_with(
                            session.id,
                            false,
                            &kws_for_opts,
                            |sys, usr, max_tok| {
                                if let Some(q) = qwen.as_mut() {
                                    q.complete(sys, usr, max_tok).map(Some)
                                } else {
                                    Ok(None)
                                }
                            },
                        );
                    }
                    let _ = tx_ui.send(UiEvent::CleanupDone);
                } else if cfg.auto_title_enabled {
                    let (qwen_w, qwen_t) = resolve_qwen_paths(&cfg);
                    let mut qwen = if qwen_w.is_file() && qwen_t.is_file() {
                        QwenCleanupModel::load(&qwen_w, &qwen_t).ok()
                    } else {
                        None
                    };
                    let _ = store.auto_title_session_with(
                        session.id,
                        false,
                        &kws_for_opts,
                        |sys, usr, max_tok| {
                            if let Some(q) = qwen.as_mut() {
                                q.complete(sys, usr, max_tok).map(Some)
                            } else {
                                Ok(None)
                            }
                        },
                    );
                }
                let _ = sync_session_to_drop_dir_if_enabled(&store, session.id, &cfg);
                Ok(())
            })();

            if let Err(e) = result {
                let _ = tx_ui.send(UiEvent::Status(format!("Recording error: {e}")));
            }
            let _ = tx_ui.send(UiEvent::SessionFinished(created_session_id));
        });
    }

    fn stop_recording(&mut self) {
        if let Some(flag) = &self.stop_flag {
            flag.store(true, Ordering::Relaxed);
        }
        self.mic_level.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.sys_level.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.is_recording = false;
        self.recording_started_at = None;
        self.status_line = "Stopping & saving session…".into();
    }

    fn refresh_calendar_events(&self) {
        self.refresh_calendar_events_with_prompt(false);
    }

    fn refresh_calendar_events_with_prompt(&self, request_macos_access: bool) {
        let cfg = self.cfg.clone();
        let tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let res = calendar_apps::fetch_all_calendar_events(&cfg, request_macos_access);
            let _ = tx.send(UiEvent::CalendarRefreshed(res));
        });
    }

    fn join_and_record_event(&mut self, ev: &CalendarEvent, open_url: bool) {
        if open_url
            && let Some(ref url) = ev.meeting_url
            && let Err(e) = open_url_in_system_browser(url)
        {
            self.status_line = format!("Failed to open meeting URL: {e}");
        }
        self.session_title = ev.title.clone();
        let mut kw_parts: Vec<String> = self
            .keywords_input
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        for att in &ev.attendees {
            if !kw_parts.iter().any(|k| k.eq_ignore_ascii_case(att)) {
                kw_parts.push(att.clone());
            }
        }
        self.keywords_input = kw_parts.join(", ");
        self.pending_calendar_meta = Some((
            ev.uid.clone(),
            ev.meeting_url.clone(),
            ev.attendees.clone(),
        ));
        self.start_recording();
    }

    fn run_auto_title_for_session(&mut self, id: Uuid, force: bool) {
        if self.auto_titling_id.is_some() {
            return;
        }
        self.auto_titling_id = Some(id);
        self.status_line = "Generating concise session title…".into();
        let cfg = self.cfg.clone();
        let tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let title = (|| -> anyhow::Result<Option<String>> {
                let store = SessionStore::open(&cfg.data_dir)?;
                let (qwen_w, qwen_t) = resolve_qwen_paths(&cfg);
                let mut qwen = if qwen_w.is_file() && qwen_t.is_file() {
                    QwenCleanupModel::load(&qwen_w, &qwen_t).ok()
                } else {
                    None
                };
                let t = store.auto_title_session_with(
                    id,
                    force,
                    &cfg.keywords,
                    |sys, usr, max_tok| {
                        if let Some(q) = qwen.as_mut() {
                            q.complete(sys, usr, max_tok).map(Some)
                        } else {
                            Ok(None)
                        }
                    },
                )?;
                let _ = sync_session_to_drop_dir_if_enabled(&store, id, &cfg);
                Ok(t)
            })();
            match title {
                Ok(t) => {
                    let _ = tx.send(UiEvent::AutoTitleDone(id, t));
                }
                Err(e) => {
                    let _ = tx.send(UiEvent::Status(format!("Auto-title failed: {e}")));
                    let _ = tx.send(UiEvent::AutoTitleDone(id, None));
                }
            }
        });
    }

    fn update_search_filter(&mut self) {
        let q = self.search_query.trim();
        if q.len() < 2 {
            self.search_results.clear();
            return;
        }
        if let Ok(store) = SessionStore::open(&self.cfg.data_dir)
            && let Ok(hits) = store.search(q, 30)
        {
            self.search_results = hits;
        }
    }

    fn commit_speaker_rename(&mut self, session_id: Uuid) {
        let Some((spk_id, new_lbl)) = self.active_speaker_rename.clone() else {
            return;
        };
        let trimmed = new_lbl.trim();
        if trimmed.is_empty() {
            return;
        }
        if let Ok(store) = SessionStore::open(&self.cfg.data_dir)
            && store
                .rename_speaker(session_id, &spk_id, trimmed)
                .unwrap_or(false)
        {
            self.status_line = format!("Renamed speaker to '{trimmed}'.");
            self.active_speaker_rename = None;
            self.load_session(session_id);
        }
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.event_rx.try_recv() {
            match ev {
                UiEvent::LiveTurn(turn) => self.live_turns.push(turn),
                UiEvent::Status(msg) => self.status_line = msg,
                UiEvent::CleanupProgress(done, total) => {
                    self.cleanup_progress = Some((done, total));
                }
                UiEvent::CleanupDone => {
                    self.cleanup_progress = None;
                    self.refresh_sessions();
                    if let Some(sid) = self.selected_session_id {
                        self.load_session(sid);
                    }
                }
                UiEvent::AutoTitleDone(sid, title) => {
                    if self.auto_titling_id == Some(sid) {
                        self.auto_titling_id = None;
                    }
                    if let Some(t) = title {
                        self.status_line = format!("Auto-titled session: {t}");
                    } else {
                        self.status_line = "Could not infer a better title from transcript.".into();
                    }
                    self.refresh_sessions();
                    if self.selected_session_id == Some(sid) {
                        self.load_session(sid);
                    }
                }
                UiEvent::CalendarRefreshed(res) => {
                    self.calendar_events = res.events;
                    self.calendar_statuses = res.statuses;
                    self.apple_calendar_status = res.apple_status;
                    if let Some(updated_oauth) = res.updated_oauth_accounts {
                        self.cfg.oauth_calendar_accounts = updated_oauth;
                        let _ = self.cfg.save();
                    }
                }
                UiEvent::SessionFinished(created_id) => {
                    self.is_recording = false;
                    self.recording_started_at = None;
                    self.status_line = "Session saved locally.".into();
                    self.refresh_sessions();
                    if let Some(sid) = created_id {
                        self.load_session(sid);
                        if !self.mini_mode {
                            self.mode = WorkspaceMode::SessionView;
                        }
                    }
                }
                UiEvent::UpdateResult(status) => {
                    self.update_line = match status {
                        UpdateStatus::UpToDate(v) => format!("Up to date (v{v})"),
                        UpdateStatus::Available { version, url } => {
                            format!("Update {version} available: {url}")
                        }
                        UpdateStatus::Error(e) => e,
                    };
                }
            }
        }
    }

    fn render_mini_mode(&mut self, ctx: &egui::Context) {
        let hairline = palette::HAIRLINE;
        let cyan = palette::CYAN;
        let azure = palette::AZURE;
        let ice = palette::ICE;
        let text_primary = palette::TEXT_PRIMARY;
        let text_muted = palette::TEXT_MUTED;

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(palette::CARD_BG)
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    draw_grayscale_mimir_mark(ui, 18.0);
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("Mimir")
                            .strong()
                            .size(13.5)
                            .color(text_primary),
                    );
                    if self.is_recording {
                        let elapsed = self
                            .recording_started_at
                            .map(|t| format_duration_secs(t.elapsed().as_secs()))
                            .unwrap_or_else(|| "00:00".to_string());
                        badge_pill(
                            ui,
                            &format!("● {elapsed}"),
                            palette::DANGER,
                            palette::DANGER_BG,
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if action_button(ui, "Expand", ButtonVariant::Ghost).clicked() {
                            self.toggle_mini_mode(ctx);
                        }
                        let btn_lbl = if self.is_recording { "Stop" } else { "Record" };
                        let variant = if self.is_recording {
                            ButtonVariant::Danger
                        } else {
                            ButtonVariant::Primary
                        };
                        if action_button(ui, btn_lbl, variant).clicked() {
                            if self.is_recording {
                                self.stop_recording();
                            } else {
                                self.start_recording();
                            }
                        }
                    });
                });

                ui.add_space(8.0);
                let mic_lvl = f32::from_bits(self.mic_level.load(Ordering::Relaxed));
                let sys_lvl = f32::from_bits(self.sys_level.load(Ordering::Relaxed));
                ui.horizontal(|ui| {
                    draw_level_meter(ui, "MIC", mic_lvl, cyan, hairline, text_muted);
                    ui.add_space(10.0);
                    draw_level_meter(ui, "SYS", sys_lvl, azure, hairline, text_muted);
                });

                ui.add_space(8.0);
                egui::Frame::new()
                    .fill(palette::VOID_BG)
                    .stroke(egui::Stroke::new(1.0_f32, hairline))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.set_min_size(ui.available_size());
                        if let Some(last) = self.live_turns.last() {
                            ui.horizontal(|ui| {
                                badge_pill(
                                    ui,
                                    &last.speaker_label,
                                    ice,
                                    palette::ACTIVE_FILL,
                                );
                                ui.label(
                                    egui::RichText::new(format_ms_timestamp(last.start_ms))
                                        .monospace()
                                        .size(10.5)
                                        .color(text_muted),
                                );
                            });
                            ui.add_space(4.0);
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&last.text)
                                        .size(12.5)
                                        .color(text_primary),
                                )
                                .truncate(),
                            );
                        } else {
                            ui.label(
                                egui::RichText::new(if self.is_recording {
                                    "Listening for speech…"
                                } else {
                                    &self.status_line
                                })
                                .size(12.0)
                                .color(text_muted),
                            );
                        }
                    });
            });
    }

    fn render_sidebar(&mut self, ctx: &egui::Context) {
        let hairline = palette::HAIRLINE;
        let cyan = palette::CYAN;
        let azure = palette::AZURE;
        let ice = palette::ICE;
        let text_primary = palette::TEXT_PRIMARY;
        let text_muted = palette::TEXT_MUTED;

        egui::SidePanel::left("workspace_sidebar")
            .exact_width(264.0)
            .frame(
                egui::Frame::new()
                    .fill(palette::RAIL_BG)
                    .stroke(egui::Stroke::new(1.0_f32, hairline))
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(ctx, |ui| {
                // 1. Brand Header
                ui.horizontal(|ui| {
                    draw_grayscale_mimir_mark(ui, 20.0);
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("Mimir")
                            .strong()
                            .size(15.0)
                            .color(text_primary),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if action_button(ui, "Mini", ButtonVariant::Ghost)
                            .on_hover_text("Switch to compact floating recorder")
                            .clicked()
                        {
                            self.toggle_mini_mode(ctx);
                        }
                    });
                });

                ui.add_space(12.0);

                // 2. Primary Capture Action / Active Recording Card
                if self.is_recording {
                    let elapsed = self
                        .recording_started_at
                        .map(|t| format_duration_secs(t.elapsed().as_secs()))
                        .unwrap_or_else(|| "00:00".to_string());
                    egui::Frame::new()
                        .fill(palette::DANGER_BG)
                        .stroke(egui::Stroke::new(1.0_f32, palette::DANGER))
                        .corner_radius(8)
                        .inner_margin(egui::Margin::same(10))
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(format!("● Recording · {elapsed}"))
                                        .strong()
                                        .size(12.5)
                                        .color(palette::DANGER),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if action_button(ui, "Stop", ButtonVariant::Danger)
                                            .clicked()
                                        {
                                            self.stop_recording();
                                        }
                                    },
                                );
                            });
                            ui.add_space(6.0);
                            let mic_lvl = f32::from_bits(self.mic_level.load(Ordering::Relaxed));
                            let sys_lvl = f32::from_bits(self.sys_level.load(Ordering::Relaxed));
                            ui.horizontal(|ui| {
                                draw_level_meter(ui, "MIC", mic_lvl, cyan, hairline, text_muted);
                                ui.add_space(8.0);
                                draw_level_meter(ui, "SYS", sys_lvl, azure, hairline, text_muted);
                                if self.mode != WorkspaceMode::Capture {
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .small_button(
                                                    egui::RichText::new("View Live").color(ice),
                                                )
                                                .clicked()
                                            {
                                                self.mode = WorkspaceMode::Capture;
                                            }
                                        },
                                    );
                                }
                            });
                        });
                } else {
                    let is_capture_active = self.mode == WorkspaceMode::Capture;
                    let fill = if is_capture_active {
                        palette::ACTIVE_FILL
                    } else {
                        palette::ELEVATED_BG
                    };
                    let stroke_col = if is_capture_active {
                        cyan
                    } else {
                        palette::HAIRLINE_SOFT
                    };
                    let txt_col = if is_capture_active { ice } else { text_primary };
                    let btn = egui::Button::new(
                        egui::RichText::new("●  New Capture")
                            .strong()
                            .size(13.0)
                            .color(txt_col),
                    )
                    .fill(fill)
                    .stroke(egui::Stroke::new(1.0_f32, stroke_col))
                    .corner_radius(egui::CornerRadius::same(8))
                    .min_size(egui::vec2(ui.available_width(), 36.0));
                    if ui.add(btn).clicked() {
                        self.mode = WorkspaceMode::Capture;
                    }
                }

                ui.add_space(12.0);

                // 3. Instant Search / Filter Bar
                let search_resp = styled_text_input(
                    ui,
                    &mut self.search_query,
                    "Filter or search transcripts…",
                    None,
                );
                if search_resp.changed() {
                    self.update_search_filter();
                }

                ui.add_space(10.0);

                // 4. Sessions Header & Active/Archived Toggle
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("SESSIONS")
                            .monospace()
                            .size(10.5)
                            .strong()
                            .color(text_muted),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let filter_lbl = if self.show_archived {
                            "Showing All"
                        } else {
                            "Active Only"
                        };
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new(filter_lbl)
                                        .size(11.0)
                                        .color(palette::TEXT_SECONDARY),
                                )
                                .fill(egui::Color32::TRANSPARENT)
                                .stroke(egui::Stroke::NONE),
                            )
                            .clicked()
                        {
                            self.show_archived = !self.show_archived;
                            self.refresh_sessions();
                        }
                    });
                });

                ui.add_space(4.0);

                // Reserve space at bottom for Settings footer
                let footer_height = 54.0;
                let list_height = (ui.available_height() - footer_height).max(120.0);
                ui.allocate_ui(egui::vec2(ui.available_width(), list_height), |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("sidebar_sessions_scroll")
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 4.0;
                            let q = self.search_query.trim().to_ascii_lowercase();
                            let hit_session_ids: HashSet<Uuid> =
                                self.search_results.iter().map(|h| h.session_id).collect();
                            let filtered: Vec<SessionSummary> = self
                                .sessions
                                .iter()
                                .filter(|s| {
                                    if q.is_empty() {
                                        return true;
                                    }
                                    let title_match = s
                                        .title
                                        .as_deref()
                                        .unwrap_or("Untitled")
                                        .to_ascii_lowercase()
                                        .contains(&q);
                                    title_match || hit_session_ids.contains(&s.id)
                                })
                                .cloned()
                                .collect();

                            if filtered.is_empty() {
                                ui.add_space(20.0);
                                ui.vertical_centered(|ui| {
                                    ui.label(
                                        egui::RichText::new(if q.is_empty() {
                                            "No recorded sessions yet."
                                        } else {
                                            "No matching sessions."
                                        })
                                        .size(12.0)
                                        .color(text_muted),
                                    );
                                });
                            }

                            for s in filtered {
                                let selected = self.mode == WorkspaceMode::SessionView
                                    && self.selected_session_id == Some(s.id);
                                let dur = match s.ended_at {
                                    Some(end) => {
                                        let secs =
                                            (end - s.started_at).num_seconds().max(0) as u64;
                                        format!(" · {}", format_duration_compact(secs))
                                    }
                                    None => String::new(),
                                };
                                let meta = format!(
                                    "{}{dur} · {} turns",
                                    s.started_at.format("%b %d, %H:%M"),
                                    s.utterance_count
                                );
                                if session_card_row(
                                    ui,
                                    s.id,
                                    s.title.as_deref().unwrap_or("Untitled Session"),
                                    &meta,
                                    s.has_summary,
                                    s.archived,
                                    selected,
                                )
                                .clicked()
                                {
                                    self.load_session(s.id);
                                    self.mode = WorkspaceMode::SessionView;
                                }
                            }
                        });
                });

                ui.separator();
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    let settings_active = self.mode == WorkspaceMode::Settings;
                    if pill_tab(ui, "⚙  Settings", settings_active).clicked() {
                        self.mode = WorkspaceMode::Settings;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let badge = if self.encrypt_at_rest {
                            "Encrypted"
                        } else {
                            "Local"
                        };
                        badge_pill(ui, badge, text_muted, palette::VOID_BG);
                    });
                });
            });
    }

    fn render_capture_canvas(&mut self, ui: &mut egui::Ui) {
        let hairline = palette::HAIRLINE;
        let cyan = palette::CYAN;
        let azure = palette::AZURE;
        let ice = palette::ICE;
        let text_primary = palette::TEXT_PRIMARY;
        let text_muted = palette::TEXT_MUTED;

        // 1. Upcoming Calendar Meeting Banner (when idle and a meeting is within window)
        if !self.is_recording && !self.calendar_events.is_empty() {
            let events_snapshot = self.calendar_events.clone();
            let now_ts = Utc::now().timestamp();
            if let Some(ev) = events_snapshot.first() {
                let mins = ((ev.start_ts - now_ts) as f64 / 60.0).round() as i64;
                let timing = if ev.start_ts <= now_ts && now_ts <= ev.end_ts {
                    "Happening now".to_string()
                } else if mins >= 0 {
                    format!("Starts in {mins} min")
                } else {
                    format!("Started {} min ago", -mins)
                };
                egui::Frame::new()
                    .fill(palette::ACTIVE_FILL)
                    .stroke(egui::Stroke::new(1.0_f32, cyan))
                    .corner_radius(10)
                    .inner_margin(egui::Margin::same(16))
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.horizontal(|ui| {
                                    badge_pill(ui, "UPCOMING MEETING", ice, palette::VOID_BG);
                                    let src_badge = format_calendar_source_label(&ev.source);
                                    badge_pill(
                                        ui,
                                        src_badge,
                                        palette::TEXT_SECONDARY,
                                        palette::VOID_BG,
                                    );
                                });
                                ui.add_space(4.0);
                                ui.label(
                                    egui::RichText::new(&ev.title)
                                        .strong()
                                        .size(15.0)
                                        .color(text_primary),
                                );
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{timing} · {} attendee(s)",
                                        ev.attendees.len()
                                    ))
                                    .size(12.5)
                                    .color(palette::TEXT_SECONDARY),
                                );
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if action_button(ui, "Record Only", ButtonVariant::Secondary)
                                        .clicked()
                                    {
                                        self.join_and_record_event(ev, false);
                                    }
                                    if ev.meeting_url.is_some()
                                        && action_button(
                                            ui,
                                            "Join & Record",
                                            ButtonVariant::Primary,
                                        )
                                        .clicked()
                                    {
                                        self.join_and_record_event(ev, true);
                                    }
                                },
                            );
                        });
                    });
                ui.add_space(12.0);
            }
        }

        // 2. Capture Header Card
        egui::Frame::new()
            .fill(palette::CARD_BG)
            .stroke(egui::Stroke::new(1.0_f32, hairline))
            .corner_radius(10)
            .inner_margin(egui::Margin::same(18))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new(if self.is_recording {
                                "LIVE CAPTURE IN PROGRESS"
                            } else {
                                "NEW CAPTURE SESSION"
                            })
                            .monospace()
                            .size(10.5)
                            .strong()
                            .color(if self.is_recording {
                                palette::DANGER
                            } else {
                                text_muted
                            }),
                        );
                        ui.add_space(2.0);
                        let subtitle = if self.is_recording {
                            let elapsed = self
                                .recording_started_at
                                .map(|t| format_duration_secs(t.elapsed().as_secs()))
                                .unwrap_or_else(|| "00:00".to_string());
                            format!("Elapsed {elapsed} · Dual-channel local transcription")
                        } else {
                            "Captures microphone and system audio locally on your device."
                                .to_string()
                        };
                        ui.label(
                            egui::RichText::new(subtitle)
                                .size(12.5)
                                .color(palette::TEXT_SECONDARY),
                        );
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let btn_text = if self.is_recording {
                            "■  Stop Capture"
                        } else {
                            "●  Start Capture"
                        };
                        let variant = if self.is_recording {
                            ButtonVariant::Danger
                        } else {
                            ButtonVariant::Primary
                        };
                        if action_button(ui, btn_text, variant).clicked() {
                            if self.is_recording {
                                self.stop_recording();
                            } else {
                                self.start_recording();
                            }
                        }

                        ui.add_space(12.0);
                        let mic_lvl = f32::from_bits(self.mic_level.load(Ordering::Relaxed));
                        let sys_lvl = f32::from_bits(self.sys_level.load(Ordering::Relaxed));
                        draw_level_meter(ui, "SYS", sys_lvl, azure, hairline, text_muted);
                        ui.add_space(8.0);
                        draw_level_meter(ui, "MIC", mic_lvl, cyan, hairline, text_muted);
                    });
                });

                ui.add_space(14.0);
                ui.add_enabled_ui(!self.is_recording, |ui| {
                    ui.columns(2, |cols| {
                        cols[0].label(
                            egui::RichText::new("Meeting Title")
                                .size(11.5)
                                .color(palette::TEXT_SECONDARY),
                        );
                        cols[0].add_space(4.0);
                        styled_text_input(
                            &mut cols[0],
                            &mut self.session_title,
                            "Untitled meeting (optional)…",
                            None,
                        );

                        cols[1].horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("Vocabulary / Speaker Hints")
                                    .size(11.5)
                                    .color(palette::TEXT_SECONDARY),
                            );
                            if !self.cfg.keyword_presets.is_empty() {
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let selected_lbl = self
                                            .selected_preset_name
                                            .as_deref()
                                            .unwrap_or("Load preset…");
                                        egui::ComboBox::from_id_salt("capture_preset_combo")
                                            .selected_text(selected_lbl)
                                            .show_ui(ui, |ui| {
                                                let presets = self.cfg.keyword_presets.clone();
                                                for p in presets {
                                                    let is_sel =
                                                        self.selected_preset_name.as_deref()
                                                            == Some(&p.name);
                                                    if ui
                                                        .selectable_label(is_sel, &p.name)
                                                        .clicked()
                                                    {
                                                        self.selected_preset_name =
                                                            Some(p.name.clone());
                                                        self.keywords_input =
                                                            p.keywords.join(", ");
                                                    }
                                                }
                                            });
                                    },
                                );
                            }
                        });
                        cols[1].add_space(4.0);
                        styled_text_input(
                            &mut cols[1],
                            &mut self.keywords_input,
                            "Names, acronyms, product terms (comma-separated)…",
                            None,
                        );
                    });
                });
            });

        ui.add_space(12.0);

        // 3. Live Transcript Feed
        egui::Frame::new()
            .fill(palette::CARD_BG)
            .stroke(egui::Stroke::new(1.0_f32, hairline))
            .corner_radius(10)
            .inner_margin(egui::Margin::same(18))
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                egui::ScrollArea::vertical()
                    .id_salt("live_feed_scroll")
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        if self.live_turns.is_empty() {
                            empty_state(
                                ui,
                                if self.is_recording {
                                    "Listening for speech…"
                                } else {
                                    "Ready to capture"
                                },
                                if self.is_recording {
                                    "Utterances will appear here in real time as you or remote participants speak."
                                } else {
                                    "Click Start Capture (or press Cmd/Ctrl+Shift+M) to transcribe microphone and system audio locally."
                                },
                            );
                        } else {
                            for turn in &self.live_turns {
                                render_turn_card(
                                    ui,
                                    &turn.speaker_label,
                                    turn.channel,
                                    turn.start_ms,
                                    turn.end_ms,
                                    &turn.text,
                                    None,
                                );
                                ui.add_space(6.0);
                            }
                        }
                    });
            });
    }

    fn render_session_canvas(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(sel_id) = self.selected_session_id else {
            empty_state(
                ui,
                "No session selected",
                "Choose a session from the sidebar or start a new capture.",
            );
            return;
        };

        let hairline = palette::HAIRLINE;
        let cyan = palette::CYAN;
        let ice = palette::ICE;
        let text_primary = palette::TEXT_PRIMARY;
        let text_muted = palette::TEXT_MUTED;

        let is_archived = self
            .sessions
            .iter()
            .find(|s| s.id == sel_id)
            .is_some_and(|s| s.archived);

        // 1. Document Header Card
        egui::Frame::new()
            .fill(palette::CARD_BG)
            .stroke(egui::Stroke::new(1.0_f32, hairline))
            .corner_radius(10)
            .inner_margin(egui::Margin::same(18))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());

                // Row 1: Editable Title + Session Actions
                ui.horizontal(|ui| {
                    let title_resp = ui.add(
                        egui::TextEdit::singleline(&mut self.selected_title_edit)
                            .font(egui::FontId::proportional(18.0))
                            .text_color(text_primary)
                            .hint_text(
                                egui::RichText::new("Untitled Session")
                                    .size(18.0)
                                    .color(text_muted),
                            )
                            .frame(false)
                            .desired_width(340.0),
                    );
                    if title_resp.lost_focus() {
                        self.rename_selected_session();
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.confirm_delete_id == Some(sel_id) {
                            if action_button(ui, "Cancel", ButtonVariant::Ghost).clicked() {
                                self.confirm_delete_id = None;
                            }
                            if action_button(ui, "Confirm Delete", ButtonVariant::Danger).clicked()
                            {
                                self.delete_selected_session(sel_id);
                                return;
                            }
                        } else if action_button(ui, "Delete", ButtonVariant::Ghost).clicked() {
                            self.confirm_delete_id = Some(sel_id);
                        }

                        let arch_lbl = if is_archived { "Unarchive" } else { "Archive" };
                        if action_button(ui, arch_lbl, ButtonVariant::Ghost).clicked() {
                            self.toggle_archive_selected_session();
                        }

                        if !self.selected_markdown.is_empty()
                            && action_button(ui, "Copy Markdown", ButtonVariant::Secondary)
                                .clicked()
                        {
                            ctx.copy_text(self.selected_markdown.clone());
                            self.status_line = "Copied transcript Markdown to clipboard.".into();
                        }

                        let titling = self.auto_titling_id == Some(sel_id);
                        let title_btn_lbl = if titling { "Titling…" } else { "Auto-Title" };
                        if ui
                            .add_enabled(
                                !titling && !self.selected_markdown.is_empty(),
                                egui::Button::new(
                                    egui::RichText::new(title_btn_lbl)
                                        .size(11.5)
                                        .color(palette::TEXT_PRIMARY),
                                )
                                .fill(palette::ELEVATED_BG)
                                .stroke(egui::Stroke::new(1.0, palette::HAIRLINE))
                                .corner_radius(egui::CornerRadius::same(7)),
                            )
                            .on_hover_text("Generate a concise topic title from this transcript")
                            .clicked()
                        {
                            self.run_auto_title_for_session(sel_id, true);
                        }
                    });
                });

                // Metadata subtitle (human-readable date, duration, speaker & turn counts)
                if let Some(ref sess) = self.selected_session {
                    ui.add_space(4.0);
                    let dur_str = match sess.ended_at {
                        Some(end) => {
                            let secs = (end - sess.started_at).num_seconds().max(0) as u64;
                            format!(" · {}", format_duration_compact(secs))
                        }
                        None => String::new(),
                    };
                    let meta_str = format!(
                        "{}{dur_str} · {} speaker(s) · {} utterances",
                        sess.started_at.format("%b %d, %Y at %H:%M"),
                        sess.speakers.len().max(1),
                        sess.utterances.len()
                    );
                    ui.label(
                        egui::RichText::new(meta_str)
                            .size(12.0)
                            .color(palette::TEXT_SECONDARY),
                    );
                }

                ui.add_space(12.0);

                // Row 2: View Switcher + Post-Processing Actions
                ui.horizontal_wrapped(|ui| {
                    egui::Frame::new()
                        .fill(palette::VOID_BG)
                        .stroke(egui::Stroke::new(1.0_f32, hairline))
                        .corner_radius(egui::CornerRadius::same(8))
                        .inner_margin(egui::Margin::symmetric(4, 3))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                let primary_lbl = if self.selected_has_cleaned {
                                    "Cleaned"
                                } else {
                                    "Transcript"
                                };
                                if pill_tab(
                                    ui,
                                    primary_lbl,
                                    !self.show_summary_panel
                                        && self.transcript_view == TranscriptView::Cleaned,
                                )
                                .clicked()
                                {
                                    self.show_summary_panel = false;
                                    self.transcript_view = TranscriptView::Cleaned;
                                    self.load_session(sel_id);
                                }
                                if self.selected_has_cleaned {
                                    for (view, lbl) in
                                        [(TranscriptView::Raw, "Raw"), (TranscriptView::Diff, "Diff")]
                                    {
                                        if pill_tab(
                                            ui,
                                            lbl,
                                            !self.show_summary_panel
                                                && self.transcript_view == view,
                                        )
                                        .clicked()
                                        {
                                            self.show_summary_panel = false;
                                            self.transcript_view = view;
                                            self.load_session(sel_id);
                                        }
                                    }
                                }
                                let sum_lbl = if !self.selected_action_items.is_empty() {
                                    format!("Summary ({})", self.selected_action_items.len())
                                } else {
                                    "Summary".to_string()
                                };
                                if pill_tab(ui, &sum_lbl, self.show_summary_panel).clicked() {
                                    self.show_summary_panel = true;
                                }
                            });
                        });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if action_button(ui, "Clean Up Transcript", ButtonVariant::Primary)
                            .clicked()
                        {
                            let data_dir = self.cfg.data_dir.clone();
                            let (qwen_w, qwen_t) = resolve_qwen_paths(&self.cfg);
                            let mut kws = self.cfg.keywords.clone();
                            let tx = self.event_tx.clone();
                            self.status_line = "Running local transcript cleanup…".into();
                            self.cleanup_progress = Some((0, 0));
                            std::thread::spawn(move || {
                                let res: anyhow::Result<String> = (|| {
                                    let store = SessionStore::open(&data_dir)?;
                                    if let Some(sess) = store.get_session(sel_id)? {
                                        for k in &sess.keywords_used {
                                            if !kws.contains(k) {
                                                kws.push(k.clone());
                                            }
                                        }
                                    }
                                    let mut qwen = if qwen_w.is_file() && qwen_t.is_file() {
                                        Some(QwenCleanupModel::load(&qwen_w, &qwen_t)?)
                                    } else {
                                        None
                                    };
                                    let used_qwen = qwen.is_some();
                                    let updated = store.cleanup_session_with_context_and_progress(
                                        sel_id,
                                        |utt, prev, next| {
                                            let src =
                                                utt.raw_text.as_deref().unwrap_or(&utt.text);
                                            let base = basic_heuristic_cleanup(src, &kws);
                                            let cleaned = if let Some(q) = qwen.as_mut() {
                                                let (sys, usr) =
                                                    build_qwen_cleanup_prompt_with_context(
                                                        prev, &base, next, &kws,
                                                    );
                                                match q.complete(&sys, &usr, 128) {
                                                    Ok(cand) => {
                                                        sanitize_cleanup_edit(&base, &cand)
                                                    }
                                                    Err(_) => base,
                                                }
                                            } else {
                                                base
                                            };
                                            Ok(Some(cleaned))
                                        },
                                        |done, total| {
                                            let _ =
                                                tx.send(UiEvent::CleanupProgress(done, total));
                                        },
                                    )?;
                                    Ok(format!(
                                        "Cleaned {updated} utterance(s) via {}",
                                        if used_qwen {
                                            "Qwen 2.5 0.5B"
                                        } else {
                                            "heuristics"
                                        }
                                    ))
                                })();
                                let msg = match res {
                                    Ok(s) => s,
                                    Err(e) => format!("Cleanup error: {e}"),
                                };
                                let _ = tx.send(UiEvent::CleanupDone);
                                let _ = tx.send(UiEvent::Status(msg));
                            });
                        }

                        if self.selected_has_cleaned
                            && action_button(ui, "Revert Raw", ButtonVariant::Secondary).clicked()
                            && let Ok(store) = SessionStore::open(&self.cfg.data_dir)
                            && let Ok(n) = store.revert_session_cleanup(sel_id)
                        {
                            self.status_line = format!("Reverted {n} utterance(s) to raw text.");
                            self.load_session(sel_id);
                        }

                        let audio_dir = self.cfg.data_dir.join("audio");
                        let has_audio = ["mic", "sys"].iter().any(|ch| {
                            audio_dir.join(format!("{sel_id}_{ch}.wav")).is_file()
                                || audio_dir.join(format!("{sel_id}_{ch}.wav.enc")).is_file()
                        });
                        if has_audio
                            && action_button(ui, "Export WAV", ButtonVariant::Secondary).clicked()
                        {
                            let export_dir = self.cfg.data_dir.join("exports");
                            let _ = std::fs::create_dir_all(&export_dir);
                            let mut exported = 0usize;
                            for ch in ["mic", "sys"] {
                                let plain = audio_dir.join(format!("{sel_id}_{ch}.wav"));
                                let enc = audio_dir.join(format!("{sel_id}_{ch}.wav.enc"));
                                let out = export_dir.join(format!("{sel_id}_{ch}.wav"));
                                if plain.is_file() {
                                    if std::fs::copy(&plain, &out).is_ok() {
                                        exported += 1;
                                    }
                                } else if enc.is_file()
                                    && let Ok(raw) = std::fs::read(&enc)
                                    && let Ok(dec) =
                                        decrypt_bytes_at_rest(&self.cfg.data_dir, &raw)
                                    && std::fs::write(&out, dec).is_ok()
                                {
                                    exported += 1;
                                }
                            }
                            self.status_line = if exported > 0 {
                                format!(
                                    "Exported {exported} WAV file(s) to {}",
                                    export_dir.display()
                                )
                            } else {
                                "Failed to export WAV archive.".into()
                            };
                        }
                    });
                });

                if let Some((done, total)) = self.cleanup_progress {
                    ui.add_space(10.0);
                    let frac = if total > 0 {
                        done as f32 / total as f32
                    } else {
                        0.0
                    };
                    let label = if total > 0 {
                        format!("Cleaning {done} / {total} utterances…")
                    } else {
                        "Loading Qwen 2.5 0.5B…".to_string()
                    };
                    ui.add(egui::ProgressBar::new(frac).text(label).animate(total == 0));
                    ui.ctx().request_repaint();
                }

                // Row 3: Interactive Speaker Chips (click any speaker chip to rename inline)
                if let Some(ref sess) = self.selected_session
                    && !sess.speakers.is_empty()
                {
                    let speakers: Vec<_> = sess.speakers.values().cloned().collect();
                    ui.add_space(10.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new("Speakers:")
                                .size(11.5)
                                .color(text_muted),
                        );
                        for spk in speakers {
                            let is_editing = self
                                .active_speaker_rename
                                .as_ref()
                                .is_some_and(|(id, _)| id == &spk.id);
                            let chip_col = match spk.channel {
                                ChannelId::Mic => cyan,
                                ChannelId::System => palette::AZURE,
                            };
                            let btn = egui::Button::new(
                                egui::RichText::new(format!("● {} ✎", spk.label))
                                    .size(11.5)
                                    .color(if is_editing { ice } else { text_primary }),
                            )
                            .fill(if is_editing {
                                palette::ACTIVE_FILL
                            } else {
                                palette::VOID_BG
                            })
                            .stroke(egui::Stroke::new(
                                1.0_f32,
                                if is_editing { chip_col } else { hairline },
                            ))
                            .corner_radius(egui::CornerRadius::same(12));
                            if ui
                                .add(btn)
                                .on_hover_text("Click to rename speaker across this session")
                                .clicked()
                            {
                                if is_editing {
                                    self.active_speaker_rename = None;
                                } else {
                                    self.active_speaker_rename =
                                        Some((spk.id.clone(), spk.label.clone()));
                                }
                            }
                        }
                    });

                    if let Some((_, ref mut edit_name)) = self.active_speaker_rename {
                        ui.add_space(8.0);
                        let mut should_commit = false;
                        let mut should_cancel = false;
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("Rename speaker to:")
                                    .size(12.0)
                                    .color(palette::TEXT_SECONDARY),
                            );
                            let resp = styled_text_input(
                                ui,
                                edit_name,
                                "Enter participant name…",
                                Some(200.0),
                            );
                            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                should_commit = true;
                            }
                            if action_button(ui, "Save Name", ButtonVariant::Primary).clicked() {
                                should_commit = true;
                            }
                            if action_button(ui, "Cancel", ButtonVariant::Ghost).clicked() {
                                should_cancel = true;
                            }
                        });
                        if should_commit {
                            self.commit_speaker_rename(sel_id);
                        } else if should_cancel {
                            self.active_speaker_rename = None;
                        }
                    }
                }
            });

        ui.add_space(12.0);

        // 2. Main Transcript / Summary Document Area
        egui::Frame::new()
            .fill(palette::CARD_BG)
            .stroke(egui::Stroke::new(1.0_f32, hairline))
            .corner_radius(10)
            .inner_margin(egui::Margin::same(18))
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                egui::ScrollArea::vertical()
                    .id_salt("session_body_scroll")
                    .show(ui, |ui| {
                        if self.show_summary_panel {
                            if let Some(ref summary) = self.selected_summary {
                                let hdr = match self.selected_summary_template.as_deref() {
                                    Some(t) if !t.trim().is_empty() => {
                                        format!("MEETING SUMMARY ({t})")
                                    }
                                    _ => "MEETING SUMMARY".to_string(),
                                };
                                ui.label(
                                    egui::RichText::new(hdr)
                                        .monospace()
                                        .size(11.0)
                                        .strong()
                                        .color(ice),
                                );
                                ui.add_space(8.0);
                                render_markdown_view(
                                    ui,
                                    summary,
                                    ice,
                                    cyan,
                                    text_primary,
                                    text_muted,
                                    hairline,
                                );
                            } else {
                                empty_state(
                                    ui,
                                    "No summary saved for this session yet",
                                    "Ask your connected MCP assistant (Claude, Cursor, Windsurf) to summarize this meeting and call save_summary.",
                                );
                            }
                            if !self.selected_action_items.is_empty() {
                                ui.add_space(16.0);
                                ui.label(
                                    egui::RichText::new("ACTION ITEMS")
                                        .monospace()
                                        .size(11.0)
                                        .strong()
                                        .color(cyan),
                                );
                                ui.add_space(8.0);
                                for item in &self.selected_action_items {
                                    let owner = item.owner.as_deref().unwrap_or("Unassigned");
                                    egui::Frame::new()
                                        .fill(palette::VOID_BG)
                                        .stroke(egui::Stroke::new(1.0_f32, hairline))
                                        .corner_radius(8)
                                        .inner_margin(egui::Margin::symmetric(12, 8))
                                        .show(ui, |ui| {
                                            ui.set_min_width(ui.available_width());
                                            ui.horizontal_wrapped(|ui| {
                                                badge_pill(
                                                    ui,
                                                    owner,
                                                    ice,
                                                    palette::ACTIVE_FILL,
                                                );
                                                ui.label(
                                                    egui::RichText::new(&item.task)
                                                        .size(13.0)
                                                        .color(text_primary),
                                                );
                                                if let Some(ref due) = item.due_hint {
                                                    badge_pill(
                                                        ui,
                                                        &format!("Due: {due}"),
                                                        palette::TEXT_SECONDARY,
                                                        palette::CARD_BG,
                                                    );
                                                }
                                                if let Some(ref ts) = item.timestamp {
                                                    ui.label(
                                                        egui::RichText::new(format!("[{ts}]"))
                                                            .monospace()
                                                            .size(11.0)
                                                            .color(text_muted),
                                                    );
                                                }
                                            });
                                        });
                                    ui.add_space(6.0);
                                }
                            }
                        } else if let Some(ref sess) = self.selected_session {
                            if sess.utterances.is_empty() {
                                empty_state(
                                    ui,
                                    "No speech recorded in this session",
                                    "This session completed without any transcribed utterances.",
                                );
                            } else {
                                for utt in &sess.utterances {
                                    let spk_name = sess
                                        .speakers
                                        .get(&utt.speaker_id)
                                        .map(|s| s.label.as_str())
                                        .unwrap_or(&utt.speaker_id);
                                    let (primary_text, diff_raw) = match self.transcript_view {
                                        TranscriptView::Cleaned => (utt.text.as_str(), None),
                                        TranscriptView::Raw => (
                                            utt.raw_text.as_deref().unwrap_or(utt.text.as_str()),
                                            None,
                                        ),
                                        TranscriptView::Diff => {
                                            let raw_diff = utt
                                                .raw_text
                                                .as_deref()
                                                .filter(|r| *r != utt.text.as_str());
                                            (utt.text.as_str(), raw_diff)
                                        }
                                    };
                                    render_turn_card(
                                        ui,
                                        spk_name,
                                        utt.channel,
                                        utt.start_ms,
                                        utt.end_ms,
                                        primary_text,
                                        diff_raw,
                                    );
                                    ui.add_space(6.0);
                                }
                            }
                        }
                    });
            });
    }

    fn render_settings_canvas(&mut self, ui: &mut egui::Ui) {
        let hairline = palette::HAIRLINE;
        let text_primary = palette::TEXT_PRIMARY;
        let text_muted = palette::TEXT_MUTED;
        let ice = palette::ICE;

        // Sub-navigation Header
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Settings")
                    .strong()
                    .size(19.0)
                    .color(text_primary),
            );
            ui.add_space(16.0);
            egui::Frame::new()
                .fill(palette::CARD_BG)
                .stroke(egui::Stroke::new(1.0_f32, hairline))
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(egui::Margin::symmetric(4, 3))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        for (sec, label) in [
                            (SettingsSection::General, "General & Privacy"),
                            (SettingsSection::Calendars, "Calendars"),
                            (SettingsSection::Vocabulary, "Vocabulary"),
                            (SettingsSection::ModelsAndMcp, "Models & MCP"),
                        ] {
                            if pill_tab(ui, label, self.settings_section == sec).clicked() {
                                self.settings_section = sec;
                            }
                        }
                    });
                });
        });

        ui.add_space(14.0);

        egui::ScrollArea::vertical()
            .id_salt("settings_canvas_scroll")
            .show(ui, |ui| match self.settings_section {
                SettingsSection::General => {
                    section_card(ui, "AUDIO CAPTURE & PRIVACY", None, |ui| {
                        ui.spacing_mut().item_spacing.y = 14.0;
                        if toggle_switch(
                            ui,
                            &mut self.cfg.capture_system_audio,
                            "Capture system audio",
                            Some("Record remote meeting participants via OS audio loopback / ScreenCaptureKit."),
                            !self.is_recording,
                        )
                        .changed()
                        {
                            let _ = self.cfg.save();
                        }
                        if toggle_switch(
                            ui,
                            &mut self.diarize_mic,
                            "Multi-speaker microphone diarization",
                            Some("Separate multiple local speakers sharing the same microphone."),
                            !self.is_recording,
                        )
                        .changed()
                        {
                            self.cfg.diarize_mic = self.diarize_mic;
                            let _ = self.cfg.save();
                        }
                        if toggle_switch(
                            ui,
                            &mut self.save_wav,
                            "Save local WAV audio archives",
                            Some("Retain 16 kHz WAV recordings per session (encrypted when at-rest encryption is enabled)."),
                            !self.is_recording,
                        )
                        .changed()
                        {
                            self.cfg.audio_retention = if self.save_wav {
                                AudioRetention::ArchiveWav
                            } else {
                                AudioRetention::Ephemeral
                            };
                            let _ = self.cfg.save();
                        }
                        if toggle_switch(
                            ui,
                            &mut self.cfg.cleanup_enabled,
                            "Automatic transcript cleanup on stop",
                            Some("Run local heuristic & Qwen 2.5 0.5B cleanup automatically when capture finishes."),
                            !self.is_recording,
                        )
                        .changed()
                        {
                            let _ = self.cfg.save();
                        }
                        if toggle_switch(
                            ui,
                            &mut self.cfg.auto_title_enabled,
                            "Auto-title untitled sessions on stop",
                            Some("Generate a concise topic title after recording (Qwen 2.5 0.5B if installed, else keyword fallback)."),
                            !self.is_recording,
                        )
                        .changed()
                        {
                            let _ = self.cfg.save();
                        }
                        if toggle_switch(
                            ui,
                            &mut self.encrypt_at_rest,
                            "At-rest encryption (ChaCha20-Poly1305)",
                            Some("Encrypt SQLite utterances, FTS5 search index, WAL files, and WAV archives on disk."),
                            !self.is_recording,
                        )
                        .changed()
                        {
                            self.cfg.encrypt_at_rest = self.encrypt_at_rest;
                            let _ = self.cfg.save();
                            if self.encrypt_at_rest
                                && let Ok(store) =
                                    SessionStore::open_with_encryption(&self.cfg.data_dir, true)
                                && let Ok(count) = store.encrypt_existing_data()
                            {
                                self.status_line = format!(
                                    "At-rest encryption enabled ({count} existing record(s) encrypted)."
                                );
                            }
                        }
                        let drop_sub = match self.cfg.drop_dir.as_ref() {
                            Some(d) => format!("Automatically export sessions to {}", d.display()),
                            None => "Automatically write Markdown and JSONL exports on stop, cleanup, or summary save.".to_string(),
                        };
                        if toggle_switch(
                            ui,
                            &mut self.cfg.drop_sync_enabled,
                            "Sync Markdown & JSON exports to Drop Folder",
                            Some(&drop_sub),
                            true,
                        )
                        .changed()
                        {
                            let _ = self.cfg.save();
                        }
                    });
                }
                SettingsSection::Calendars => {
                    section_card(ui, "LOCAL CALENDAR DETECTION", None, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(
                                    "Mimir automatically checks your local OS calendar apps for upcoming meetings starting within 15 minutes.",
                                )
                                .size(12.5)
                                .color(palette::TEXT_SECONDARY),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if action_button(
                                        ui,
                                        "Refresh Now",
                                        ButtonVariant::Secondary,
                                    )
                                    .clicked()
                                    {
                                        self.refresh_calendar_events();
                                    }
                                    #[cfg(target_os = "macos")]
                                    {
                                        if matches!(
                                            self.apple_calendar_status.as_deref(),
                                            Some("not_determined")
                                        ) && action_button(
                                            ui,
                                            "Grant Apple Calendar Access",
                                            ButtonVariant::Primary,
                                        )
                                        .clicked()
                                        {
                                            self.refresh_calendar_events_with_prompt(true);
                                        } else if matches!(
                                            self.apple_calendar_status.as_deref(),
                                            Some("denied") | Some("restricted") | Some("write_only")
                                        ) && action_button(
                                            ui,
                                            "Open macOS Privacy Settings",
                                            ButtonVariant::Secondary,
                                        )
                                        .clicked()
                                        {
                                            let _ = open_url_in_system_browser(
                                                "x-apple.systempreferences:com.apple.preference.security?Privacy_Calendars",
                                            );
                                        }
                                    }
                                },
                            );
                        });

                        if !self.calendar_statuses.is_empty() {
                            ui.add_space(10.0);
                            for st in &self.calendar_statuses {
                                let is_connected = st.status.starts_with("Connected");
                                let is_not_detected = st.status == "Not detected";
                                if is_not_detected && self.calendar_statuses.len() > 1 {
                                    continue;
                                }
                                egui::Frame::new()
                                    .fill(palette::VOID_BG)
                                    .stroke(egui::Stroke::new(1.0_f32, hairline))
                                    .corner_radius(egui::CornerRadius::same(7))
                                    .inner_margin(egui::Margin::symmetric(12, 8))
                                    .show(ui, |ui| {
                                        ui.set_min_width(ui.available_width());
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                egui::RichText::new(&st.name)
                                                    .strong()
                                                    .color(text_primary),
                                            );
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    let (fg, bg) = if is_connected {
                                                        (palette::SUCCESS, palette::SUCCESS_BG)
                                                    } else {
                                                        (palette::TEXT_SECONDARY, palette::CARD_BG)
                                                    };
                                                    badge_pill(
                                                        ui,
                                                        &format!(
                                                            "{} · {} upcoming",
                                                            st.status, st.event_count
                                                        ),
                                                        fg,
                                                        bg,
                                                    );
                                                },
                                            );
                                        });
                                    });
                                ui.add_space(4.0);
                            }
                        }
                    });

                    ui.add_space(12.0);

                    section_card(ui, "CALENDAR FEEDS (.ICS)", None, |ui| {
                        ui.label(
                            egui::RichText::new(
                                "Paste a private .ics feed URL from Google Calendar, Outlook 365, or iCloud, or a local .ics file path.",
                            )
                            .size(12.5)
                            .color(palette::TEXT_SECONDARY),
                        );
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            let input_w = (ui.available_width() - 105.0).max(180.0);
                            styled_text_input(
                                ui,
                                &mut self.calendar_ics_input,
                                "https://calendar.google.com/calendar/ical/.../basic.ics",
                                Some(input_w),
                            );
                            if action_button(ui, "Add Feed", ButtonVariant::Primary).clicked() {
                                let trimmed = self.calendar_ics_input.trim().to_string();
                                if !trimmed.is_empty()
                                    && !self.cfg.calendar_ics_urls.contains(&trimmed)
                                {
                                    self.cfg.calendar_ics_urls.push(trimmed);
                                    self.calendar_ics_input.clear();
                                    let _ = self.cfg.save();
                                    self.refresh_calendar_events();
                                }
                            }
                        });

                        if !self.cfg.calendar_ics_urls.is_empty() {
                            ui.add_space(10.0);
                            let mut remove_idx = None;
                            for (idx, url) in self.cfg.calendar_ics_urls.iter().enumerate() {
                                egui::Frame::new()
                                    .fill(palette::VOID_BG)
                                    .stroke(egui::Stroke::new(1.0_f32, hairline))
                                    .corner_radius(egui::CornerRadius::same(7))
                                    .inner_margin(egui::Margin::symmetric(12, 8))
                                    .show(ui, |ui| {
                                        ui.set_min_width(ui.available_width());
                                        ui.horizontal(|ui| {
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(url)
                                                        .monospace()
                                                        .size(11.5)
                                                        .color(text_primary),
                                                )
                                                .truncate(),
                                            );
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if action_button(
                                                        ui,
                                                        "Remove",
                                                        ButtonVariant::Danger,
                                                    )
                                                    .clicked()
                                                    {
                                                        remove_idx = Some(idx);
                                                    }
                                                },
                                            );
                                        });
                                    });
                                ui.add_space(4.0);
                            }
                            if let Some(idx) = remove_idx {
                                self.cfg.calendar_ics_urls.remove(idx);
                                let _ = self.cfg.save();
                                self.refresh_calendar_events();
                            }
                        }
                    });

                    ui.add_space(12.0);

                    section_card(ui, "CALDAV ACCOUNTS (OPTIONAL)", None, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(
                                    "Connect iCloud, Fastmail, Nextcloud, or Radicale directly via CalDAV app password.",
                                )
                                .size(12.5)
                                .color(palette::TEXT_SECONDARY),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let lbl = if self.show_caldav_form {
                                        "Hide Form"
                                    } else {
                                        "+ Add CalDAV Account"
                                    };
                                    if action_button(ui, lbl, ButtonVariant::Secondary).clicked() {
                                        self.show_caldav_form = !self.show_caldav_form;
                                    }
                                },
                            );
                        });

                        if self.show_caldav_form {
                            ui.add_space(10.0);
                            ui.columns(2, |cols| {
                                cols[0].label(
                                    egui::RichText::new("Account Label")
                                        .size(11.5)
                                        .color(palette::TEXT_SECONDARY),
                                );
                                styled_text_input(
                                    &mut cols[0],
                                    &mut self.caldav_name_input,
                                    "e.g. Fastmail or Nextcloud",
                                    None,
                                );
                                cols[0].add_space(6.0);
                                cols[0].label(
                                    egui::RichText::new("Username / Email")
                                        .size(11.5)
                                        .color(palette::TEXT_SECONDARY),
                                );
                                styled_text_input(
                                    &mut cols[0],
                                    &mut self.caldav_user_input,
                                    "you@example.com",
                                    None,
                                );

                                cols[1].label(
                                    egui::RichText::new("CalDAV Calendar URL")
                                        .size(11.5)
                                        .color(palette::TEXT_SECONDARY),
                                );
                                styled_text_input(
                                    &mut cols[1],
                                    &mut self.caldav_url_input,
                                    "https://caldav.fastmail.com/dav/calendars/...",
                                    None,
                                );
                                cols[1].add_space(6.0);
                                cols[1].label(
                                    egui::RichText::new("App Password")
                                        .size(11.5)
                                        .color(palette::TEXT_SECONDARY),
                                );
                                styled_password_input(
                                    &mut cols[1],
                                    &mut self.caldav_pass_input,
                                    "App-specific password",
                                    None,
                                );
                            });
                            ui.add_space(8.0);
                            if action_button(ui, "Save CalDAV Account", ButtonVariant::Primary)
                                .clicked()
                            {
                                let url = self.caldav_url_input.trim().to_string();
                                if !url.is_empty() {
                                    self.cfg.caldav_accounts.push(CalDavAccount {
                                        name: self.caldav_name_input.trim().to_string(),
                                        url,
                                        username: self.caldav_user_input.trim().to_string(),
                                        password: self.caldav_pass_input.clone(),
                                    });
                                    self.caldav_name_input.clear();
                                    self.caldav_url_input.clear();
                                    self.caldav_user_input.clear();
                                    self.caldav_pass_input.clear();
                                    self.show_caldav_form = false;
                                    let _ = self.cfg.save();
                                    self.refresh_calendar_events();
                                }
                            }
                        }

                        if !self.cfg.caldav_accounts.is_empty() {
                            ui.add_space(10.0);
                            let mut remove_caldav = None;
                            for (idx, acc) in self.cfg.caldav_accounts.iter().enumerate() {
                                egui::Frame::new()
                                    .fill(palette::VOID_BG)
                                    .stroke(egui::Stroke::new(1.0_f32, hairline))
                                    .corner_radius(egui::CornerRadius::same(7))
                                    .inner_margin(egui::Margin::symmetric(12, 8))
                                    .show(ui, |ui| {
                                        ui.set_min_width(ui.available_width());
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "{} ({})",
                                                    if acc.name.is_empty() {
                                                        "CalDAV"
                                                    } else {
                                                        &acc.name
                                                    },
                                                    acc.username
                                                ))
                                                .strong()
                                                .color(text_primary),
                                            );
                                            ui.label(
                                                egui::RichText::new(&acc.url)
                                                    .monospace()
                                                    .size(11.0)
                                                    .color(text_muted),
                                            );
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if action_button(
                                                        ui,
                                                        "Remove",
                                                        ButtonVariant::Danger,
                                                    )
                                                    .clicked()
                                                    {
                                                        remove_caldav = Some(idx);
                                                    }
                                                },
                                            );
                                        });
                                    });
                                ui.add_space(4.0);
                            }
                            if let Some(idx) = remove_caldav {
                                self.cfg.caldav_accounts.remove(idx);
                                let _ = self.cfg.save();
                                self.refresh_calendar_events();
                            }
                        }
                    });
                }
                SettingsSection::Vocabulary => {
                    section_card(ui, "KEYWORD & VOCABULARY PRESETS", None, |ui| {
                        ui.label(
                            egui::RichText::new(
                                "Create reusable vocabulary sets (colleague names, project codenames, technical terms) to bias Whistle ASR and Qwen cleanup.",
                            )
                            .size(12.5)
                            .color(palette::TEXT_SECONDARY),
                        );
                        ui.add_space(10.0);
                        ui.columns(2, |cols| {
                            cols[0].label(
                                egui::RichText::new("Preset Name")
                                    .size(11.5)
                                    .color(palette::TEXT_SECONDARY),
                            );
                            styled_text_input(
                                &mut cols[0],
                                &mut self.preset_name_input,
                                "e.g. Engineering Standup",
                                None,
                            );

                            cols[1].label(
                                egui::RichText::new("Keywords (comma-separated)")
                                    .size(11.5)
                                    .color(palette::TEXT_SECONDARY),
                            );
                            styled_text_input(
                                &mut cols[1],
                                &mut self.keywords_input,
                                "e.g. Kubernetes, Mimir, Whistle, Alice",
                                None,
                            );
                        });
                        ui.add_space(8.0);
                        if action_button(ui, "Save Preset", ButtonVariant::Primary).clicked() {
                            let name = self.preset_name_input.trim().to_string();
                            let kws: Vec<String> = self
                                .keywords_input
                                .split(',')
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty())
                                .collect();
                            if !name.is_empty() {
                                if let Some(existing) = self
                                    .cfg
                                    .keyword_presets
                                    .iter_mut()
                                    .find(|p| p.name.eq_ignore_ascii_case(&name))
                                {
                                    existing.keywords = kws;
                                } else {
                                    self.cfg.keyword_presets.push(KeywordPreset {
                                        name: name.clone(),
                                        keywords: kws,
                                    });
                                }
                                self.selected_preset_name = Some(name.clone());
                                let _ = self.cfg.save();
                                self.status_line = format!("Saved keyword preset '{name}'.");
                            }
                        }

                        if !self.cfg.keyword_presets.is_empty() {
                            ui.add_space(12.0);
                            let presets = self.cfg.keyword_presets.clone();
                            let mut delete_name: Option<String> = None;
                            for p in presets {
                                egui::Frame::new()
                                    .fill(palette::VOID_BG)
                                    .stroke(egui::Stroke::new(1.0_f32, hairline))
                                    .corner_radius(egui::CornerRadius::same(7))
                                    .inner_margin(egui::Margin::symmetric(12, 8))
                                    .show(ui, |ui| {
                                        ui.set_min_width(ui.available_width());
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                egui::RichText::new(&p.name)
                                                    .strong()
                                                    .color(text_primary),
                                            );
                                            ui.label(
                                                egui::RichText::new(p.keywords.join(", "))
                                                    .size(12.0)
                                                    .color(palette::TEXT_SECONDARY),
                                            );
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if action_button(
                                                        ui,
                                                        "Delete",
                                                        ButtonVariant::Danger,
                                                    )
                                                    .clicked()
                                                    {
                                                        delete_name = Some(p.name.clone());
                                                    }
                                                    if action_button(
                                                        ui,
                                                        "Load",
                                                        ButtonVariant::Secondary,
                                                    )
                                                    .clicked()
                                                    {
                                                        self.selected_preset_name =
                                                            Some(p.name.clone());
                                                        self.preset_name_input = p.name.clone();
                                                        self.keywords_input =
                                                            p.keywords.join(", ");
                                                    }
                                                },
                                            );
                                        });
                                    });
                                ui.add_space(4.0);
                            }
                            if let Some(del) = delete_name {
                                self.cfg
                                    .keyword_presets
                                    .retain(|p| !p.name.eq_ignore_ascii_case(&del));
                                if self
                                    .selected_preset_name
                                    .as_deref()
                                    .is_some_and(|s| s.eq_ignore_ascii_case(&del))
                                {
                                    self.selected_preset_name = None;
                                }
                                let _ = self.cfg.save();
                                self.status_line = format!("Deleted keyword preset '{del}'.");
                            }
                        }
                    });
                }
                SettingsSection::ModelsAndMcp => {
                    section_card(ui, "AI ASSISTANT INTEGRATION (MCP)", None, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(
                                    "Register Mimir's local MCP server with Claude Desktop, Cursor, Windsurf, and other MCP clients so they can read transcripts and save meeting summaries.",
                                )
                                .size(12.5)
                                .color(palette::TEXT_SECONDARY),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if action_button(
                                        ui,
                                        "Connect Installed AI Assistants",
                                        ButtonVariant::Primary,
                                    )
                                    .clicked()
                                    {
                                        let home = dirs::home_dir()
                                            .unwrap_or_else(|| self.cfg.data_dir.clone());
                                        let mimir_bin = std::env::current_exe()
                                            .ok()
                                            .and_then(|exe| {
                                                let dir = exe.parent()?;
                                                let bundled = dir.join("mimir-cli");
                                                if bundled.is_file() {
                                                    Some(bundled)
                                                } else {
                                                    let sibling = dir.join("mimir");
                                                    if sibling.is_file() {
                                                        Some(sibling)
                                                    } else {
                                                        None
                                                    }
                                                }
                                            })
                                            .unwrap_or_else(|| std::path::PathBuf::from("mimir"));
                                        match connect_detected_assistants(&home, &mimir_bin, false)
                                        {
                                            Ok(outcomes) if outcomes.is_empty() => {
                                                self.connect_report =
                                                    "No supported MCP assistant config files detected in your home directory.".into();
                                            }
                                            Ok(outcomes) => {
                                                self.connect_report = outcomes
                                                    .into_iter()
                                                    .map(|o| {
                                                        format!(
                                                            "[{}] {} → {}",
                                                            o.status,
                                                            o.name,
                                                            o.path.display()
                                                        )
                                                    })
                                                    .collect::<Vec<_>>()
                                                    .join("\n");
                                                self.status_line =
                                                    "Connected detected MCP assistants.".into();
                                            }
                                            Err(e) => {
                                                self.connect_report = format!("Error: {e}");
                                            }
                                        }
                                    }
                                },
                            );
                        });
                        if !self.connect_report.is_empty() {
                            ui.add_space(10.0);
                            egui::Frame::new()
                                .fill(palette::VOID_BG)
                                .stroke(egui::Stroke::new(1.0_f32, hairline))
                                .corner_radius(egui::CornerRadius::same(7))
                                .inner_margin(egui::Margin::same(10))
                                .show(ui, |ui| {
                                    ui.set_min_width(ui.available_width());
                                    ui.monospace(&self.connect_report);
                                });
                        }
                    });

                    ui.add_space(12.0);

                    section_card(ui, "LOCAL AI MODELS & UPDATES", None, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(
                                    "All speech recognition, diarization, and transcript cleanup models run 100% locally on your hardware.",
                                )
                                .size(12.5)
                                .color(palette::TEXT_SECONDARY),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if action_button(
                                        ui,
                                        "Check for App Updates…",
                                        ButtonVariant::Secondary,
                                    )
                                    .clicked()
                                    {
                                        let tx = self.event_tx.clone();
                                        self.update_line = "Checking GitHub Releases…".into();
                                        std::thread::spawn(move || {
                                            let res =
                                                check_for_updates(env!("CARGO_PKG_VERSION"));
                                            let _ = tx.send(UiEvent::UpdateResult(res));
                                        });
                                    }
                                },
                            );
                        });
                        if !self.update_line.is_empty() {
                            ui.add_space(6.0);
                            ui.label(egui::RichText::new(&self.update_line).color(ice));
                        }
                        ui.add_space(10.0);
                        for m in model_statuses(&self.cfg) {
                            egui::Frame::new()
                                .fill(palette::VOID_BG)
                                .stroke(egui::Stroke::new(1.0_f32, hairline))
                                .corner_radius(egui::CornerRadius::same(7))
                                .inner_margin(egui::Margin::symmetric(12, 10))
                                .show(ui, |ui| {
                                    ui.set_min_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new(&m.name)
                                                .strong()
                                                .color(text_primary),
                                        );
                                        if m.installed {
                                            badge_pill(
                                                ui,
                                                &format!(
                                                    "Installed · {:.1} MB",
                                                    m.size_bytes as f64 / (1024.0 * 1024.0)
                                                ),
                                                palette::SUCCESS,
                                                palette::SUCCESS_BG,
                                            );
                                        } else {
                                            badge_pill(
                                                ui,
                                                &format!(
                                                    "Not installed · {}",
                                                    m.expected_size_label
                                                ),
                                                text_muted,
                                                palette::CARD_BG,
                                            );
                                        }
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if m.installed
                                                    && action_button(
                                                        ui,
                                                        "Delete",
                                                        ButtonVariant::Danger,
                                                    )
                                                    .clicked()
                                                {
                                                    let res = match m.id.as_str() {
                                                        "whistle" => std::fs::remove_file(
                                                            resolve_weights_path(&self.cfg),
                                                        )
                                                        .map_err(Into::into),
                                                        "qwen" => delete_qwen_model(&self.cfg),
                                                        "silero_vad" => {
                                                            delete_silero_vad(&self.cfg)
                                                        }
                                                        "speaker_embed" => {
                                                            delete_speaker_embed(&self.cfg)
                                                        }
                                                        _ => Ok(()),
                                                    };
                                                    self.status_line = match res {
                                                        Ok(()) => format!("Deleted {}", m.name),
                                                        Err(e) => format!("Delete error: {e}"),
                                                    };
                                                }
                                                let btn_lbl = if m.installed {
                                                    "Verify"
                                                } else {
                                                    "Download"
                                                };
                                                let var = if m.installed {
                                                    ButtonVariant::Secondary
                                                } else {
                                                    ButtonVariant::Primary
                                                };
                                                if action_button(ui, btn_lbl, var).clicked() {
                                                    let cfg_clone = self.cfg.clone();
                                                    let tx = self.event_tx.clone();
                                                    let id = m.id.clone();
                                                    self.status_line =
                                                        format!("Downloading {}…", m.name);
                                                    std::thread::spawn(move || {
                                                        let msg = match id.as_str() {
                                                            "whistle" => match fetch_whistle_weights(
                                                                &resolve_weights_path(&cfg_clone),
                                                            ) {
                                                                Ok(p) => format!(
                                                                    "Verified whistle.cact at {}",
                                                                    p.display()
                                                                ),
                                                                Err(e) => {
                                                                    format!("Fetch error: {e}")
                                                                }
                                                            },
                                                            "qwen" => {
                                                                match fetch_qwen_model(&cfg_clone) {
                                                                    Ok((w, _)) => format!(
                                                                        "Verified Qwen 2.5 0.5B at {}",
                                                                        w.display()
                                                                    ),
                                                                    Err(e) => {
                                                                        format!("Fetch error: {e}")
                                                                    }
                                                                }
                                                            }
                                                            "silero_vad" => {
                                                                match fetch_silero_vad(&cfg_clone) {
                                                                    Ok(p) => format!(
                                                                        "Verified silero_vad.onnx at {}",
                                                                        p.display()
                                                                    ),
                                                                    Err(e) => {
                                                                        format!("Fetch error: {e}")
                                                                    }
                                                                }
                                                            }
                                                            "speaker_embed" => {
                                                                match fetch_speaker_embed(
                                                                    &cfg_clone,
                                                                ) {
                                                                    Ok(p) => format!(
                                                                        "Verified speaker_embed.onnx at {}",
                                                                        p.display()
                                                                    ),
                                                                    Err(e) => {
                                                                        format!("Fetch error: {e}")
                                                                    }
                                                                }
                                                            }
                                                            _ => "Unknown model".into(),
                                                        };
                                                        let _ = tx.send(UiEvent::Status(msg));
                                                    });
                                                }
                                            },
                                        );
                                    });
                                });
                            ui.add_space(6.0);
                        }
                    });
                }
            });
    }
}

impl eframe::App for MimirDesktopApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_recording();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

        if ctx.input(|i| i.modifiers.command && i.modifiers.shift && i.key_pressed(egui::Key::M)) {
            if self.is_recording {
                self.stop_recording();
            } else {
                self.start_recording();
            }
        }
        if ctx.input(|i| i.modifiers.command && i.modifiers.shift && i.key_pressed(egui::Key::C))
            && !self.selected_markdown.is_empty()
        {
            ctx.copy_text(self.selected_markdown.clone());
            self.status_line = "Copied transcript to clipboard.".into();
        }

        if self.is_recording {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        } else {
            if self.last_store_poll.elapsed() >= std::time::Duration::from_secs(2) {
                self.last_store_poll = std::time::Instant::now();
                let stamp = db_modified_stamp(&self.cfg.data_dir);
                if stamp != self.last_db_stamp {
                    self.last_db_stamp = stamp;
                    self.refresh_sessions();
                    if let Some(sid) = self.selected_session_id {
                        self.load_session(sid);
                    }
                }
            }
            if self.last_calendar_poll.elapsed() >= std::time::Duration::from_secs(60) {
                self.last_calendar_poll = std::time::Instant::now();
                self.refresh_calendar_events();
            }
            ctx.request_repaint_after(std::time::Duration::from_secs(2));
        }

        if self.mini_mode {
            self.render_mini_mode(ctx);
            return;
        }

        // Subtle Status Footer
        egui::TopBottomPanel::bottom("workspace_status_footer")
            .frame(
                egui::Frame::new()
                    .fill(palette::RAIL_BG)
                    .stroke(egui::Stroke::new(1.0_f32, palette::HAIRLINE))
                    .inner_margin(egui::Margin::symmetric(16, 6)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(&self.status_line)
                            .size(11.5)
                            .color(palette::TEXT_SECONDARY),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new("Cmd/Ctrl+Shift+M: Record · Cmd/Ctrl+Shift+C: Copy")
                                .monospace()
                                .size(10.5)
                                .color(palette::TEXT_MUTED),
                        );
                    });
                });
            });

        // Left Navigation & Sessions Sidebar
        self.render_sidebar(ctx);

        // Main Workspace Canvas
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(palette::VOID_BG)
                    .inner_margin(egui::Margin::same(18)),
            )
            .show(ctx, |ui| match self.mode {
                WorkspaceMode::Capture => self.render_capture_canvas(ui),
                WorkspaceMode::SessionView => self.render_session_canvas(ui, ctx),
                WorkspaceMode::Settings => self.render_settings_canvas(ui),
            });
    }
}

fn render_turn_card(
    ui: &mut egui::Ui,
    speaker_label: &str,
    channel: ChannelId,
    start_ms: u64,
    end_ms: u64,
    text: &str,
    diff_raw: Option<&str>,
) {
    let accent = match channel {
        ChannelId::Mic => palette::CYAN,
        ChannelId::System => palette::AZURE,
    };
    egui::Frame::new()
        .fill(palette::VOID_BG)
        .stroke(egui::Stroke::new(1.0_f32, palette::HAIRLINE))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                let (bar_rect, _) =
                    ui.allocate_exact_size(egui::vec2(3.0, 16.0), egui::Sense::hover());
                ui.painter().rect_filled(bar_rect, 1.5, accent);
                ui.label(
                    egui::RichText::new(speaker_label)
                        .strong()
                        .size(12.5)
                        .color(accent),
                );
                let ch_tag = match channel {
                    ChannelId::Mic => "Mic",
                    ChannelId::System => "Remote",
                };
                ui.label(
                    egui::RichText::new(format!(
                        "· {ch_tag} · {} – {}",
                        format_ms_timestamp(start_ms),
                        format_ms_timestamp(end_ms)
                    ))
                    .monospace()
                    .size(10.5)
                    .color(palette::TEXT_MUTED),
                );
            });
            if let Some(raw) = diff_raw {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(raw)
                        .strikethrough()
                        .size(12.5)
                        .color(palette::TEXT_MUTED),
                );
            }
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(text)
                    .size(13.5)
                    .color(palette::TEXT_PRIMARY),
            );
        });
}

fn format_ms_timestamp(ms: u64) -> String {
    let total_secs = ms / 1000;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{mins:02}:{secs:02}")
}

fn format_duration_secs(secs: u64) -> String {
    let mins = secs / 60;
    let rem = secs % 60;
    format!("{mins:02}:{rem:02}")
}

fn format_duration_compact(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        let mins = secs / 60;
        let rem = secs % 60;
        if rem == 0 {
            format!("{mins}m")
        } else {
            format!("{mins}m {rem}s")
        }
    }
}

fn format_calendar_source_label(source: &str) -> &'static str {
    match source {
        "apple_calendar" => "APPLE CALENDAR",
        "outlook" => "OUTLOOK",
        "windows_calendar" => "WINDOWS CALENDAR",
        "gnome_evolution" => "GNOME CALENDAR",
        "kde_korganizer" => "KDE KORGANIZER",
        "thunderbird" => "THUNDERBIRD",
        s if s.starts_with("caldav:") => "CALDAV",
        s if s.starts_with("google:") => "GOOGLE CALENDAR",
        s if s.starts_with("microsoft:") => "MICROSOFT 365",
        _ => "CALENDAR FEED",
    }
}

fn draw_grayscale_mimir_mark(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let s = size / 512.0;
    let ox = rect.min.x;
    let oy = rect.min.y;
    let dark = ui.visuals().dark_mode;
    let tone = |a: u8| {
        if dark {
            egui::Color32::from_rgba_unmultiplied(241, 245, 249, a)
        } else {
            egui::Color32::from_rgba_unmultiplied(15, 23, 42, a)
        }
    };
    let painter = ui.painter();
    let poly = |pts: &[(f32, f32)], col: egui::Color32| {
        let mapped = pts
            .iter()
            .map(|&(x, y)| egui::pos2(ox + x * s, oy + y * s))
            .collect();
        painter.add(egui::Shape::convex_polygon(
            mapped,
            col,
            egui::Stroke::NONE,
        ));
    };
    poly(
        &[
            (92.0, 92.0),
            (156.0, 92.0),
            (256.0, 192.0),
            (256.0, 268.0),
            (92.0, 104.0),
        ],
        tone(240),
    );
    poly(
        &[
            (256.0, 192.0),
            (356.0, 92.0),
            (420.0, 92.0),
            (420.0, 104.0),
            (256.0, 268.0),
        ],
        tone(240),
    );
    poly(
        &[(92.0, 125.21), (156.0, 189.21), (156.0, 344.23), (92.0, 394.15)],
        tone(190),
    );
    poly(
        &[
            (356.0, 189.21),
            (420.0, 125.21),
            (420.0, 394.15),
            (356.0, 344.23),
        ],
        tone(150),
    );
    poly(
        &[(92.0, 413.17), (156.0, 363.25), (156.0, 420.0), (92.0, 420.0)],
        tone(115),
    );
    poly(
        &[
            (356.0, 363.25),
            (420.0, 413.17),
            (420.0, 420.0),
            (356.0, 420.0),
        ],
        tone(115),
    );
}

fn draw_level_meter(
    ui: &mut egui::Ui,
    label: &str,
    level: f32,
    active_col: egui::Color32,
    inactive_col: egui::Color32,
    label_col: egui::Color32,
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        ui.label(
            egui::RichText::new(label)
                .monospace()
                .size(9.5)
                .color(label_col),
        );
        ui.add_space(2.0);
        let bars = 6usize;
        let lit = (level * bars as f32).ceil() as usize;
        for i in 0..bars {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(3.0, 10.0), egui::Sense::hover());
            let col = if i < lit { active_col } else { inactive_col };
            ui.painter().rect_filled(rect, 1.0, col);
        }
    });
}

fn write_wav_i16(
    path: &std::path::Path,
    samples: &[i16],
    sample_rate: u32,
) -> std::io::Result<()> {
    use std::io::Write;
    let data_bytes = (samples.len() * 2) as u32;
    let riff_size = 36u32.saturating_add(data_bytes);
    let byte_rate = sample_rate * 2;
    let mut f = std::fs::File::create(path)?;
    f.write_all(b"RIFF")?;
    f.write_all(&riff_size.to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?;
    f.write_all(&sample_rate.to_le_bytes())?;
    f.write_all(&byte_rate.to_le_bytes())?;
    f.write_all(&2u16.to_le_bytes())?;
    f.write_all(&16u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data_bytes.to_le_bytes())?;
    for s in samples {
        f.write_all(&s.to_le_bytes())?;
    }
    Ok(())
}

fn save_pcm_wav_maybe_encrypted(
    data_dir: &std::path::Path,
    audio_dir: &std::path::Path,
    stem: &str,
    samples: &[i16],
    encrypt: bool,
) {
    let wav_path = audio_dir.join(format!("{stem}.wav"));
    if write_wav_i16(&wav_path, samples, 16_000).is_ok()
        && encrypt
        && let Ok(raw) = std::fs::read(&wav_path)
        && let Ok(enc) = encrypt_bytes_at_rest(data_dir, &raw)
        && std::fs::write(audio_dir.join(format!("{stem}.wav.enc")), enc).is_ok()
    {
        let _ = std::fs::remove_file(&wav_path);
    }
}

fn db_modified_stamp(data_dir: &std::path::Path) -> Option<std::time::SystemTime> {
    let db = std::fs::metadata(data_dir.join("mimir.db"))
        .and_then(|m| m.modified())
        .ok();
    let wal = std::fs::metadata(data_dir.join("mimir.db-wal"))
        .and_then(|m| m.modified())
        .ok();
    match (db, wal) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn main() -> eframe::Result<()> {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!(
        "../../../assets/brand/mimir-icon-64.png"
    ))
    .ok();
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1080.0, 700.0])
        .with_min_inner_size([760.0, 520.0])
        .with_title("Mimir");
    if let Some(ic) = icon {
        viewport = viewport.with_icon(std::sync::Arc::new(ic));
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "Mimir",
        options,
        Box::new(|cc| Ok(Box::new(MimirDesktopApp::new(cc)))),
    )
}

fn render_markdown_view(
    ui: &mut egui::Ui,
    markdown: &str,
    ice: egui::Color32,
    cyan: egui::Color32,
    text_primary: egui::Color32,
    text_muted: egui::Color32,
    hairline: egui::Color32,
) {
    let mut in_code = false;
    let mut code_buf = String::new();
    let code_bg = palette::VOID_BG;

    for raw_line in markdown.lines() {
        let trimmed = raw_line.trim();
        if trimmed.starts_with("```") {
            if in_code {
                egui::Frame::new()
                    .fill(code_bg)
                    .stroke(egui::Stroke::new(1.0_f32, hairline))
                    .corner_radius(6)
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(code_buf.trim_end())
                                .monospace()
                                .size(12.0)
                                .color(text_primary),
                        );
                    });
                ui.add_space(4.0);
                code_buf.clear();
                in_code = false;
            } else {
                in_code = true;
            }
            continue;
        }
        if in_code {
            code_buf.push_str(raw_line);
            code_buf.push('\n');
            continue;
        }
        if trimmed.is_empty() {
            ui.add_space(4.0);
            continue;
        }
        if trimmed == "---" || trimmed == "***" || trimmed == "___" {
            ui.separator();
            continue;
        }

        let hashes = trimmed.chars().take_while(|&c| c == '#').count();
        if (1..=4).contains(&hashes) && trimmed.chars().nth(hashes) == Some(' ') {
            let heading_text = trimmed[hashes..].trim();
            let size = match hashes {
                1 => 18.0,
                2 => 15.0,
                _ => 13.5,
            };
            let color = if hashes <= 2 { ice } else { text_primary };
            ui.add_space(if hashes == 1 { 2.0 } else { 5.0 });
            ui.label(
                egui::RichText::new(heading_text)
                    .strong()
                    .size(size)
                    .color(color),
            );
            continue;
        }

        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            ui.horizontal_wrapped(|ui| {
                ui.add_space(6.0);
                ui.label(egui::RichText::new("•").strong().color(cyan));
                ui.label(build_inline_markdown_job(
                    rest,
                    13.5,
                    text_primary,
                    ice,
                    code_bg,
                ));
            });
            continue;
        }

        if let Some((num, rest)) = trimmed.split_once(". ")
            && !num.is_empty()
            && num.chars().all(|c| c.is_ascii_digit())
        {
            ui.horizontal_wrapped(|ui| {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(format!("{num}."))
                        .monospace()
                        .strong()
                        .color(cyan),
                );
                ui.label(build_inline_markdown_job(
                    rest,
                    13.5,
                    text_primary,
                    ice,
                    code_bg,
                ));
            });
            continue;
        }

        if let Some(rest) = trimmed
            .strip_prefix("> ")
            .or_else(|| (trimmed == ">").then_some(""))
        {
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(12, 20, 34))
                .stroke(egui::Stroke::new(1.0_f32, hairline))
                .corner_radius(4)
                .inner_margin(egui::Margin::symmetric(10, 5))
                .show(ui, |ui| {
                    ui.label(build_inline_markdown_job(
                        rest,
                        12.5,
                        text_muted,
                        ice,
                        code_bg,
                    ));
                });
            continue;
        }

        ui.label(build_inline_markdown_job(
            raw_line,
            13.5,
            text_primary,
            ice,
            code_bg,
        ));
    }

    if in_code && !code_buf.is_empty() {
        egui::Frame::new()
            .fill(code_bg)
            .stroke(egui::Stroke::new(1.0_f32, hairline))
            .corner_radius(6)
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(code_buf.trim_end())
                        .monospace()
                        .size(12.0)
                        .color(text_primary),
                );
            });
    }
}

fn build_inline_markdown_job(
    text: &str,
    size: f32,
    base_color: egui::Color32,
    accent: egui::Color32,
    code_bg: egui::Color32,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut plain_start = 0usize;

    let push_plain = |job: &mut egui::text::LayoutJob, slice: &str| {
        if !slice.is_empty() {
            job.append(
                slice,
                0.0,
                egui::TextFormat {
                    font_id: egui::FontId::proportional(size),
                    color: base_color,
                    ..Default::default()
                },
            );
        }
    };

    while i < bytes.len() {
        if bytes[i..].starts_with(b"**")
            && let Some(end) = text[i + 2..].find("**")
        {
            push_plain(&mut job, &text[plain_start..i]);
            job.append(
                &text[i + 2..i + 2 + end],
                0.0,
                egui::TextFormat {
                    font_id: egui::FontId::proportional(size),
                    color: egui::Color32::WHITE,
                    ..Default::default()
                },
            );
            i = i + 2 + end + 2;
            plain_start = i;
            continue;
        }
        if bytes[i] == b'`'
            && let Some(end) = text[i + 1..].find('`')
        {
            push_plain(&mut job, &text[plain_start..i]);
            job.append(
                &text[i + 1..i + 1 + end],
                0.0,
                egui::TextFormat {
                    font_id: egui::FontId::monospace(size - 0.5),
                    color: accent,
                    background: code_bg,
                    ..Default::default()
                },
            );
            i = i + 1 + end + 1;
            plain_start = i;
            continue;
        }
        if bytes[i] == b'*'
            && !bytes[i..].starts_with(b"**")
            && let Some(end) = text[i + 1..].find('*')
        {
            push_plain(&mut job, &text[plain_start..i]);
            job.append(
                &text[i + 1..i + 1 + end],
                0.0,
                egui::TextFormat {
                    font_id: egui::FontId::proportional(size),
                    color: base_color,
                    italics: true,
                    ..Default::default()
                },
            );
            i = i + 1 + end + 1;
            plain_start = i;
            continue;
        }
        let ch_len = text[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        i += ch_len;
    }
    push_plain(&mut job, &text[plain_start..]);
    job
}

#[cfg(target_os = "macos")]
fn spawn_macos_sc_tap(
    tx: mpsc::Sender<mimir_audio::CapturedFrame>,
    stop_flag: Arc<AtomicBool>,
    tx_ui: mpsc::Sender<UiEvent>,
) -> Option<std::process::Child> {
    use std::io::{BufRead, BufReader, Read};
    use std::process::{Command, Stdio};

    let tap_bin = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("mimir-sc-tap")))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| std::path::PathBuf::from("mimir-sc-tap"));

    let mut child = match Command::new(&tap_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = tx_ui.send(UiEvent::Status(format!(
                "System audio tap unavailable ({}): {e}",
                tap_bin.display()
            )));
            return None;
        }
    };

    if let Some(stderr) = child.stderr.take() {
        let tx_err = tx_ui.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    let _ = tx_err.send(UiEvent::Status(trimmed.to_string()));
                }
            }
        });
    }

    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        return None;
    };

    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut rem: Vec<u8> = Vec::new();
        while !stop_flag.load(Ordering::Relaxed) {
            match stdout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    rem.extend_from_slice(&buf[..n]);
                    let full_bytes = (rem.len() / 4) * 4;
                    if full_bytes > 0 {
                        let mut samples = Vec::with_capacity(full_bytes / 4);
                        for chunk in rem[..full_bytes].chunks_exact(4) {
                            let s = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                            if s.is_finite() {
                                samples.push(s);
                            } else {
                                samples.push(0.0);
                            }
                        }
                        rem.drain(..full_bytes);
                        if tx
                            .send(mimir_audio::CapturedFrame {
                                channel: ChannelId::System,
                                samples,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        }
    });

    Some(child)
}
