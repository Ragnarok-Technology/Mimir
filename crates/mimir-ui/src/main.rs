mod updater;

use chrono::Utc;
use eframe::egui;
use mimir_audio::{BleedGate, VadChunker, start_dual_capture};
use mimir_bridge::connect::connect_detected_assistants;
use mimir_bridge::mcp::format_live_utterance_md;
use mimir_core::calendar::{
    CalendarEvent, open_url_in_system_browser, parse_ics, upcoming_or_active_events,
};
use mimir_core::{
    ActionItem, AudioRetention, ChannelId, KeywordPreset, MimirConfig, SUMMARY_TEMPLATES, Session,
    Utterance,
};
use mimir_diarize::{Diarizer, DiarizerConfig, insert_utterance_chronological, mean_pool_frames};
use mimir_store::cleanup::{
    basic_heuristic_cleanup, build_qwen_cleanup_prompt_with_context, sanitize_cleanup_edit,
};
use mimir_store::{
    SearchHit, SessionStore, SessionSummary, encrypt_bytes_at_rest, ensure_private_dir,
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
    Assistants,
}

enum UiEvent {
    LiveLine(String),
    Status(String),
    CleanupProgress(usize, usize),
    CleanupDone,
    SummarizeProgress(usize, usize),
    SummarizeDone,
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
    selected_action_items: Vec<ActionItem>,
    selected_summary_template: String,
    show_summary_panel: bool,
    transcript_view: TranscriptView,
    search_query: String,
    search_results: Vec<SearchHit>,
    speaker_id_edit: String,
    speaker_label_edit: String,
    status_line: String,
    cleanup_progress: Option<(usize, usize)>,
    summarize_progress: Option<(usize, usize)>,
    calendar_events: Vec<CalendarEvent>,
    calendar_ics_input: String,
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
        let default_template = cfg.default_summary_template.clone();
        let mut app = Self {
            cfg,
            tab: Tab::Live,
            mini_mode: false,
            session_title: String::new(),
            keywords_input: String::new(),
            preset_name_input: String::new(),
            selected_preset_name: None,
            diarize_mic: false,
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
            selected_action_items: Vec::new(),
            selected_summary_template: default_template,
            show_summary_panel: false,
            transcript_view: TranscriptView::Cleaned,
            search_query: String::new(),
            search_results: Vec::new(),
            speaker_id_edit: String::new(),
            speaker_label_edit: String::new(),
            status_line: "Ready — 100% Local • Whistle AI • Zero Telemetry".to_string(),
            cleanup_progress: None,
            summarize_progress: None,
            calendar_events: Vec::new(),
            calendar_ics_input: String::new(),
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
                    self.selected_action_items.clear();
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
            self.selected_markdown = render_markdown_with_view(&session, self.transcript_view);
            self.selected_summary = session.summary_markdown.clone();
            if let Some(tmpl) = session.summary_template.clone() {
                self.selected_summary_template = tmpl;
            }
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

        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let engine = WhistleEngine::load(&lib_path, &weights_path)?;
                let store = SessionStore::open(&cfg.data_dir)?;
                let mut session = Session::new(title, keywords.clone());
                if let Some((uid, url, attendees)) = cal_meta {
                    session.calendar_event_uid = Some(uid);
                    session.meeting_url = url;
                    if !attendees.is_empty() {
                        session.attendees_json = serde_json::to_string(&attendees).ok();
                    }
                }
                store.create_session(&session)?;

                let (audio_tx, audio_rx) = mpsc::channel();
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
                if cfg.auto_summarize {
                    let (qwen_w, qwen_t) = resolve_qwen_paths(&cfg);
                    if qwen_w.is_file() && qwen_t.is_file() {
                        let _ = tx_ui.send(UiEvent::Status("Auto-summarizing with Qwen 2.5 0.5B…".into()));
                        if let Ok(mut qwen) = QwenCleanupModel::load(&qwen_w, &qwen_t) {
                            let _ = store.summarize_session_with_progress(
                                session.id,
                                &cfg.default_summary_template,
                                |sys, usr, max_tok| qwen.complete(sys, usr, max_tok),
                                |done, total| {
                                    let _ = tx_ui.send(UiEvent::SummarizeProgress(done, total));
                                },
                            );
                            let _ = tx_ui.send(UiEvent::SummarizeDone);
                        }
                    }
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
                    if let Some(sid) = self.selected_session_id {
                        self.load_session(sid);
                    }
                }
                UiEvent::SummarizeProgress(done, total) => {
                    self.summarize_progress = Some((done, total));
                }
                UiEvent::SummarizeDone => {
                    self.summarize_progress = None;
                    self.show_summary_panel = true;
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
            .frame(egui::Frame::new().fill(rail_bg).inner_margin(egui::Margin::symmetric(14, 9)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    draw_grayscale_mimir_mark(ui, 18.0);
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("M I M I R").strong().size(13.5).color(text_primary));
                    ui.add_space(6.0);
                    let status_text = if self.is_recording { "● LIVE · CAPTURING" } else { "● STANDBY" };
                    let status_col = if self.is_recording { ice } else { cyan };
                    ui.label(egui::RichText::new(status_text).monospace().size(11.0).color(status_col));
                    ui.add_space(10.0);
                    let mic_lvl = f32::from_bits(self.mic_level.load(Ordering::Relaxed));
                    let sys_lvl = f32::from_bits(self.sys_level.load(Ordering::Relaxed));
                    draw_level_meter(ui, "MIC", mic_lvl, cyan, hairline, text_muted);
                    ui.add_space(6.0);
                    draw_level_meter(ui, "SYS", sys_lvl, azure, hairline, text_muted);
                    ui.add_space(12.0);
                    if !self.mini_mode {
                        ui.selectable_value(&mut self.tab, Tab::Live, "Live");
                        ui.selectable_value(&mut self.tab, Tab::Transcripts, "Transcripts");
                        ui.selectable_value(&mut self.tab, Tab::Assistants, "Assistants & Updates");
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Hide").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                        }
                        let mini_label = if self.mini_mode { "Full View" } else { "Mini Mode" };
                        if ui.button(mini_label).clicked() {
                            self.mini_mode = !self.mini_mode;
                            if self.mini_mode {
                                self.tab = Tab::Live;
                            }
                        }
                    });
                });
            });

        egui::TopBottomPanel::bottom("action_dock")
            .frame(egui::Frame::new().fill(rail_bg).inner_margin(egui::Margin::symmetric(14, 7)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Copy Markdown").clicked() && !self.selected_markdown.is_empty() {
                        ctx.copy_text(self.selected_markdown.clone());
                        self.status_line = "Copied transcript to clipboard.".into();
                    }
                    ui.label(egui::RichText::new(&self.status_line).monospace().size(11.0).color(text_muted));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new("whistle.cact · 100% local · zero telemetry").monospace().size(10.5).color(text_muted));
                    });
                });
            });

        if !self.mini_mode {
            egui::SidePanel::left("left_rail")
                .exact_width(228.0)
                .frame(egui::Frame::new().fill(rail_bg).inner_margin(egui::Margin::same(14)))
                .show(ctx, |ui| {
                    let btn_text = if self.is_recording { "■  Stop Capture" } else { "●  Start Capture" };
                    let cta = egui::Button::new(egui::RichText::new(btn_text).strong().color(text_primary))
                        .fill(active_bg)
                        .stroke(egui::Stroke::new(1.0, if self.is_recording { ice } else { cyan }))
                        .min_size(egui::vec2(ui.available_width(), 34.0));
                    if ui.add(cta).clicked() {
                        if self.is_recording {
                            self.stop_recording();
                        } else {
                            self.start_recording();
                        }
                    }

                    ui.add_space(10.0);
                    ui.label(egui::RichText::new("CAPTURE").monospace().size(10.0).color(text_muted));
                    ui.add_enabled(
                        !self.is_recording,
                        egui::Checkbox::new(&mut self.diarize_mic, "Multi-speaker mic"),
                    );
                    ui.add_enabled(
                        !self.is_recording,
                        egui::Checkbox::new(&mut self.save_wav, "Save local WAVs"),
                    );
                    ui.add_enabled(
                        !self.is_recording,
                        egui::Checkbox::new(&mut self.encrypt_at_rest, "Encrypt at rest"),
                    );
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(6.0);

                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("SESSIONS").monospace().size(10.0).color(text_muted));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("↻").clicked() {
                                self.refresh_sessions();
                            }
                            let filter_lbl = if self.show_archived { "All" } else { "Active" };
                            if ui.small_button(filter_lbl).clicked() {
                                self.show_archived = !self.show_archived;
                                self.refresh_sessions();
                            }
                        });
                    });
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical().id_salt("rail_sessions").show(ui, |ui| {
                        let items = self.sessions.clone();
                        for s in items {
                            let selected = self.selected_session_id == Some(s.id);
                            let arch_tag = if s.archived { " · archived" } else { "" };
                            let label = format!(
                                "{}\n{} turns{}",
                                s.title.as_deref().unwrap_or("Untitled"),
                                s.utterance_count,
                                arch_tag
                            );
                            if ui.selectable_label(selected, label).clicked() {
                                self.load_session(s.id);
                                self.tab = Tab::Transcripts;
                            }
                        }
                    });
                });
        }

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
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
                            .corner_radius(8)
                            .inner_margin(egui::Margin::same(10))
                            .show(ui, |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(egui::RichText::new("UPCOMING MEETING").monospace().size(10.0).color(ice));
                                    ui.label(egui::RichText::new(&ev.title).strong().color(text_primary));
                                    ui.label(egui::RichText::new(format!("· {timing} · {} attendee(s)", ev.attendees.len())).color(text_muted));
                                    if ev.meeting_url.is_some()
                                        && ui.button(egui::RichText::new("Join & Record").strong().color(ice)).clicked()
                                    {
                                        self.join_and_record_event(ev, true);
                                    }
                                    if ui.button("Record Only").clicked() {
                                        self.join_and_record_event(ev, false);
                                    }
                                });
                            });
                        ui.add_space(6.0);
                    }
                }
                ui.horizontal(|ui| {
                    ui.add_enabled(
                        !self.is_recording,
                        egui::TextEdit::singleline(&mut self.session_title)
                            .hint_text("Session title (optional)"),
                    );
                    ui.add_enabled(
                        !self.is_recording,
                        egui::TextEdit::singleline(&mut self.keywords_input)
                            .hint_text("Keywords (comma-separated)"),
                    );
                    if self.mini_mode {
                        let btn_label = if self.is_recording { "Stop" } else { "Capture" };
                        if ui.button(btn_label).clicked() {
                            if self.is_recording {
                                self.stop_recording();
                            } else {
                                self.start_recording();
                            }
                        }
                    }
                });
                if !self.mini_mode {
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new("Preset:").size(11.5).color(text_muted));
                        let selected_lbl = self
                            .selected_preset_name
                            .as_deref()
                            .unwrap_or("Select preset…");
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
                        ui.add(
                            egui::TextEdit::singleline(&mut self.preset_name_input)
                                .desired_width(110.0)
                                .hint_text("Preset name"),
                        );
                        if ui.button("Save Preset").clicked() {
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
                        if let Some(sel_name) = self.selected_preset_name.clone()
                            && ui.button("Delete Preset").clicked()
                        {
                            self.cfg
                                .keyword_presets
                                .retain(|p| !p.name.eq_ignore_ascii_case(&sel_name));
                            self.selected_preset_name = None;
                            let _ = self.cfg.save();
                            self.status_line = format!("Deleted keyword preset '{sel_name}'.");
                        }
                    });
                }
                ui.add_space(8.0);
                egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                    if self.live_lines.is_empty() {
                        egui::Frame::new()
                            .fill(card_bg)
                            .stroke(egui::Stroke::new(1.0, hairline))
                            .corner_radius(8)
                            .inner_margin(egui::Margin::same(14))
                            .show(ui, |ui| {
                                ui.label(egui::RichText::new("Ready to refract dual-channel audio locally.").strong().color(text_primary));
                                ui.label(egui::RichText::new("Press Start Capture to transcribe mic (Cyan) and system audio (Azure) with Whistle AI.").color(text_muted));
                            });
                    } else {
                        for line in &self.live_lines {
                            let is_sys = line.contains("[System]") || line.contains("System");
                            let bar_col = if is_sys { azure } else { cyan };
                            egui::Frame::new()
                                .fill(card_bg)
                                .stroke(egui::Stroke::new(1.0, hairline))
                                .corner_radius(8)
                                .inner_margin(egui::Margin::same(10))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        let (rect, _) = ui.allocate_exact_size(egui::vec2(3.0, 20.0), egui::Sense::hover());
                                        ui.painter().rect_filled(rect, 1.5, bar_col);
                                        ui.label(egui::RichText::new(line).color(text_primary));
                                    });
                                });
                            ui.add_space(4.0);
                        }
                    }
                });
            }
            Tab::Transcripts => {
                ui.horizontal(|ui| {
                    let q_resp = ui.add(
                        egui::TextEdit::singleline(&mut self.search_query)
                            .hint_text("Search transcripts (FTS5)…"),
                    );
                    if (q_resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                        || ui.button("Search").clicked()
                    {
                        self.run_search();
                    }
                    if !self.search_results.is_empty() && ui.button("Clear").clicked() {
                        self.search_query.clear();
                        self.search_results.clear();
                    }
                });
                if !self.search_results.is_empty() {
                    ui.add_space(4.0);
                    egui::Frame::new()
                        .fill(card_bg)
                        .stroke(egui::Stroke::new(1.0, hairline))
                        .corner_radius(8)
                        .inner_margin(egui::Margin::same(8))
                        .show(ui, |ui| {
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
                        });
                }
                ui.add_space(6.0);
                if let Some(sel_id) = self.selected_session_id {
                    let is_archived = self
                        .sessions
                        .iter()
                        .find(|s| s.id == sel_id)
                        .is_some_and(|s| s.archived);
                    ui.horizontal_wrapped(|ui| {
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.selected_title_edit)
                                .desired_width(200.0)
                                .hint_text("Session title…"),
                        );
                        if (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                            || ui.button("Rename").clicked()
                        {
                            self.rename_selected_session();
                        }
                        let arch_lbl = if is_archived { "Unarchive" } else { "Archive" };
                        if ui.button(arch_lbl).clicked() {
                            self.toggle_archive_selected_session();
                        }
                        if self.confirm_delete_id == Some(sel_id) {
                            if ui
                                .button(egui::RichText::new("Confirm Delete").color(egui::Color32::LIGHT_RED))
                                .clicked()
                            {
                                self.delete_selected_session(sel_id);
                            }
                            if ui.button("Cancel").clicked() {
                                self.confirm_delete_id = None;
                            }
                        } else if ui
                            .button(egui::RichText::new("Delete").color(egui::Color32::LIGHT_RED))
                            .clicked()
                        {
                            self.confirm_delete_id = Some(sel_id);
                        }
                        if ui.button("Run Cleanup").clicked() {
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
                        if ui.button("Revert Raw").clicked() {
                            if let Ok(store) = SessionStore::open(&self.cfg.data_dir)
                                && let Ok(n) = store.revert_session_cleanup(sel_id)
                            {
                                self.status_line = format!("Reverted {n} utterance(s) to raw text.");
                                self.load_session(sel_id);
                            }
                        }
                        for (view, lbl) in [
                            (TranscriptView::Cleaned, "Cleaned"),
                            (TranscriptView::Raw, "Raw"),
                            (TranscriptView::Diff, "Diff"),
                        ] {
                            if ui.selectable_label(!self.show_summary_panel && self.transcript_view == view, lbl).clicked() {
                                self.show_summary_panel = false;
                                self.transcript_view = view;
                                self.load_session(sel_id);
                            }
                        }
                        if ui.selectable_label(self.show_summary_panel, "Summary").clicked() {
                            self.show_summary_panel = true;
                        }
                        egui::ComboBox::from_id_salt("summary_tmpl_combo")
                            .selected_text(&self.selected_summary_template)
                            .show_ui(ui, |ui| {
                                for tmpl in SUMMARY_TEMPLATES {
                                    ui.selectable_value(
                                        &mut self.selected_summary_template,
                                        tmpl.to_string(),
                                        tmpl,
                                    );
                                }
                            });
                        if ui.button("Summarize").clicked() {
                            let cfg_clone = self.cfg.clone();
                            let tmpl = self.selected_summary_template.clone();
                            let tx = self.event_tx.clone();
                            self.status_line = "Summarizing session with Qwen 2.5 0.5B…".into();
                            self.summarize_progress = Some((0, 0));
                            std::thread::spawn(move || {
                                let res: anyhow::Result<String> = (|| {
                                    let (qwen_w, qwen_t) = resolve_qwen_paths(&cfg_clone);
                                    anyhow::ensure!(
                                        qwen_w.is_file() && qwen_t.is_file(),
                                        "Qwen 2.5 0.5B not installed — fetch it in Assistants tab"
                                    );
                                    let store = SessionStore::open(&cfg_clone.data_dir)?;
                                    let mut qwen = QwenCleanupModel::load(&qwen_w, &qwen_t)?;
                                    let out = store.summarize_session_with_progress(
                                        sel_id,
                                        &tmpl,
                                        |sys, usr, max_tok| qwen.complete(sys, usr, max_tok),
                                        |done, total| {
                                            let _ = tx.send(UiEvent::SummarizeProgress(done, total));
                                        },
                                    )?;
                                    let _ = sync_session_to_drop_dir_if_enabled(&store, sel_id, &cfg_clone);
                                    Ok(format!(
                                        "Summarized ({}, {} action item(s))",
                                        out.summary_template,
                                        out.action_items.len()
                                    ))
                                })();
                                let msg = match res {
                                    Ok(s) => s,
                                    Err(e) => format!("Summary error: {e}"),
                                };
                                let _ = tx.send(UiEvent::SummarizeDone);
                                let _ = tx.send(UiEvent::Status(msg));
                            });
                        }
                    });
                    if let Some((done, total)) = self.cleanup_progress {
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
                    if let Some((done, total)) = self.summarize_progress {
                        let frac = if total > 0 {
                            done as f32 / total as f32
                        } else {
                            0.0
                        };
                        let label = if total > 0 {
                            format!("Summarizing step {done} / {total}…")
                        } else {
                            "Loading Qwen 2.5 0.5B…".to_string()
                        };
                        ui.add(egui::ProgressBar::new(frac).text(label).animate(total == 0));
                        ui.ctx().request_repaint();
                    }
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.speaker_id_edit)
                                .desired_width(90.0)
                                .hint_text("Speaker ID"),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut self.speaker_label_edit)
                                .desired_width(140.0)
                                .hint_text("New speaker name"),
                        );
                        if ui.button("Rename Speaker").clicked() {
                            self.rename_speaker_inline(sel_id);
                        }
                    });
                    ui.add_space(6.0);
                }
                egui::Frame::new()
                    .fill(card_bg)
                    .stroke(egui::Stroke::new(1.0, hairline))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical().id_salt("sess_md").show(ui, |ui| {
                            if self.show_summary_panel {
                                if let Some(ref summary) = self.selected_summary {
                                    ui.label(egui::RichText::new("SUMMARY").monospace().size(11.0).color(ice));
                                    ui.add_space(4.0);
                                    ui.label(egui::RichText::new(summary).color(text_primary));
                                } else {
                                    ui.label(egui::RichText::new("No summary generated yet — click Summarize above.").color(text_muted));
                                }
                                if !self.selected_action_items.is_empty() {
                                    ui.add_space(10.0);
                                    ui.label(egui::RichText::new("ACTION ITEMS").monospace().size(11.0).color(cyan));
                                    ui.add_space(4.0);
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
                                                .color(text_primary),
                                        );
                                    }
                                }
                            } else if self.selected_markdown.is_empty() {
                                ui.label(egui::RichText::new("Select a session from the left rail to view its Markdown.").color(text_muted));
                            } else {
                                ui.monospace(&self.selected_markdown);
                            }
                        });
                    });
            }
            Tab::Assistants => {
                egui::Frame::new()
                    .fill(card_bg)
                    .stroke(egui::Stroke::new(1.0, hairline))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::same(14))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("AUTOMATION & CALENDAR").monospace().size(11.0).color(text_muted));
                        ui.add_space(6.0);
                        if ui.checkbox(&mut self.cfg.auto_summarize, "Auto-summarize after recording (Qwen 2.5 0.5B)").changed() {
                            let _ = self.cfg.save();
                        }
                        if ui.checkbox(&mut self.cfg.drop_sync_enabled, "Sync exports to Drop Folder on stop / cleanup / summarize").changed() {
                            let _ = self.cfg.save();
                        }
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.calendar_ics_input)
                                    .desired_width(300.0)
                                    .hint_text("https://...ics or /path/to/calendar.ics"),
                            );
                            if ui.button("Add .ics").clicked() {
                                let trimmed = self.calendar_ics_input.trim().to_string();
                                if !trimmed.is_empty() && !self.cfg.calendar_ics_urls.contains(&trimmed) {
                                    self.cfg.calendar_ics_urls.push(trimmed);
                                    self.calendar_ics_input.clear();
                                    let _ = self.cfg.save();
                                    self.refresh_calendar_events();
                                }
                            }
                            if ui.button("Refresh Calendar").clicked() {
                                self.refresh_calendar_events();
                            }
                        });
                        let mut remove_idx = None;
                        for (idx, url) in self.cfg.calendar_ics_urls.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.monospace(url);
                                if ui.small_button("Remove").clicked() {
                                    remove_idx = Some(idx);
                                }
                            });
                        }
                        if let Some(idx) = remove_idx {
                            self.cfg.calendar_ics_urls.remove(idx);
                            let _ = self.cfg.save();
                            self.refresh_calendar_events();
                        }
                    });
                ui.add_space(10.0);
                egui::Frame::new()
                    .fill(card_bg)
                    .stroke(egui::Stroke::new(1.0, hairline))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::same(14))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("MCP ASSISTANT AUTO-CONNECT").monospace().size(11.0).color(text_muted));
                        ui.add_space(6.0);
                        if ui.button("Connect Installed AI Assistants (MCP)").clicked() {
                            if let (Some(home), Ok(exe)) = (dirs::home_dir(), std::env::current_exe()) {
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
                        }
                        if !self.connect_report.is_empty() {
                            ui.add_space(6.0);
                            ui.monospace(&self.connect_report);
                        }
                    });
                ui.add_space(10.0);
                egui::Frame::new()
                    .fill(card_bg)
                    .stroke(egui::Stroke::new(1.0, hairline))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::same(14))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("LOCAL AI MODELS & UPDATES").monospace().size(11.0).color(text_muted));
                            if ui.button("Check for Updates…").clicked() {
                                let tx = self.event_tx.clone();
                                self.update_line = "Checking GitHub Releases…".into();
                                std::thread::spawn(move || {
                                    let res = check_for_updates(env!("CARGO_PKG_VERSION"));
                                    let _ = tx.send(UiEvent::UpdateResult(res));
                                });
                            }
                        });
                        ui.add_space(6.0);
                        for m in model_statuses(&self.cfg) {
                            ui.horizontal_wrapped(|ui| {
                                let status_str = if m.installed {
                                    format!("Installed ({:.1} MB)", m.size_bytes as f64 / (1024.0 * 1024.0))
                                } else {
                                    format!("Not installed ({})", m.expected_size_label)
                                };
                                ui.label(format!("{} — {}", m.name, status_str));
                                let btn_lbl = if m.installed { "Verify" } else { "Download" };
                                if ui.button(btn_lbl).clicked() {
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
                                if m.installed && ui.button("Delete").clicked() {
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
                            });
                        }
                        if !self.update_line.is_empty() {
                            ui.add_space(6.0);
                            ui.label(&self.update_line);
                        }
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

fn apply_obsidian_prism_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    let void_bg = egui::Color32::from_rgb(6, 8, 13);        // #06080D
    let card_bg = egui::Color32::from_rgb(11, 16, 26);      // #0B101A
    let elevated = egui::Color32::from_rgb(15, 23, 42);     // #0F172A
    let hairline = egui::Color32::from_rgb(24, 36, 54);     // #182436
    let cyan = egui::Color32::from_rgb(56, 189, 248);       // #38BDF8
    let cobalt = egui::Color32::from_rgb(37, 99, 235);      // #2563EB

    visuals.panel_fill = void_bg;
    visuals.window_fill = card_bg;
    visuals.extreme_bg_color = card_bg;
    visuals.selection.bg_fill = cobalt;
    visuals.selection.stroke = egui::Stroke::new(1.0, cyan);
    visuals.widgets.noninteractive.bg_fill = card_bg;
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, hairline);
    visuals.widgets.inactive.bg_fill = elevated;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, hairline);
    visuals.widgets.hovered.bg_fill = elevated;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, cyan);
    visuals.widgets.active.bg_fill = cobalt;
    ctx.set_visuals(visuals);
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
