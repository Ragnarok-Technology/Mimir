mod updater;

use chrono::Utc;
use eframe::egui;
use mimir_audio::{BleedGate, VadChunker, start_dual_capture};
use mimir_bridge::connect::connect_detected_assistants;
use mimir_bridge::mcp::format_live_utterance_md;
use mimir_core::{AudioRetention, ChannelId, MimirConfig, Session, TranscriptFormat, Utterance};
use mimir_diarize::{Diarizer, DiarizerConfig, insert_utterance_chronological, mean_pool_frames};
use mimir_store::{SearchHit, SessionStore, SessionSummary, export::render_session};
use mimir_whistle::artifact::{fetch_whistle_weights, resolve_lib_path, resolve_weights_path};
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
    SessionFinished,
    UpdateResult(UpdateStatus),
}

struct MimirDesktopApp {
    cfg: MimirConfig,
    tab: Tab,
    mini_mode: bool,
    session_title: String,
    keywords_input: String,
    diarize_mic: bool,
    save_wav: bool,
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
    search_query: String,
    search_results: Vec<SearchHit>,
    speaker_id_edit: String,
    speaker_label_edit: String,
    status_line: String,
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
        let mut app = Self {
            cfg,
            tab: Tab::Live,
            mini_mode: false,
            session_title: String::new(),
            keywords_input: String::new(),
            diarize_mic: false,
            save_wav,
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
            search_query: String::new(),
            search_results: Vec::new(),
            speaker_id_edit: String::new(),
            speaker_label_edit: String::new(),
            status_line: "Ready — 100% Local • Whistle AI • Zero Telemetry".to_string(),
            connect_report: String::new(),
            update_line: String::new(),
            event_tx,
            event_rx,
        };
        app.refresh_sessions();
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
            && let Ok(md) = render_session(&session, TranscriptFormat::Markdown)
        {
            self.selected_markdown = md;
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
        let mic_level = self.mic_level.clone();
        let sys_level = self.sys_level.clone();
        let tx_ui = self.event_tx.clone();

        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let engine = WhistleEngine::load(&lib_path, &weights_path)?;
                let store = SessionStore::open(&cfg.data_dir)?;
                let mut session = Session::new(title, keywords.clone());
                store.create_session(&session)?;

                let (audio_tx, audio_rx) = mpsc::channel();
                let capture = start_dual_capture(audio_tx)?;

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
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }

                drop(capture);
                mic_level.store(0.0f32.to_bits(), Ordering::Relaxed);
                sys_level.store(0.0f32.to_bits(), Ordering::Relaxed);
                if save_wav {
                    let audio_dir = cfg.data_dir.join("audio");
                    let _ = std::fs::create_dir_all(&audio_dir);
                    if !mic_pcm.is_empty() {
                        let _ = write_wav_i16(&audio_dir.join(format!("{}_mic.wav", session.id)), &mic_pcm, 16_000);
                    }
                    if !sys_pcm.is_empty() {
                        let _ = write_wav_i16(&audio_dir.join(format!("{}_sys.wav", session.id)), &sys_pcm, 16_000);
                    }
                }
                let _ = store.finish_session(
                    session.id,
                    Utc::now(),
                    &langs.into_iter().collect::<Vec<_>>(),
                );
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
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
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
                    ui.horizontal(|ui| {
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.selected_title_edit)
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
                    });
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
                            if self.selected_markdown.is_empty() {
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
                        ui.label(egui::RichText::new("WHISTLE MODEL & UPDATES").monospace().size(11.0).color(text_muted));
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button("Download / Verify whistle.cact (16.9 MB)").clicked() {
                                let dest = resolve_weights_path(&self.cfg);
                                let tx = self.event_tx.clone();
                                self.status_line = "Fetching whistle.cact…".into();
                                std::thread::spawn(move || {
                                    let msg = match fetch_whistle_weights(&dest) {
                                        Ok(p) => format!("Verified whistle.cact at {}", p.display()),
                                        Err(e) => format!("Fetch error: {e}"),
                                    };
                                    let _ = tx.send(UiEvent::Status(msg));
                                });
                            }
                            if ui.button("Check for Updates…").clicked() {
                                let tx = self.event_tx.clone();
                                self.update_line = "Checking GitHub Releases…".into();
                                std::thread::spawn(move || {
                                    let res = check_for_updates(env!("CARGO_PKG_VERSION"));
                                    let _ = tx.send(UiEvent::UpdateResult(res));
                                });
                            }
                        });
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
        .with_inner_size([860.0, 560.0])
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
