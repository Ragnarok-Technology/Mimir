mod ui_kit;
mod updater;

use chrono::Utc;
use eframe::egui;
use ui_kit::{
    ButtonVariant, action_button, apply_obsidian_prism_theme, badge_pill, empty_state, palette,
    pill_tab, section_card, session_card_row, styled_text_input, toggle_switch,
};
use mimir_audio::{BleedGate, VadChunker, start_dual_capture};
use mimir_bridge::connect::connect_detected_assistants;
use mimir_bridge::mcp::format_live_utterance_md;
use mimir_core::calendar::{
    CalendarEvent, open_url_in_system_browser, parse_ics, upcoming_or_active_events,
};
use mimir_core::{
    ActionItem, AudioRetention, ChannelId, KeywordPreset, MimirConfig, Session, Utterance,
};
use mimir_diarize::{Diarizer, DiarizerConfig, insert_utterance_chronological, mean_pool_frames};
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
    delete_qwen_model, delete_silero_vad, delete_speaker_embed, fetch_qwen_model,
    fetch_silero_vad, fetch_speaker_embed, fetch_whistle_weights, model_statuses,
    resolve_lib_path, resolve_qwen_paths, resolve_weights_path,
};
use mimir_whistle::qwen::QwenCleanupModel;
use mimir_whistle::{TranscribeOptions, WhistleEngine};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use updater::{UpdateStatus, check_for_updates};
use uuid::Uuid;

#[derive(PartialEq, Eq)]
enum Tab {
    Live,
    Transcripts,
    Settings,
}

enum UiEvent {
    LiveLine(String),
    Status(String),
    CleanupProgress(usize, usize),
    CleanupDone,
    CalendarEvents(Vec<CalendarEvent>),
    SessionFinished,
    UpdateResult(UpdateStatus),
}

struct MimirDesktopApp {
    cfg: MimirConfig,
    tab: Tab,
    mini_mode: bool,
    session_title: String,
    keywords_input: String,
    preset_name_input: String,
    selected_preset_name: Option<String>,
    diarize_mic: bool,
    save_wav: bool,
    encrypt_at_rest: bool,
    is_recording: bool,
    stop_flag: Option<Arc<AtomicBool>>,
    mic_level: Arc<AtomicU32>,
    sys_level: Arc<AtomicU32>,
    live_lines: Vec<String>,
    sessions: Vec<SessionSummary>,
    show_archived: bool,
    selected_session_id: Option<Uuid>,
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
    speaker_id_edit: String,
    speaker_label_edit: String,
    status_line: String,
    cleanup_progress: Option<(usize, usize)>,
    calendar_events: Vec<CalendarEvent>,
    calendar_ics_input: String,
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
            tab: Tab::Live,
            mini_mode: false,
            session_title: String::new(),
            keywords_input,
            preset_name_input: String::new(),
            selected_preset_name: None,
            diarize_mic,
            save_wav,
            encrypt_at_rest,
            is_recording: false,
            stop_flag: None,
            mic_level: Arc::new(AtomicU32::new(0.0f32.to_bits())),
            sys_level: Arc::new(AtomicU32::new(0.0f32.to_bits())),
            live_lines: Vec::new(),
            sessions: Vec::new(),
            show_archived: false,
            selected_session_id: None,
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
            speaker_id_edit: String::new(),
            speaker_label_edit: String::new(),
            status_line: "Ready — 100% Local • Whistle AI • Zero Telemetry".to_string(),
            cleanup_progress: None,
            calendar_events: Vec::new(),
            calendar_ics_input: String::new(),
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

    fn refresh_sessions(&mut self) {
        if let Ok(store) = SessionStore::open(&self.cfg.data_dir)
            && let Ok(list) = store.list_sessions_filtered(50, self.show_archived)
        {
            self.sessions = list;
            if let Some(sel) = self.selected_session_id {
                if let Some(s) = self.sessions.iter().find(|s| s.id == sel) {
                    self.selected_title_edit = s.title.clone().unwrap_or_default();
                } else if let Some(first) = self.sessions.first() {
                    self.load_session(first.id);
                } else {
                    self.selected_session_id = None;
                    self.selected_markdown.clear();
                    self.selected_title_edit.clear();
                    self.selected_summary = None;
                    self.selected_summary_template = None;
                    self.selected_action_items.clear();
                    self.selected_has_cleaned = false;
                    self.selected_has_summary = false;
                    self.show_summary_panel = false;
                }
            } else if let Some(first) = self.sessions.first() {
                self.load_session(first.id);
            }
        }
    }

    fn load_session(&mut self, id: Uuid) {
        self.selected_session_id = Some(id);
        self.confirm_delete_id = None;
        if let Some(s) = self.sessions.iter().find(|s| s.id == id) {
            self.selected_title_edit = s.title.clone().unwrap_or_default();
        }
        if let Ok(store) = SessionStore::open(&self.cfg.data_dir)
            && let Ok(Some(session)) = store.get_session(id)
        {
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
        }
    }

    fn rename_selected_session(&mut self) {
        if let Some(id) = self.selected_session_id
            && let Ok(store) = SessionStore::open(&self.cfg.data_dir)
            && store.rename_session(id, &self.selected_title_edit).unwrap_or(false)
        {
            self.status_line = "Renamed session.".into();
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
                    "Unarchived session.".into()
                };
                if next_archived && !self.show_archived {
                    self.selected_session_id = None;
                    self.selected_markdown.clear();
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
            self.selected_session_id = None;
            self.selected_markdown.clear();
            self.selected_title_edit.clear();
            self.status_line = "Deleted session permanently.".into();
            self.refresh_sessions();
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
            self.status_line = "Missing whistle.cact — click Fetch Model in Assistants tab".into();
            return;
        }

        let stop = Arc::new(AtomicBool::new(false));
        self.stop_flag = Some(stop.clone());
        self.is_recording = true;
        self.live_lines.clear();
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

                let (audio_tx, audio_rx) = mpsc::channel();
                #[cfg(target_os = "macos")]
                let mut sc_tap_child: Option<std::process::Child> = if cfg.capture_system_audio {
                    spawn_macos_sc_tap(audio_tx.clone(), stop.clone(), tx_ui.clone())
                } else {
                    None
                };
                let mut capture = start_dual_capture(audio_tx)?;

                let mut bleed_gate = BleedGate::default_20ms();
                let mut mic_vad = VadChunker::new(ChannelId::Mic, 0.015);
                let mut sys_vad = VadChunker::new(ChannelId::System, 0.012);
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
                                ChannelId::Mic => mic_level.store(norm.to_bits(), Ordering::Relaxed),
                                ChannelId::System => sys_level.store(norm.to_bits(), Ordering::Relaxed),
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
                                    let emb = engine.embed(&chunk.samples).ok().and_then(|v| {
                                        if v.len() <= 512 {
                                            Some(v)
                                        } else {
                                            mean_pool_frames(&v, 384).or(Some(v))
                                        }
                                    });
                                    let words = res.into_word_tokens(chunk.start_ms);
                                    let end_ms = words.last().map(|w| w.end_ms).unwrap_or(
                                        chunk.start_ms + (chunk.samples.len() as u64 * 1000 / 16_000),
                                    );
                                    let spk = diarizer.assign_speaker(chunk.channel, emb.as_deref());
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
                                    insert_utterance_chronological(&mut session.utterances, utt.clone());
                                    let _ = tx_ui.send(UiEvent::LiveLine(format_live_utterance_md(
                                        &spk.label,
                                        &utt,
                                    )));
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
                    let _ = tx_ui.send(UiEvent::CleanupDone);
                }
                let _ = sync_session_to_drop_dir_if_enabled(&store, session.id, &cfg);
                Ok(())
            })();

            if let Err(e) = result {
                let _ = tx_ui.send(UiEvent::Status(format!("Recording error: {e}")));
            }
            let _ = tx_ui.send(UiEvent::SessionFinished);
        });
    }

    fn stop_recording(&mut self) {
        if let Some(flag) = &self.stop_flag {
            flag.store(true, Ordering::Relaxed);
        }
        self.mic_level.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.sys_level.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.is_recording = false;
        self.status_line = "Stopping & saving session…".into();
    }

    fn refresh_calendar_events(&self) {
        let sources = self.cfg.calendar_ics_urls.clone();
        if sources.is_empty() {
            return;
        }
        let tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let mut all = Vec::new();
            for src in sources {
                let trimmed = src.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let content = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                    ureq::get(trimmed)
                        .call()
                        .ok()
                        .and_then(|mut r| r.body_mut().read_to_string().ok())
                } else {
                    std::fs::read_to_string(trimmed).ok()
                };
                if let Some(ics) = content {
                    all.extend(parse_ics(&ics, trimmed));
                }
            }
            let upcoming = upcoming_or_active_events(&all, Utc::now().timestamp(), 900);
            let _ = tx.send(UiEvent::CalendarEvents(upcoming));
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

    fn run_search(&mut self) {
        let q = self.search_query.trim();
        if q.is_empty() {
            self.search_results.clear();
            return;
        }
        if let Ok(store) = SessionStore::open(&self.cfg.data_dir)
            && let Ok(hits) = store.search(q, 20)
        {
            self.search_results = hits;
            self.status_line = format!("Found {} matching utterance(s).", self.search_results.len());
        }
    }

    fn rename_speaker_inline(&mut self, session_id: Uuid) {
        let spk_id = self.speaker_id_edit.trim().to_string();
        let new_lbl = self.speaker_label_edit.trim().to_string();
        if spk_id.is_empty() || new_lbl.is_empty() {
            return;
        }
        if let Ok(store) = SessionStore::open(&self.cfg.data_dir)
            && store.rename_speaker(session_id, &spk_id, &new_lbl).unwrap_or(false)
        {
            self.status_line = format!("Renamed {spk_id} -> {new_lbl}");
            self.speaker_id_edit.clear();
            self.speaker_label_edit.clear();
            self.load_session(session_id);
        }
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.event_rx.try_recv() {
            match ev {
                UiEvent::LiveLine(line) => self.live_lines.push(line),
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
                UiEvent::CalendarEvents(events) => {
                    self.calendar_events = events;
                }
                UiEvent::SessionFinished => {
                    self.is_recording = false;
                    self.status_line = "Session saved locally.".into();
                    self.refresh_sessions();
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
            if !self.cfg.calendar_ics_urls.is_empty()
                && self.last_calendar_poll.elapsed() >= std::time::Duration::from_secs(60)
            {
                self.last_calendar_poll = std::time::Instant::now();
                self.refresh_calendar_events();
            }
            ctx.request_repaint_after(std::time::Duration::from_secs(2));
        }

        let rail_bg = egui::Color32::from_rgb(8, 12, 20);      // #080C14
        let card_bg = egui::Color32::from_rgb(11, 16, 26);     // #0B101A
        let active_bg = egui::Color32::from_rgb(12, 27, 46);   // #0C1B2E
        let hairline = egui::Color32::from_rgb(24, 36, 54);    // #182436
        let ice = egui::Color32::from_rgb(103, 232, 249);      // #67E8F9
        let cyan = egui::Color32::from_rgb(56, 189, 248);      // #38BDF8
        let azure = egui::Color32::from_rgb(14, 165, 233);     // #0EA5E9
        let text_primary = egui::Color32::from_rgb(226, 232, 240); // #E2E8F0
        let text_muted = egui::Color32::from_rgb(100, 116, 139);   // #64748B

        egui::TopBottomPanel::top("top_telemetry")
            .frame(
                egui::Frame::new()
                    .fill(rail_bg)
                    .stroke(egui::Stroke::new(1.0, hairline))
                    .inner_margin(egui::Margin::symmetric(16, 10)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    draw_grayscale_mimir_mark(ui, 20.0);
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("M I M I R").strong().size(13.5).color(text_primary));
                    ui.add_space(8.0);
                    if self.is_recording {
                        badge_pill(ui, "● LIVE", ice, active_bg);
                    } else {
                        badge_pill(ui, "● STANDBY", cyan, palette::VOID_BG);
                    }
                    ui.add_space(10.0);
                    let mic_lvl = f32::from_bits(self.mic_level.load(Ordering::Relaxed));
                    let sys_lvl = f32::from_bits(self.sys_level.load(Ordering::Relaxed));
                    draw_level_meter(ui, "MIC", mic_lvl, cyan, hairline, text_muted);
                    ui.add_space(8.0);
                    draw_level_meter(ui, "SYS", sys_lvl, azure, hairline, text_muted);
                    ui.add_space(14.0);
                    if !self.mini_mode {
                        egui::Frame::new()
                            .fill(palette::VOID_BG)
                            .stroke(egui::Stroke::new(1.0, hairline))
                            .corner_radius(egui::CornerRadius::same(8))
                            .inner_margin(egui::Margin::symmetric(4, 3))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    if pill_tab(ui, "Live", self.tab == Tab::Live).clicked() {
                                        self.tab = Tab::Live;
                                    }
                                    if pill_tab(ui, "Transcripts", self.tab == Tab::Transcripts).clicked() {
                                        self.tab = Tab::Transcripts;
                                    }
                                    if pill_tab(ui, "Settings", self.tab == Tab::Settings).clicked() {
                                        self.tab = Tab::Settings;
                                    }
                                });
                            });
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if action_button(ui, "Hide", ButtonVariant::Ghost).clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                        }
                        let mini_label = if self.mini_mode { "Full View" } else { "Mini" };
                        if action_button(ui, mini_label, ButtonVariant::Ghost).clicked() {
                            self.mini_mode = !self.mini_mode;
                            if self.mini_mode {
                                self.tab = Tab::Live;
                            }
                        }
                        if action_button(ui, "Copy Latest", ButtonVariant::Secondary).clicked() {
                            if let Ok(store) = SessionStore::open(&self.cfg.data_dir)
                                && let Ok(list) = store.list_sessions_filtered(1, false)
                                && let Some(latest) = list.first()
                                && let Ok(Some(sess)) = store.get_session(latest.id)
                            {
                                let md = render_markdown_with_view(&sess, TranscriptView::Cleaned);
                                ctx.copy_text(md);
                                self.status_line = format!(
                                    "Copied '{}' to clipboard.",
                                    sess.title.as_deref().unwrap_or("Untitled")
                                );
                            } else {
                                self.status_line = "No sessions available to copy.".into();
                            }
                        }
                        if action_button(ui, "Connect AI", ButtonVariant::Secondary).clicked() {
                            let home = dirs::home_dir().unwrap_or_else(|| self.cfg.data_dir.clone());
                            let mimir_bin = std::env::current_exe()
                                .ok()
                                .and_then(|exe| {
                                    let dir = exe.parent()?;
                                    let bundled = dir.join("mimir-cli");
                                    if bundled.is_file() {
                                        Some(bundled)
                                    } else {
                                        let sibling = dir.join("mimir");
                                        if sibling.is_file() { Some(sibling) } else { None }
                                    }
                                })
                                .unwrap_or_else(|| std::path::PathBuf::from("mimir"));
                            match connect_detected_assistants(&home, &mimir_bin, false) {
                                Ok(outcomes) if outcomes.is_empty() => {
                                    self.status_line = "No supported AI assistant configs found.".into();
                                }
                                Ok(outcomes) => {
                                    let names: Vec<String> = outcomes
                                        .into_iter()
                                        .map(|o| format!("{} ({})", o.name, o.status))
                                        .collect();
                                    self.status_line = format!("MCP connected: {}", names.join(", "));
                                }
                                Err(e) => {
                                    self.status_line = format!("MCP connect failed: {e}");
                                }
                            }
                        }
                    });
                });
            });

        egui::TopBottomPanel::bottom("action_dock")
            .frame(
                egui::Frame::new()
                    .fill(rail_bg)
                    .stroke(egui::Stroke::new(1.0, hairline))
                    .inner_margin(egui::Margin::symmetric(16, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if !self.selected_markdown.is_empty()
                        && action_button(ui, "Copy Markdown", ButtonVariant::Ghost).clicked()
                    {
                        ctx.copy_text(self.selected_markdown.clone());
                        self.status_line = "Copied transcript to clipboard.".into();
                    }
                    ui.label(egui::RichText::new(&self.status_line).monospace().size(11.0).color(text_muted));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let enc_badge = if self.encrypt_at_rest {
                            "whistle.cact · local · encrypted"
                        } else {
                            "whistle.cact · 100% local"
                        };
                        ui.label(egui::RichText::new(enc_badge).monospace().size(10.5).color(text_muted));
                    });
                });
            });

        if !self.mini_mode {
            egui::SidePanel::left("left_rail")
                .exact_width(244.0)
                .frame(
                    egui::Frame::new()
                        .fill(rail_bg)
                        .stroke(egui::Stroke::new(1.0, hairline))
                        .inner_margin(egui::Margin::same(14)),
                )
                .show(ctx, |ui| {
                    let btn_text = if self.is_recording { "■  Stop Capture" } else { "●  Start Capture" };
                    let cta_fill = if self.is_recording { palette::DANGER_BG } else { active_bg };
                    let cta_stroke = if self.is_recording { palette::DANGER } else { cyan };
                    let cta_text = if self.is_recording { palette::DANGER } else { ice };
                    let cta = egui::Button::new(egui::RichText::new(btn_text).strong().size(13.0).color(cta_text))
                        .fill(cta_fill)
                        .stroke(egui::Stroke::new(1.0, cta_stroke))
                        .corner_radius(egui::CornerRadius::same(8))
                        .min_size(egui::vec2(ui.available_width(), 38.0));
                    if ui.add(cta).clicked() {
                        if self.is_recording {
                            self.stop_recording();
                        } else {
                            self.start_recording();
                        }
                    }

                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("SESSIONS").monospace().size(10.5).strong().color(text_muted));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if action_button(ui, "↻", ButtonVariant::Ghost).clicked() {
                                self.refresh_sessions();
                            }
                            let filter_lbl = if self.show_archived { "All" } else { "Active" };
                            if action_button(ui, filter_lbl, ButtonVariant::Ghost).clicked() {
                                self.show_archived = !self.show_archived;
                                self.refresh_sessions();
                            }
                        });
                    });
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical().id_salt("rail_sessions").show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 6.0;
                        let items = self.sessions.clone();
                        if items.is_empty() {
                            ui.add_space(16.0);
                            ui.label(egui::RichText::new("No sessions yet.").size(12.0).color(text_muted));
                        }
                        for s in items {
                            let selected = self.selected_session_id == Some(s.id);
                            let meta = format!(
                                "{} · {} turns",
                                s.started_at.format("%b %d %H:%M"),
                                s.utterance_count
                            );
                            if session_card_row(
                                ui,
                                s.id,
                                s.title.as_deref().unwrap_or("Untitled"),
                                &meta,
                                s.has_summary,
                                s.archived,
                                selected,
                            )
                            .clicked()
                            {
                                self.load_session(s.id);
                                self.tab = Tab::Transcripts;
                            }
                        }
                    });
                });
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(palette::VOID_BG).inner_margin(egui::Margin::same(16)))
            .show(ctx, |ui| match self.tab {
            Tab::Live => {
                if !self.is_recording && !self.calendar_events.is_empty() {
                    let events_snapshot = self.calendar_events.clone();
                    let now_ts = Utc::now().timestamp();
                    if let Some(ev) = events_snapshot.first() {
                        let mins = ((ev.start_ts - now_ts) as f64 / 60.0).round() as i64;
                        let timing = if ev.start_ts <= now_ts && now_ts <= ev.end_ts {
                            "Ongoing now".to_string()
                        } else if mins >= 0 {
                            format!("Starts in {mins} min")
                        } else {
                            format!("Started {} min ago", -mins)
                        };
                        egui::Frame::new()
                            .fill(active_bg)
                            .stroke(egui::Stroke::new(1.0, cyan))
                            .corner_radius(10)
                            .inner_margin(egui::Margin::same(14))
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                ui.horizontal_wrapped(|ui| {
                                    badge_pill(ui, "UPCOMING", ice, palette::VOID_BG);
                                    ui.label(egui::RichText::new(&ev.title).strong().size(13.5).color(text_primary));
                                    ui.label(egui::RichText::new(format!("· {timing} · {} attendee(s)", ev.attendees.len())).color(palette::TEXT_SECONDARY));
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if action_button(ui, "Record Only", ButtonVariant::Secondary).clicked() {
                                            self.join_and_record_event(ev, false);
                                        }
                                        if ev.meeting_url.is_some()
                                            && action_button(ui, "Join & Record", ButtonVariant::Primary).clicked()
                                        {
                                            self.join_and_record_event(ev, true);
                                        }
                                    });
                                });
                            });
                        ui.add_space(10.0);
                    }
                }
                section_card(ui, "SESSION SETUP", None, |ui| {
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(!self.is_recording, |ui| {
                            ui.allocate_ui(egui::vec2(240.0, 30.0), |ui| {
                                styled_text_input(ui, &mut self.session_title, "Session title (optional)", Some(220.0));
                            });
                            ui.allocate_ui(egui::vec2(280.0, 30.0), |ui| {
                                styled_text_input(ui, &mut self.keywords_input, "Keywords (comma-separated)", Some(260.0));
                            });
                        });
                        if !self.mini_mode && !self.cfg.keyword_presets.is_empty() {
                            let selected_lbl = self
                                .selected_preset_name
                                .as_deref()
                                .unwrap_or("Preset…");
                            egui::ComboBox::from_id_salt("kw_preset_combo")
                                .selected_text(selected_lbl)
                                .show_ui(ui, |ui| {
                                    let presets = self.cfg.keyword_presets.clone();
                                    for p in presets {
                                        let is_sel = self.selected_preset_name.as_deref() == Some(&p.name);
                                        if ui.selectable_label(is_sel, &p.name).clicked() {
                                            self.selected_preset_name = Some(p.name.clone());
                                            self.preset_name_input = p.name.clone();
                                            self.keywords_input = p.keywords.join(", ");
                                        }
                                    }
                                });
                        }
                        if self.mini_mode {
                            let btn_label = if self.is_recording { "Stop" } else { "Capture" };
                            let var = if self.is_recording { ButtonVariant::Danger } else { ButtonVariant::Primary };
                            if action_button(ui, btn_label, var).clicked() {
                                if self.is_recording {
                                    self.stop_recording();
                                } else {
                                    self.start_recording();
                                }
                            }
                        }
                    });
                });
                ui.add_space(10.0);
                egui::Frame::new()
                    .fill(card_bg)
                    .stroke(egui::Stroke::new(1.0, hairline))
                    .corner_radius(10)
                    .inner_margin(egui::Margin::same(16))
                    .show(ui, |ui| {
                        ui.set_min_size(ui.available_size());
                        egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                            if self.live_lines.is_empty() {
                                empty_state(
                                    ui,
                                    "Ready to capture locally",
                                    "Press Start Capture to transcribe microphone (Cyan) and system audio (Azure) via Whistle AI.",
                                );
                            } else {
                                for line in &self.live_lines {
                                    let is_sys = line.contains("[System]") || line.contains("System");
                                    let bar_col = if is_sys { azure } else { cyan };
                                    egui::Frame::new()
                                        .fill(palette::VOID_BG)
                                        .stroke(egui::Stroke::new(1.0, hairline))
                                        .corner_radius(8)
                                        .inner_margin(egui::Margin::symmetric(12, 9))
                                        .show(ui, |ui| {
                                            ui.set_min_width(ui.available_width());
                                            ui.horizontal(|ui| {
                                                let (rect, _) = ui.allocate_exact_size(egui::vec2(3.0, 18.0), egui::Sense::hover());
                                                ui.painter().rect_filled(rect, 1.5, bar_col);
                                                ui.label(egui::RichText::new(line).size(13.0).color(text_primary));
                                            });
                                        });
                                    ui.add_space(6.0);
                                }
                            }
                        });
                    });
            }
            Tab::Transcripts => {
                ui.horizontal(|ui| {
                    let q_resp = styled_text_input(
                        ui,
                        &mut self.search_query,
                        "Search transcripts…",
                        Some(320.0),
                    );
                    if (q_resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                        || action_button(ui, "Search", ButtonVariant::Primary).clicked()
                    {
                        self.run_search();
                    }
                    if !self.search_results.is_empty()
                        && action_button(ui, "Clear", ButtonVariant::Ghost).clicked()
                    {
                        self.search_query.clear();
                        self.search_results.clear();
                    }
                });
                if !self.search_results.is_empty() {
                    ui.add_space(8.0);
                    section_card(
                        ui,
                        "SEARCH RESULTS",
                        Some(&format!("{} hit(s)", self.search_results.len())),
                        |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("search_hits")
                                .max_height(130.0)
                                .show(ui, |ui| {
                                    let hits = self.search_results.clone();
                                    for hit in hits {
                                        let lbl = format!(
                                            "[{}] {}: {}",
                                            hit.session_title.as_deref().unwrap_or("Untitled"),
                                            hit.speaker_label,
                                            hit.text
                                        );
                                        if ui.selectable_label(false, lbl).clicked() {
                                            self.load_session(hit.session_id);
                                        }
                                    }
                                });
                        },
                    );
                }
                ui.add_space(10.0);
                if let Some(sel_id) = self.selected_session_id {
                    let is_archived = self
                        .sessions
                        .iter()
                        .find(|s| s.id == sel_id)
                        .is_some_and(|s| s.archived);
                    egui::Frame::new()
                        .fill(card_bg)
                        .stroke(egui::Stroke::new(1.0, hairline))
                        .corner_radius(10)
                        .inner_margin(egui::Margin::same(14))
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let resp = styled_text_input(
                                    ui,
                                    &mut self.selected_title_edit,
                                    "Session title…",
                                    Some(240.0),
                                );
                                if (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                                    || action_button(ui, "Rename", ButtonVariant::Secondary).clicked()
                                {
                                    self.rename_selected_session();
                                }
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if self.confirm_delete_id == Some(sel_id) {
                                        if action_button(ui, "Cancel", ButtonVariant::Ghost).clicked() {
                                            self.confirm_delete_id = None;
                                        }
                                        if action_button(ui, "Confirm Delete", ButtonVariant::Danger).clicked() {
                                            self.delete_selected_session(sel_id);
                                        }
                                    } else if action_button(ui, "Delete", ButtonVariant::Danger).clicked() {
                                        self.confirm_delete_id = Some(sel_id);
                                    }
                                    let arch_lbl = if is_archived { "Unarchive" } else { "Archive" };
                                    if action_button(ui, arch_lbl, ButtonVariant::Ghost).clicked() {
                                        self.toggle_archive_selected_session();
                                    }
                                });
                            });
                            ui.add_space(10.0);
                            ui.horizontal_wrapped(|ui| {
                                if self.selected_has_cleaned || self.selected_has_summary {
                                    egui::Frame::new()
                                        .fill(palette::VOID_BG)
                                        .stroke(egui::Stroke::new(1.0, hairline))
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
                                                    for (view, lbl) in [
                                                        (TranscriptView::Raw, "Raw"),
                                                        (TranscriptView::Diff, "Diff"),
                                                    ] {
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
                                                if self.selected_has_summary
                                                    && pill_tab(ui, "Summary", self.show_summary_panel)
                                                        .clicked()
                                                {
                                                    self.show_summary_panel = true;
                                                }
                                            });
                                        });
                                }
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if action_button(ui, "Run Cleanup", ButtonVariant::Primary).clicked() {
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
                                                        let src = utt.raw_text.as_deref().unwrap_or(&utt.text);
                                                        let base = basic_heuristic_cleanup(src, &kws);
                                                        let cleaned = if let Some(q) = qwen.as_mut() {
                                                            let (sys, usr) = build_qwen_cleanup_prompt_with_context(prev, &base, next, &kws);
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
                                                        let _ = tx.send(UiEvent::CleanupProgress(done, total));
                                                    },
                                                )?;
                                                Ok(format!(
                                                    "Cleaned {updated} utterance(s) via {}",
                                                    if used_qwen { "Qwen 2.5 0.5B" } else { "heuristics" }
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
                                                && let Ok(dec) = decrypt_bytes_at_rest(&self.cfg.data_dir, &raw)
                                                && std::fs::write(&out, dec).is_ok()
                                            {
                                                exported += 1;
                                            }
                                        }
                                        self.status_line = if exported > 0 {
                                            format!("Exported {exported} WAV file(s) to {}", export_dir.display())
                                        } else {
                                            "Failed to export WAV archive.".into()
                                        };
                                    }
                                });
                            });
                            if let Some((done, total)) = self.cleanup_progress {
                                ui.add_space(8.0);
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
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                styled_text_input(
                                    ui,
                                    &mut self.speaker_id_edit,
                                    "Speaker ID (e.g. spk_1)",
                                    Some(140.0),
                                );
                                styled_text_input(
                                    ui,
                                    &mut self.speaker_label_edit,
                                    "New speaker name",
                                    Some(180.0),
                                );
                                if action_button(ui, "Rename Speaker", ButtonVariant::Secondary).clicked() {
                                    self.rename_speaker_inline(sel_id);
                                }
                            });
                        });
                    ui.add_space(10.0);
                }
                egui::Frame::new()
                    .fill(card_bg)
                    .stroke(egui::Stroke::new(1.0, hairline))
                    .corner_radius(10)
                    .inner_margin(egui::Margin::same(16))
                    .show(ui, |ui| {
                        ui.set_min_size(ui.available_size());
                        egui::ScrollArea::vertical().id_salt("sess_md").show(ui, |ui| {
                            if self.show_summary_panel {
                                if let Some(ref summary) = self.selected_summary {
                                    let hdr = match self.selected_summary_template.as_deref() {
                                        Some(t) if !t.trim().is_empty() => format!("SUMMARY ({t})"),
                                        _ => "SUMMARY".to_string(),
                                    };
                                    ui.label(egui::RichText::new(hdr).monospace().size(11.0).color(ice));
                                    ui.add_space(6.0);
                                    render_markdown_view(ui, summary, ice, cyan, text_primary, text_muted, hairline);
                                } else {
                                    empty_state(
                                        ui,
                                        "No summary saved yet",
                                        "Your MCP assistant can generate and store one via save_summary.",
                                    );
                                }
                                if !self.selected_action_items.is_empty() {
                                    ui.add_space(12.0);
                                    ui.label(egui::RichText::new("ACTION ITEMS").monospace().size(11.0).color(cyan));
                                    ui.add_space(6.0);
                                    for item in &self.selected_action_items {
                                        let owner = item.owner.as_deref().unwrap_or("Unassigned");
                                        let due = item
                                            .due_hint
                                            .as_deref()
                                            .map(|d| format!(" (due: {d})"))
                                            .unwrap_or_default();
                                        let ts = item
                                            .timestamp
                                            .as_deref()
                                            .map(|t| format!(" [{t}]"))
                                            .unwrap_or_default();
                                        ui.label(
                                            egui::RichText::new(format!("• [{owner}] {}{due}{ts}", item.task))
                                                .size(13.0)
                                                .color(text_primary),
                                        );
                                    }
                                }
                            } else if self.selected_markdown.is_empty() {
                                empty_state(
                                    ui,
                                    "No session selected",
                                    "Select a session from the left rail to view its transcript.",
                                );
                            } else {
                                render_markdown_view(
                                    ui,
                                    &self.selected_markdown,
                                    ice,
                                    cyan,
                                    text_primary,
                                    text_muted,
                                    hairline,
                                );
                            }
                        });
                    });
            }
            Tab::Settings => {
                egui::ScrollArea::vertical().id_salt("settings_scroll").show(ui, |ui| {
                    section_card(ui, "CAPTURE & PRIVACY", None, |ui| {
                        ui.spacing_mut().item_spacing.y = 12.0;
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
                            "Multi-speaker mic diarization",
                            Some("Separate multiple local speakers on the microphone channel."),
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
                            "Save local WAV archives",
                            Some("Keep 16 kHz WAV recordings per session (encrypted when at-rest encryption is on)."),
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
                            "Post-process transcript cleanup on stop",
                            Some("Automatically run heuristic/Qwen transcript cleanup when capture stops."),
                            !self.is_recording,
                        )
                        .changed()
                        {
                            let _ = self.cfg.save();
                        }
                        if toggle_switch(
                            ui,
                            &mut self.encrypt_at_rest,
                            "At-rest encryption (SQLite, FTS5, WAL & WAV)",
                            Some("Encrypt transcripts, search index, WAL files, and WAV archives with ChaCha20-Poly1305."),
                            !self.is_recording,
                        )
                        .changed()
                        {
                            self.cfg.encrypt_at_rest = self.encrypt_at_rest;
                            let _ = self.cfg.save();
                            if self.encrypt_at_rest
                                && let Ok(store) = SessionStore::open_with_encryption(&self.cfg.data_dir, true)
                                && let Ok(count) = store.encrypt_existing_data()
                            {
                                self.status_line = format!(
                                    "At-rest encryption enabled ({count} existing record(s) encrypted)."
                                );
                            }
                        }
                        if toggle_switch(
                            ui,
                            &mut self.cfg.drop_sync_enabled,
                            "Sync exports to Drop Folder",
                            Some("Automatically write Markdown/JSONL exports on stop, cleanup, or MCP summary save."),
                            true,
                        )
                        .changed()
                        {
                            let _ = self.cfg.save();
                        }
                    });
                    ui.add_space(12.0);
                    section_card(ui, "KEYWORD PRESETS", None, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            styled_text_input(
                                ui,
                                &mut self.preset_name_input,
                                "Preset name",
                                Some(160.0),
                            );
                            styled_text_input(
                                ui,
                                &mut self.keywords_input,
                                "Keywords (comma-separated)",
                                Some(280.0),
                            );
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
                        });
                        if !self.cfg.keyword_presets.is_empty() {
                            ui.add_space(10.0);
                            let presets = self.cfg.keyword_presets.clone();
                            let mut delete_name: Option<String> = None;
                            for p in presets {
                                egui::Frame::new()
                                    .fill(palette::VOID_BG)
                                    .stroke(egui::Stroke::new(1.0, hairline))
                                    .corner_radius(egui::CornerRadius::same(7))
                                    .inner_margin(egui::Margin::symmetric(10, 7))
                                    .show(ui, |ui| {
                                        ui.set_min_width(ui.available_width());
                                        ui.horizontal(|ui| {
                                            ui.label(egui::RichText::new(&p.name).strong().color(text_primary));
                                            ui.label(egui::RichText::new(p.keywords.join(", ")).color(text_muted));
                                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                if action_button(ui, "Delete", ButtonVariant::Danger).clicked() {
                                                    delete_name = Some(p.name.clone());
                                                }
                                                if action_button(ui, "Load", ButtonVariant::Secondary).clicked() {
                                                    self.selected_preset_name = Some(p.name.clone());
                                                    self.preset_name_input = p.name.clone();
                                                    self.keywords_input = p.keywords.join(", ");
                                                }
                                            });
                                        });
                                    });
                                ui.add_space(4.0);
                            }
                            if let Some(del) = delete_name {
                                self.cfg.keyword_presets.retain(|p| !p.name.eq_ignore_ascii_case(&del));
                                if self.selected_preset_name.as_deref() == Some(&del) {
                                    self.selected_preset_name = None;
                                }
                                let _ = self.cfg.save();
                                self.status_line = format!("Deleted keyword preset '{del}'.");
                            }
                        }
                    });
                    ui.add_space(12.0);
                    section_card(ui, "CALENDAR (ICS)", None, |ui| {
                        ui.horizontal(|ui| {
                            styled_text_input(
                                ui,
                                &mut self.calendar_ics_input,
                                "https://...ics or /path/to/calendar.ics",
                                Some(340.0),
                            );
                            if action_button(ui, "Add .ics", ButtonVariant::Primary).clicked() {
                                let trimmed = self.calendar_ics_input.trim().to_string();
                                if !trimmed.is_empty() && !self.cfg.calendar_ics_urls.contains(&trimmed) {
                                    self.cfg.calendar_ics_urls.push(trimmed);
                                    self.calendar_ics_input.clear();
                                    let _ = self.cfg.save();
                                    self.refresh_calendar_events();
                                }
                            }
                            if action_button(ui, "Refresh", ButtonVariant::Secondary).clicked() {
                                self.refresh_calendar_events();
                            }
                        });
                        if !self.cfg.calendar_ics_urls.is_empty() {
                            ui.add_space(8.0);
                            let mut remove_idx = None;
                            for (idx, url) in self.cfg.calendar_ics_urls.iter().enumerate() {
                                egui::Frame::new()
                                    .fill(palette::VOID_BG)
                                    .stroke(egui::Stroke::new(1.0, hairline))
                                    .corner_radius(egui::CornerRadius::same(7))
                                    .inner_margin(egui::Margin::symmetric(10, 6))
                                    .show(ui, |ui| {
                                        ui.set_min_width(ui.available_width());
                                        ui.horizontal(|ui| {
                                            ui.monospace(url);
                                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                if action_button(ui, "Remove", ButtonVariant::Danger).clicked() {
                                                    remove_idx = Some(idx);
                                                }
                                            });
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
                    section_card(ui, "MCP & SUMMARIES", None, |ui| {
                        ui.label(
                            egui::RichText::new(
                                "Connect your AI assistant via MCP. Assistants read transcripts (get_transcript) and save or reuse summaries (save_summary, get_summary) using your preferred model.",
                            )
                            .size(12.5)
                            .color(palette::TEXT_SECONDARY),
                        );
                        ui.add_space(8.0);
                        if action_button(
                            ui,
                            "Connect Installed AI Assistants (MCP)",
                            ButtonVariant::Primary,
                        )
                        .clicked()
                            && let (Some(home), Ok(exe)) = (dirs::home_dir(), std::env::current_exe())
                        {
                            let cli_bin = exe.with_file_name("mimir");
                            let target_bin = if cli_bin.exists() { cli_bin } else { exe };
                            match connect_detected_assistants(&home, &target_bin, false) {
                                Ok(outcomes) => {
                                    self.connect_report = outcomes
                                        .into_iter()
                                        .map(|o| format!("[{}] {} -> {}", o.status, o.name, o.path.display()))
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                }
                                Err(e) => self.connect_report = format!("Error: {e}"),
                            }
                        }
                        if !self.connect_report.is_empty() {
                            ui.add_space(8.0);
                            egui::Frame::new()
                                .fill(palette::VOID_BG)
                                .stroke(egui::Stroke::new(1.0, hairline))
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
                        if action_button(ui, "Check for Updates…", ButtonVariant::Secondary).clicked() {
                            let tx = self.event_tx.clone();
                            self.update_line = "Checking GitHub Releases…".into();
                            std::thread::spawn(move || {
                                let res = check_for_updates(env!("CARGO_PKG_VERSION"));
                                let _ = tx.send(UiEvent::UpdateResult(res));
                            });
                        }
                        ui.add_space(8.0);
                        for m in model_statuses(&self.cfg) {
                            egui::Frame::new()
                                .fill(palette::VOID_BG)
                                .stroke(egui::Stroke::new(1.0, hairline))
                                .corner_radius(egui::CornerRadius::same(7))
                                .inner_margin(egui::Margin::symmetric(10, 8))
                                .show(ui, |ui| {
                                    ui.set_min_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new(&m.name).strong().color(text_primary));
                                        if m.installed {
                                            badge_pill(
                                                ui,
                                                &format!("Installed · {:.1} MB", m.size_bytes as f64 / (1024.0 * 1024.0)),
                                                palette::SUCCESS,
                                                palette::CARD_BG,
                                            );
                                        } else {
                                            badge_pill(
                                                ui,
                                                &format!("Not installed · {}", m.expected_size_label),
                                                text_muted,
                                                palette::CARD_BG,
                                            );
                                        }
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            if m.installed
                                                && action_button(ui, "Delete", ButtonVariant::Danger).clicked()
                                            {
                                                let res = match m.id.as_str() {
                                                    "whistle" => std::fs::remove_file(resolve_weights_path(&self.cfg)).map_err(Into::into),
                                                    "qwen" => delete_qwen_model(&self.cfg),
                                                    "silero_vad" => delete_silero_vad(&self.cfg),
                                                    "speaker_embed" => delete_speaker_embed(&self.cfg),
                                                    _ => Ok(()),
                                                };
                                                self.status_line = match res {
                                                    Ok(()) => format!("Deleted {}", m.name),
                                                    Err(e) => format!("Delete error: {e}"),
                                                };
                                            }
                                            let btn_lbl = if m.installed { "Verify" } else { "Download" };
                                            let var = if m.installed { ButtonVariant::Secondary } else { ButtonVariant::Primary };
                                            if action_button(ui, btn_lbl, var).clicked() {
                                                let cfg_clone = self.cfg.clone();
                                                let tx = self.event_tx.clone();
                                                let id = m.id.clone();
                                                self.status_line = format!("Downloading {}…", m.name);
                                                std::thread::spawn(move || {
                                                    let msg = match id.as_str() {
                                                        "whistle" => match fetch_whistle_weights(&resolve_weights_path(&cfg_clone)) {
                                                            Ok(p) => format!("Verified whistle.cact at {}", p.display()),
                                                            Err(e) => format!("Fetch error: {e}"),
                                                        },
                                                        "qwen" => match fetch_qwen_model(&cfg_clone) {
                                                            Ok((w, _)) => format!("Verified Qwen 2.5 0.5B at {}", w.display()),
                                                            Err(e) => format!("Fetch error: {e}"),
                                                        },
                                                        "silero_vad" => match fetch_silero_vad(&cfg_clone) {
                                                            Ok(p) => format!("Verified silero_vad.onnx at {}", p.display()),
                                                            Err(e) => format!("Fetch error: {e}"),
                                                        },
                                                        "speaker_embed" => match fetch_speaker_embed(&cfg_clone) {
                                                            Ok(p) => format!("Verified speaker_embed.onnx at {}", p.display()),
                                                            Err(e) => format!("Fetch error: {e}"),
                                                        },
                                                        _ => "Unknown model".into(),
                                                    };
                                                    let _ = tx.send(UiEvent::Status(msg));
                                                });
                                            }
                                        });
                                    });
                                });
                            ui.add_space(4.0);
                        }
                        if !self.update_line.is_empty() {
                            ui.add_space(6.0);
                            ui.label(egui::RichText::new(&self.update_line).color(palette::TEXT_SECONDARY));
                        }
                    });
                });
            }
        });
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
        painter.add(egui::Shape::convex_polygon(mapped, col, egui::Stroke::NONE));
    };
    // Top chevron (split into two convex halves for egui convex_polygon)
    poly(&[(92.0, 92.0), (156.0, 92.0), (256.0, 192.0), (256.0, 268.0), (92.0, 104.0)], tone(240));
    poly(&[(256.0, 192.0), (356.0, 92.0), (420.0, 92.0), (420.0, 104.0), (256.0, 268.0)], tone(240));
    poly(&[(92.0, 125.21), (156.0, 189.21), (156.0, 344.23), (92.0, 394.15)], tone(190));
    poly(&[(356.0, 189.21), (420.0, 125.21), (420.0, 394.15), (356.0, 344.23)], tone(150));
    poly(&[(92.0, 413.17), (156.0, 363.25), (156.0, 420.0), (92.0, 420.0)], tone(115));
    poly(&[(356.0, 363.25), (420.0, 413.17), (420.0, 420.0), (356.0, 420.0)], tone(115));
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
        ui.label(egui::RichText::new(label).monospace().size(9.5).color(label_col));
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

fn write_wav_i16(path: &std::path::Path, samples: &[i16], sample_rate: u32) -> std::io::Result<()> {
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
    if write_wav_i16(&wav_path, samples, 16_000).is_ok() && encrypt {
        if let Ok(raw) = std::fs::read(&wav_path)
            && let Ok(enc) = encrypt_bytes_at_rest(data_dir, &raw)
            && std::fs::write(audio_dir.join(format!("{stem}.wav.enc")), enc).is_ok()
        {
            let _ = std::fs::remove_file(&wav_path);
        }
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
        .with_min_inner_size([720.0, 500.0])
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
    let code_bg = egui::Color32::from_rgb(6, 8, 13);

    for raw_line in markdown.lines() {
        let trimmed = raw_line.trim();
        if trimmed.starts_with("```") {
            if in_code {
                egui::Frame::new()
                    .fill(code_bg)
                    .stroke(egui::Stroke::new(1.0, hairline))
                    .corner_radius(6)
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(code_buf.trim_end()).monospace().size(12.0).color(text_primary));
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
                1 => 18.5,
                2 => 15.0,
                _ => 13.5,
            };
            let color = if hashes <= 2 { ice } else { text_primary };
            ui.add_space(if hashes == 1 { 2.0 } else { 5.0 });
            ui.label(egui::RichText::new(heading_text).strong().size(size).color(color));
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
            ui.horizontal_wrapped(|ui| {
                ui.add_space(6.0);
                ui.label(egui::RichText::new("•").strong().color(cyan));
                ui.label(build_inline_markdown_job(rest, 13.0, text_primary, ice, code_bg));
            });
            continue;
        }

        if let Some((num, rest)) = trimmed.split_once(". ")
            && !num.is_empty()
            && num.chars().all(|c| c.is_ascii_digit())
        {
            ui.horizontal_wrapped(|ui| {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(format!("{num}.")).monospace().strong().color(cyan));
                ui.label(build_inline_markdown_job(rest, 13.0, text_primary, ice, code_bg));
            });
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("> ").or_else(|| (trimmed == ">").then_some("")) {
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(12, 20, 34))
                .stroke(egui::Stroke::new(1.0, hairline))
                .corner_radius(4)
                .inner_margin(egui::Margin::symmetric(10, 5))
                .show(ui, |ui| {
                    ui.label(build_inline_markdown_job(rest, 12.5, text_muted, ice, code_bg));
                });
            continue;
        }

        ui.label(build_inline_markdown_job(raw_line, 13.0, text_primary, ice, code_bg));
    }

    if in_code && !code_buf.is_empty() {
        egui::Frame::new()
            .fill(code_bg)
            .stroke(egui::Stroke::new(1.0, hairline))
            .corner_radius(6)
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| {
                ui.label(egui::RichText::new(code_buf.trim_end()).monospace().size(12.0).color(text_primary));
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
                            samples.push(f32::from_le_bytes([
                                chunk[0], chunk[1], chunk[2], chunk[3],
                            ]));
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

