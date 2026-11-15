mod pipeline;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use mimir_audio::{list_audio_devices, start_dual_capture};
use mimir_bridge::connect::connect_detected_assistants;
use mimir_bridge::http::{ensure_bearer_token, serve_http};
use mimir_bridge::mcp;
use mimir_core::{MimirConfig, Session, TranscriptFormat};
use mimir_diarize::DiarizerConfig;
use mimir_store::{SessionStore, export::render_session};
use mimir_whistle::artifact::{fetch_whistle_weights, resolve_lib_path, resolve_weights_path};
use mimir_whistle::{TranscribeOptions, WhistleEngine};
use std::io::{self, BufReader};
use std::path::PathBuf;
use std::sync::mpsc;
use tokio::sync::broadcast;
use uuid::Uuid;

#[derive(Parser)]
#[command(
    name = "mimir",
    version,
    about = "100% local, covert Whistle AI note-taking assistant"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Clone, Copy, ValueEnum)]
enum FormatArg {
    Md,
    Json,
    Jsonl,
    Srt,
    Vtt,
}

impl From<FormatArg> for TranscriptFormat {
    fn from(v: FormatArg) -> Self {
        match v {
            FormatArg::Md => TranscriptFormat::Markdown,
            FormatArg::Json => TranscriptFormat::Json,
            FormatArg::Jsonl => TranscriptFormat::Jsonl,
            FormatArg::Srt => TranscriptFormat::Srt,
            FormatArg::Vtt => TranscriptFormat::Vtt,
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Show Mimir configuration, model/library paths, and local session count.
    Status,
    /// List available OS audio input and output devices.
    Devices,
    /// Download and SHA-256 verify `whistle.cact` (16.9 MB) into local cache.
    Fetch,
    /// Start live dual-channel meeting/conversation capture and transcription.
    Start {
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        language: Option<String>,
        #[arg(long = "keyword")]
        keywords: Vec<String>,
        #[arg(long)]
        drop_dir: Option<PathBuf>,
        /// Print live utterances to stdout as they are finalized.
        #[arg(long)]
        tail: bool,
        /// Also expose the localhost HTTP/SSE API while recording.
        #[arg(long)]
        serve: bool,
        /// Diarize multiple speakers on the microphone channel (for in-room meetings).
        #[arg(long)]
        diarize_mic: bool,
        /// Read 16 kHz mono f32LE system audio samples from stdin (used by Mimir.app ScreenCaptureKit tap).
        #[arg(long)]
        system_pcm_stdin: bool,
        /// Retain local 16 kHz mono .wav audio files for this session (`FR-4.4`).
        #[arg(long)]
        save_wav: bool,
    },
    /// Stop an active background Mimir capture session.
    Stop,
    /// Tail live utterances from the active/latest session to stdout.
    Tail,
    /// Enroll a speaker voiceprint from a local WAV sample for cross-session recognition.
    Enroll {
        label: String,
        wav_path: PathBuf,
    },
    /// List enrolled local speaker voiceprints.
    Voiceprints,
    /// List recent local transcription sessions.
    Sessions {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        include_archived: bool,
    },
    /// Rename a transcription session title.
    RenameSession {
        session_id: Uuid,
        title: String,
    },
    /// Archive or unarchive a transcription session.
    Archive {
        session_id: Uuid,
        #[arg(long)]
        unarchive: bool,
    },
    /// Permanently delete a transcription session.
    Delete {
        session_id: Uuid,
    },
    /// Print the latest transcription record.
    Latest {
        #[arg(long, value_enum, default_value_t = FormatArg::Md)]
        format: FormatArg,
        /// Copy the rendered transcript directly to the system clipboard.
        #[arg(long)]
        copy: bool,
    },
    /// Export a specific session by UUID.
    Export {
        session_id: Uuid,
        #[arg(long, value_enum, default_value_t = FormatArg::Md)]
        format: FormatArg,
    },
    /// Full-text search across all local transcripts.
    Search {
        query: String,
        #[arg(long, default_value_t = 15)]
        limit: usize,
    },
    /// Rename a diarized speaker in a session (e.g. remote_1 -> Alice).
    RenameSpeaker {
        session_id: Uuid,
        speaker_id: String,
        label: String,
    },
    /// Run the MCP (Model Context Protocol) JSON-RPC 2.0 stdio server for AI assistants.
    Mcp,
    /// Run the localhost-only HTTP REST + SSE API server.
    Serve,
    /// Auto-detect installed AI assistants (Claude, Gemini, Cursor, Antigravity, etc.) and register Mimir MCP.
    Connect {
        #[arg(long)]
        dry_run: bool,
    },
    /// Transcribe a local 16 kHz mono PCM16 WAV file via Whistle.
    TranscribeWav {
        #[arg(long)]
        wav: PathBuf,
        #[arg(long)]
        lib: Option<PathBuf>,
        #[arg(long)]
        weights: Option<PathBuf>,
        #[arg(long)]
        language: Option<String>,
        #[arg(long = "keyword")]
        keywords: Vec<String>,
        #[arg(long)]
        word_timestamps: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mimir=info".into()),
        )
        .with_writer(io::stderr)
        .init();

    let cli = Cli::parse();
    let cfg = MimirConfig::load_or_default()?;

    match cli.command {
        Commands::Status => {
            let weights = resolve_weights_path(&cfg);
            let lib = resolve_lib_path(&cfg);
            let store = SessionStore::open(&cfg.data_dir)?;
            let sessions = store.list_sessions(1000)?;
            println!("Mimir v{}", env!("CARGO_PKG_VERSION"));
            println!("Data dir:       {}", cfg.data_dir.display());
            println!("HTTP bind:      {}", cfg.http_bind_addr);
            println!("Retention:      {:?}", cfg.audio_retention);
            println!(
                "Whistle model:  {} ({})",
                weights.display(),
                if weights.exists() { "present" } else { "missing — run `mimir fetch`" }
            );
            println!(
                "Cactus library: {} ({})",
                lib.display(),
                if lib.exists() { "present" } else { "set CACTUS_LIB_PATH or place in lib/" }
            );
            println!("Sessions:       {}", sessions.len());
        }
        Commands::Devices => {
            for dev in list_audio_devices()? {
                println!("{dev}");
            }
        }
        Commands::Fetch => {
            let dest = resolve_weights_path(&cfg);
            println!("Fetching whistle.cact -> {}", dest.display());
            fetch_whistle_weights(&dest)?;
            println!("Verified SHA-256 OK: {}", dest.display());
        }
        Commands::Start {
            title,
            language,
            keywords,
            drop_dir,
            tail,
            serve,
            diarize_mic,
            system_pcm_stdin,
            save_wav,
        } => {
            let lib_path = resolve_lib_path(&cfg);
            let weights_path = resolve_weights_path(&cfg);
            anyhow::ensure!(
                lib_path.exists(),
                "libcactus not found at {} (set CACTUS_LIB_PATH)",
                lib_path.display()
            );
            anyhow::ensure!(
                weights_path.exists(),
                "whistle.cact not found at {} (run `mimir fetch`)",
                weights_path.display()
            );

            let engine = WhistleEngine::load(&lib_path, &weights_path)?;
            let store = SessionStore::open(&cfg.data_dir)?;
            let mut merged_keywords = cfg.keywords.clone();
            merged_keywords.extend(keywords);

            let mut session = Session::new(title, merged_keywords.clone());
            store.create_session(&session)?;

            let (live_tx, _) = broadcast::channel(64);
            if serve {
                let serve_cfg = cfg.clone();
                let serve_tx = live_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve_http(&serve_cfg, serve_tx).await {
                        eprintln!("http server error: {e}");
                    }
                });
            }

            let (tx, rx) = mpsc::channel();
            let capture = start_dual_capture(tx.clone())?;
            if system_pcm_stdin {
                let sys_tx = tx.clone();
                std::thread::spawn(move || {
                    use std::io::Read;
                    let mut stdin = io::stdin().lock();
                    let mut buf = [0u8; 1280]; // 320 f32 samples = 20 ms at 16 kHz
                    while stdin.read_exact(&mut buf).is_ok() {
                        let samples: Vec<f32> = buf
                            .chunks_exact(4)
                            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                            .collect();
                        if sys_tx
                            .send(mimir_audio::CapturedFrame {
                                channel: mimir_core::ChannelId::System,
                                samples,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                });
            }
            drop(tx);
            eprintln!("Mimir recording session {} (press Ctrl+C to stop)...", session.id);

            let pid_path = cfg.data_dir.join("mimir.pid");
            let _ = std::fs::write(&pid_path, std::process::id().to_string());

            let drop_target = drop_dir.or_else(|| cfg.drop_dir.clone());
            let wav_target = if save_wav
                || matches!(cfg.audio_retention, mimir_core::AudioRetention::ArchiveWav)
            {
                Some(cfg.data_dir.join("audio"))
            } else {
                None
            };
            let opts = TranscribeOptions {
                language: language.or_else(|| cfg.default_language.clone()),
                keywords: merged_keywords,
                word_timestamps: true,
            };

            // Stop capture on Ctrl+C so `rx` closes and `run_pipeline` flushes & exits cleanly.
            let (stop_tx, stop_rx) = std::sync::mpsc::channel();
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                let _ = stop_tx.send(());
            });

            let session_id = session.id;
            std::thread::scope(|s| {
                let handle = s.spawn(move || {
                    pipeline::run_pipeline_with_wav(
                        rx,
                        &engine,
                        &store,
                        &mut session,
                        &opts,
                        DiarizerConfig {
                            host_label: cfg.host_label.clone(),
                            diarize_mic,
                            ..Default::default()
                        },
                        Some(&live_tx),
                        drop_target.as_deref(),
                        wav_target.as_deref(),
                        tail,
                    )
                });
                let _ = stop_rx.recv();
                drop(capture);
                handle.join().expect("pipeline thread panicked")
            })?;

            let _ = std::fs::remove_file(&pid_path);
            eprintln!("Session {} saved.", session_id);
        }
        Commands::Stop => {
            let pid_path = cfg.data_dir.join("mimir.pid");
            let pid_str = std::fs::read_to_string(&pid_path)
                .with_context(|| format!("no active session PID file at {}", pid_path.display()))?;
            let pid: i32 = pid_str.trim().parse().context("invalid PID in mimir.pid")?;
            #[cfg(unix)]
            {
                let status = std::process::Command::new("kill")
                    .arg("-INT")
                    .arg(pid.to_string())
                    .status()?;
                anyhow::ensure!(status.success(), "failed to signal Mimir process {pid}");
            }
            println!("Signaled Mimir process {pid} to stop.");
        }
        Commands::Tail => {
            use std::collections::HashSet;
            use std::io::Write;
            let store = SessionStore::open(&cfg.data_dir)?;
            let mut seen = HashSet::new();
            let (stop_tx, stop_rx) = std::sync::mpsc::channel();
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                let _ = stop_tx.send(());
            });
            loop {
                if let Some(session) = store.latest_session()? {
                    for u in &session.utterances {
                        if seen.insert(u.id) {
                            let label = session
                                .speakers
                                .get(&u.speaker_id)
                                .map(|s| s.label.as_str())
                                .unwrap_or(&u.speaker_id);
                            println!("{}", mimir_bridge::mcp::format_live_utterance_md(label, u));
                            let _ = std::io::stdout().flush();
                        }
                    }
                }
                if stop_rx
                    .recv_timeout(std::time::Duration::from_millis(400))
                    .is_ok()
                {
                    break;
                }
            }
        }
        Commands::Enroll { label, wav_path } => {
            let lib_path = resolve_lib_path(&cfg);
            let weights_path = resolve_weights_path(&cfg);
            anyhow::ensure!(lib_path.exists(), "libcactus not found at {}", lib_path.display());
            anyhow::ensure!(weights_path.exists(), "whistle.cact not found at {}", weights_path.display());
            let engine = WhistleEngine::load(&lib_path, &weights_path)?;
            let samples = read_wav_16khz_mono(&wav_path)?;
            let emb = engine.embed(&samples)?;
            let pooled = mimir_diarize::mean_pool_frames(&emb, 384).unwrap_or(emb);
            let store = SessionStore::open(&cfg.data_dir)?;
            store.save_voiceprint(&label, &pooled)?;
            println!("Enrolled voiceprint '{label}' ({} dims).", pooled.len());
        }
        Commands::Voiceprints => {
            let store = SessionStore::open(&cfg.data_dir)?;
            for vp in store.list_voiceprints()? {
                println!("{} ({} dims)", vp.label, vp.embedding.len());
            }
        }
        Commands::Sessions {
            limit,
            include_archived,
        } => {
            let store = SessionStore::open(&cfg.data_dir)?;
            for s in store.list_sessions_filtered(limit, include_archived)? {
                println!(
                    "{} | {} | {} utterances | {} | {}",
                    s.id,
                    s.started_at.to_rfc3339(),
                    s.utterance_count,
                    if s.archived { "archived" } else { "active" },
                    s.title.as_deref().unwrap_or("Untitled")
                );
            }
        }
        Commands::RenameSession { session_id, title } => {
            let store = SessionStore::open(&cfg.data_dir)?;
            let ok = store.rename_session(session_id, &title)?;
            anyhow::ensure!(ok, "session {session_id} not found");
            println!("Renamed session {session_id}");
        }
        Commands::Archive {
            session_id,
            unarchive,
        } => {
            let store = SessionStore::open(&cfg.data_dir)?;
            let ok = store.set_session_archived(session_id, !unarchive)?;
            anyhow::ensure!(ok, "session {session_id} not found");
            println!(
                "{} session {session_id}",
                if unarchive { "Unarchived" } else { "Archived" }
            );
        }
        Commands::Delete { session_id } => {
            let store = SessionStore::open(&cfg.data_dir)?;
            let ok = store.delete_session(session_id)?;
            anyhow::ensure!(ok, "session {session_id} not found");
            println!("Deleted session {session_id}");
        }
        Commands::Latest { format, copy } => {
            let store = SessionStore::open(&cfg.data_dir)?;
            let session = store
                .latest_session()?
                .context("no transcription sessions found")?;
            let rendered = render_session(&session, format.into())?;
            if copy {
                copy_to_clipboard(&rendered)?;
            }
            println!("{rendered}");
        }
        Commands::Export { session_id, format } => {
            let store = SessionStore::open(&cfg.data_dir)?;
            let session = store
                .get_session(session_id)?
                .with_context(|| format!("session {session_id} not found"))?;
            println!("{}", render_session(&session, format.into())?);
        }
        Commands::Search { query, limit } => {
            let store = SessionStore::open(&cfg.data_dir)?;
            let hits = store.search(&query, limit)?;
            println!("{}", serde_json::to_string_pretty(&hits)?);
        }
        Commands::RenameSpeaker {
            session_id,
            speaker_id,
            label,
        } => {
            let store = SessionStore::open(&cfg.data_dir)?;
            let ok = store.rename_speaker(session_id, &speaker_id, &label)?;
            anyhow::ensure!(ok, "speaker {speaker_id} not found in session {session_id}");
            println!("Updated {speaker_id} -> {label}");
        }
        Commands::Mcp => {
            let stdin = io::stdin();
            let stdout = io::stdout();
            mcp::serve_stdio(&cfg.data_dir, BufReader::new(stdin.lock()), stdout.lock())?;
        }
        Commands::Serve => {
            let token = ensure_bearer_token(&cfg)?;
            let (live_tx, _) = broadcast::channel(64);
            eprintln!("Mimir HTTP API listening on http://{}", cfg.http_bind_addr);
            eprintln!("Bearer token file: {}", cfg.token_path().display());
            let _ = token;
            serve_http(&cfg, live_tx).await?;
        }
        Commands::Connect { dry_run } => {
            let home = dirs::home_dir().context("could not determine home directory")?;
            let exe = std::env::current_exe().context("could not determine mimir binary path")?;
            let outcomes = connect_detected_assistants(&home, &exe, dry_run)?;
            if outcomes.is_empty() {
                println!("No supported assistant configs detected under {}.", home.display());
            } else {
                for o in outcomes {
                    println!("[{}] {} -> {}", o.status, o.name, o.path.display());
                }
            }
        }
        Commands::TranscribeWav {
            wav,
            lib,
            weights,
            language,
            keywords,
            word_timestamps,
        } => {
            let lib_path = lib.unwrap_or_else(|| resolve_lib_path(&cfg));
            let weights_path = weights.unwrap_or_else(|| resolve_weights_path(&cfg));
            let samples = read_wav_16khz_mono(&wav)?;
            let engine = WhistleEngine::load(&lib_path, &weights_path)?;
            let result = engine.transcribe(
                &samples,
                &TranscribeOptions {
                    language,
                    keywords,
                    word_timestamps,
                },
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "text": result.text,
                    "language": result.language,
                    "ttft_ms": result.ttft_ms,
                    "decode_tps": result.decode_tps,
                    "words": result.words,
                }))?
            );
        }
    }

    Ok(())
}

/// Minimal 16-bit PCM WAV reader (expects 16 kHz mono PCM16).
fn read_wav_16khz_mono(path: &PathBuf) -> Result<Vec<f32>> {
    let bytes = std::fs::read(path)?;
    anyhow::ensure!(
        bytes.len() >= 44 && &bytes[0..4] == b"RIFF",
        "not a RIFF WAV file"
    );

    let mut pos = 12usize;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into()?) as usize;
        let data_start = pos + 8;
        let data_end = (data_start + size).min(bytes.len());
        if id == b"data" {
            let samples = bytes[data_start..data_end]
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
                .collect();
            return Ok(samples);
        }
        pos = data_start + size + (size % 2);
    }

    anyhow::bail!("no data chunk found in WAV file")
}

fn copy_to_clipboard(text: &str) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let (cmd, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("pbcopy", &[])
    } else if cfg!(target_os = "windows") {
        ("cmd", &["/C", "clip"])
    } else {
        ("xclip", &["-selection", "clipboard"])
    };
    let mut child = Command::new(cmd).args(args).stdin(Stdio::piped()).spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    let _ = child.wait()?;
    Ok(())
}
