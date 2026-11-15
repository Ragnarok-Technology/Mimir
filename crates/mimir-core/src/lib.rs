//! Core domain types, configuration, and errors shared across all Mimir crates.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use thiserror::Error;
use uuid::Uuid;

/// Whistle operates strictly on 16 kHz mono audio.
pub const WHISTLE_SAMPLE_RATE: u32 = 16_000;

/// Supported languages for base `whistle.cact`.
pub const WHISTLE_LANGUAGES: [&str; 7] = ["en", "de", "fr", "es", "it", "nl", "pl"];

#[derive(Debug, Error)]
pub enum MimirError {
    #[error("audio capture error: {0}")]
    Audio(String),

    #[error("whistle engine error: {0}")]
    Whistle(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, MimirError>;

/// Identifies which OS audio stream an audio chunk or utterance came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelId {
    /// Local host microphone (deterministic Host attribution).
    Mic,
    /// System speaker/output loopback (remote participants).
    System,
}

/// A normalized 16 kHz mono PCM chunk ready for VAD or Whistle inference.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub channel: ChannelId,
    /// Milliseconds since the recording session started.
    pub start_ms: u64,
    pub sample_rate: u32,
    pub samples: Vec<f32>,
}

/// A single word with timing and decoder probability from Whistle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WordToken {
    pub word: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub probability: f32,
}

/// Metadata for a tracked speaker within a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeakerInfo {
    pub id: String,
    pub label: String,
    pub channel: ChannelId,
    pub is_host: bool,
}

/// A single transcribed turn/utterance in a session timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Utterance {
    pub id: Uuid,
    pub speaker_id: String,
    pub channel: ChannelId,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_text: Option<String>,
    pub language: String,
    pub avg_confidence: f32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<WordToken>,
}

/// Complete meeting/conversation transcription record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: Uuid,
    pub title: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub languages_detected: Vec<String>,
    pub keywords_used: Vec<String>,
    pub speakers: HashMap<String, SpeakerInfo>,
    pub utterances: Vec<Utterance>,
}

impl Session {
    pub fn new(title: Option<String>, keywords: Vec<String>) -> Self {
        let mut speakers = HashMap::new();
        speakers.insert(
            "host".to_string(),
            SpeakerInfo {
                id: "host".to_string(),
                label: "Host".to_string(),
                channel: ChannelId::Mic,
                is_host: true,
            },
        );

        Self {
            id: Uuid::new_v4(),
            title,
            started_at: Utc::now(),
            ended_at: None,
            languages_detected: Vec::new(),
            keywords_used: keywords,
            speakers,
            utterances: Vec::new(),
        }
    }
}

/// Output formats supported by Mimir exporters and assistant bridges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptFormat {
    Markdown,
    Json,
    Jsonl,
    Srt,
    Vtt,
}

/// How raw audio buffers are handled after transcription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AudioRetention {
    /// Ephemeral in-memory only; discarded immediately after transcription.
    #[default]
    Ephemeral,
    /// Opt-in local WAV archive alongside session records.
    ArchiveWav,
}

/// Runtime configuration for Mimir.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MimirConfig {
    pub data_dir: PathBuf,
    pub drop_dir: Option<PathBuf>,
    pub whistle_weights: Option<PathBuf>,
    pub cactus_lib_path: Option<PathBuf>,
    pub host_label: String,
    pub default_language: Option<String>,
    pub keywords: Vec<String>,
    pub audio_retention: AudioRetention,
    pub http_bind_addr: String,
    #[serde(default)]
    pub auto_detect_calls: bool,
    #[serde(default)]
    pub encrypt_at_rest: bool,
    #[serde(default)]
    pub speaker_onnx_path: Option<PathBuf>,
    #[serde(default)]
    pub vad_onnx_path: Option<PathBuf>,
    #[serde(default)]
    pub cleanup_enabled: bool,
    #[serde(default)]
    pub cleanup_endpoint: Option<String>,
    #[serde(default)]
    pub cleanup_model: Option<String>,
}

impl Default for MimirConfig {
    fn default() -> Self {
        let data_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("mimir");
        Self {
            data_dir,
            drop_dir: None,
            whistle_weights: None,
            cactus_lib_path: None,
            host_label: "Host".to_string(),
            default_language: None,
            keywords: Vec::new(),
            audio_retention: AudioRetention::Ephemeral,
            http_bind_addr: "127.0.0.1:7331".to_string(),
            auto_detect_calls: false,
            encrypt_at_rest: false,
            speaker_onnx_path: None,
            vad_onnx_path: None,
            cleanup_enabled: false,
            cleanup_endpoint: None,
            cleanup_model: None,
        }
    }
}

impl MimirConfig {
    /// Returns the path to the JSON config file inside `data_dir`.
    pub fn config_path(data_dir: &std::path::Path) -> PathBuf {
        data_dir.join("config.json")
    }

    /// Loads config from `data_dir/config.json` if present, otherwise returns `Self::default()`.
    pub fn load_or_default() -> Result<Self> {
        let default = Self::default();
        let path = Self::config_path(&default.data_dir);
        if !path.exists() {
            return Ok(default);
        }
        let raw = std::fs::read_to_string(&path)?;
        serde_json::from_str(&raw)
            .map_err(|e| MimirError::Config(format!("invalid {}: {e}", path.display())))
    }

    /// Persists config to `self.data_dir/config.json` with restrictive permissions on Unix.
    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;
        let path = Self::config_path(&self.data_dir);
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| MimirError::Config(format!("failed to serialize config: {e}")))?;
        std::fs::write(&path, json)?;
        restrict_file_permissions(&path)?;
        Ok(())
    }

    /// Resolves the SQLite database path inside `data_dir`.
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("mimir.db")
    }

    /// Resolves the local HTTP bearer token file path inside `data_dir`.
    pub fn token_path(&self) -> PathBuf {
        self.data_dir.join("api.token")
    }
}

/// Sets `0600` permissions on Unix for sensitive local files (tokens, config, DB).
pub fn restrict_file_permissions(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_initializes_with_host_speaker() {
        let session = Session::new(Some("Standup".into()), vec!["Mimir".into()]);
        assert_eq!(session.title.as_deref(), Some("Standup"));
        assert!(session.speakers.contains_key("host"));
        assert!(session.speakers["host"].is_host);
        assert_eq!(session.speakers["host"].channel, ChannelId::Mic);
    }

    #[test]
    fn default_retention_is_ephemeral() {
        let cfg = MimirConfig::default();
        assert_eq!(cfg.audio_retention, AudioRetention::Ephemeral);
        assert_eq!(cfg.http_bind_addr, "127.0.0.1:7331");
    }
}
