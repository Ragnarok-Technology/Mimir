//! Local session storage (SQLite FTS5 + crash-safe JSONL WAL) and drop-folder sync.

pub mod cleanup;
pub mod export;

use chrono::{DateTime, Utc};
use mimir_core::{
    ChannelId, MimirError, Result, Session, SpeakerInfo, TranscriptFormat, Utterance, WordToken,
    restrict_file_permissions,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Compact summary returned by `list_sessions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: Uuid,
    pub title: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub utterance_count: usize,
    #[serde(default)]
    pub archived: bool,
}

/// Full-text search hit across stored sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub session_id: Uuid,
    pub session_title: Option<String>,
    pub utterance_id: Uuid,
    pub speaker_id: String,
    pub speaker_label: String,
    pub start_ms: u64,
    pub text: String,
}

/// Enrolled local voiceprint persisted in SQLite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredVoiceprint {
    pub label: String,
    pub embedding: Vec<f32>,
}

/// Local SQLite + JSONL persistence layer.
pub struct SessionStore {
    conn: Connection,
    sessions_dir: PathBuf,
}

impl SessionStore {
    /// Opens (or initializes) the SQLite FTS5 database and JSONL WAL directory under `data_dir`.
    pub fn open(data_dir: &Path) -> Result<Self> {
        fs::create_dir_all(data_dir)?;
        let sessions_dir = data_dir.join("sessions");
        fs::create_dir_all(&sessions_dir)?;

        let db_path = data_dir.join("mimir.db");
        let conn = Connection::open(&db_path)
            .map_err(|e| MimirError::Storage(format!("failed to open sqlite db: {e}")))?;
        let _ = restrict_file_permissions(&db_path);

        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                title TEXT,
                started_at TEXT NOT NULL,
                ended_at TEXT,
                languages_json TEXT NOT NULL DEFAULT '[]',
                keywords_json TEXT NOT NULL DEFAULT '[]',
                archived INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS speakers (
                session_id TEXT NOT NULL,
                id TEXT NOT NULL,
                label TEXT NOT NULL,
                channel TEXT NOT NULL,
                is_host INTEGER NOT NULL,
                PRIMARY KEY (session_id, id),
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS utterances (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                speaker_id TEXT NOT NULL,
                channel TEXT NOT NULL,
                start_ms INTEGER NOT NULL,
                end_ms INTEGER NOT NULL,
                text TEXT NOT NULL,
                raw_text TEXT,
                language TEXT NOT NULL,
                avg_confidence REAL NOT NULL,
                words_json TEXT NOT NULL DEFAULT '[]',
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_utterances_session_time
                ON utterances(session_id, start_ms, end_ms);

            CREATE VIRTUAL TABLE IF NOT EXISTS utterances_fts USING fts5(
                text,
                utterance_id UNINDEXED,
                session_id UNINDEXED
            );

            CREATE TABLE IF NOT EXISTS voiceprints (
                label TEXT PRIMARY KEY,
                embedding_json TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            "#,
        )
        .map_err(|e| MimirError::Storage(format!("failed to initialize schema: {e}")))?;

        // Idempotent migrations for pre-existing databases.
        let _ = conn.execute(
            "ALTER TABLE sessions ADD COLUMN archived INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE utterances ADD COLUMN raw_text TEXT",
            [],
        );

        Ok(Self { conn, sessions_dir })
    }

    /// Creates a new session record and registers its initial speakers.
    pub fn create_session(&self, session: &Session) -> Result<()> {
        let langs = serde_json::to_string(&session.languages_detected)
            .map_err(|e| MimirError::Storage(e.to_string()))?;
        let kws = serde_json::to_string(&session.keywords_used)
            .map_err(|e| MimirError::Storage(e.to_string()))?;

        self.conn
            .execute(
                "INSERT INTO sessions (id, title, started_at, ended_at, languages_json, keywords_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    session.id.to_string(),
                    session.title,
                    session.started_at.to_rfc3339(),
                    session.ended_at.map(|d| d.to_rfc3339()),
                    langs,
                    kws,
                ],
            )
            .map_err(|e| MimirError::Storage(e.to_string()))?;

        for spk in session.speakers.values() {
            self.upsert_speaker(session.id, spk)?;
        }

        Ok(())
    }

    /// Inserts or updates a speaker's label in a session.
    pub fn upsert_speaker(&self, session_id: Uuid, speaker: &SpeakerInfo) -> Result<()> {
        let ch = match speaker.channel {
            ChannelId::Mic => "mic",
            ChannelId::System => "system",
        };
        self.conn
            .execute(
                "INSERT INTO speakers (session_id, id, label, channel, is_host)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(session_id, id) DO UPDATE SET label = excluded.label",
                params![
                    session_id.to_string(),
                    speaker.id,
                    speaker.label,
                    ch,
                    i32::from(speaker.is_host),
                ],
            )
            .map_err(|e| MimirError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Renames a speaker in SQLite; returns `true` if updated.
    pub fn rename_speaker(&self, session_id: Uuid, speaker_id: &str, new_label: &str) -> Result<bool> {
        let rows = self
            .conn
            .execute(
                "UPDATE speakers SET label = ?1 WHERE session_id = ?2 AND id = ?3",
                params![new_label, session_id.to_string(), speaker_id],
            )
            .map_err(|e| MimirError::Storage(e.to_string()))?;
        Ok(rows > 0)
    }

    /// Appends an utterance to SQLite, FTS5, and the crash-safe per-session JSONL WAL.
    pub fn append_utterance(&self, session_id: Uuid, utterance: &Utterance) -> Result<()> {
        let ch = match utterance.channel {
            ChannelId::Mic => "mic",
            ChannelId::System => "system",
        };
        let words_json = serde_json::to_string(&utterance.words)
            .map_err(|e| MimirError::Storage(e.to_string()))?;

        self.conn
            .execute(
                "INSERT INTO utterances
                 (id, session_id, speaker_id, channel, start_ms, end_ms, text, raw_text, language, avg_confidence, words_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    utterance.id.to_string(),
                    session_id.to_string(),
                    utterance.speaker_id,
                    ch,
                    utterance.start_ms as i64,
                    utterance.end_ms as i64,
                    utterance.text,
                    utterance.raw_text,
                    utterance.language,
                    utterance.avg_confidence,
                    words_json,
                ],
            )
            .map_err(|e| MimirError::Storage(e.to_string()))?;

        self.conn
            .execute(
                "INSERT INTO utterances_fts (text, utterance_id, session_id) VALUES (?1, ?2, ?3)",
                params![
                    utterance.text,
                    utterance.id.to_string(),
                    session_id.to_string()
                ],
            )
            .map_err(|e| MimirError::Storage(e.to_string()))?;

        let wal_path = self.sessions_dir.join(format!("{session_id}.jsonl"));
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&wal_path)?;
        let line = serde_json::to_string(utterance)
            .map_err(|e| MimirError::Storage(e.to_string()))?;
        writeln!(file, "{line}")?;
        file.flush()?;

        Ok(())
    }

    /// Marks a session as ended and updates detected languages.
    pub fn finish_session(
        &self,
        session_id: Uuid,
        ended_at: DateTime<Utc>,
        languages_detected: &[String],
    ) -> Result<()> {
        let langs = serde_json::to_string(languages_detected)
            .map_err(|e| MimirError::Storage(e.to_string()))?;
        self.conn
            .execute(
                "UPDATE sessions SET ended_at = ?1, languages_json = ?2 WHERE id = ?3",
                params![ended_at.to_rfc3339(), langs, session_id.to_string()],
            )
            .map_err(|e| MimirError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Lists recent non-archived sessions ordered newest-first.
    pub fn list_sessions(&self, limit: usize) -> Result<Vec<SessionSummary>> {
        self.list_sessions_filtered(limit, false)
    }

    /// Lists recent sessions ordered newest-first, optionally including archived sessions.
    pub fn list_sessions_filtered(
        &self,
        limit: usize,
        include_archived: bool,
    ) -> Result<Vec<SessionSummary>> {
        let sql = if include_archived {
            "SELECT s.id, s.title, s.started_at, s.ended_at, COUNT(u.id) as cnt, s.archived
             FROM sessions s
             LEFT JOIN utterances u ON u.session_id = s.id
             GROUP BY s.id
             ORDER BY s.started_at DESC
             LIMIT ?1"
        } else {
            "SELECT s.id, s.title, s.started_at, s.ended_at, COUNT(u.id) as cnt, s.archived
             FROM sessions s
             LEFT JOIN utterances u ON u.session_id = s.id
             WHERE s.archived = 0
             GROUP BY s.id
             ORDER BY s.started_at DESC
             LIMIT ?1"
        };
        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(|e| MimirError::Storage(e.to_string()))?;

        let rows = stmt
            .query_map(params![limit as i64], |row| {
                let id_str: String = row.get(0)?;
                let title: Option<String> = row.get(1)?;
                let started_str: String = row.get(2)?;
                let ended_str: Option<String> = row.get(3)?;
                let count: i64 = row.get(4)?;
                let archived: i64 = row.get(5)?;
                Ok((id_str, title, started_str, ended_str, count, archived != 0))
            })
            .map_err(|e| MimirError::Storage(e.to_string()))?;

        let mut out = Vec::new();
        for r in rows {
            let (id_str, title, started_str, ended_str, count, archived) =
                r.map_err(|e| MimirError::Storage(e.to_string()))?;
            out.push(SessionSummary {
                id: Uuid::parse_str(&id_str)
                    .map_err(|e| MimirError::Storage(e.to_string()))?,
                title,
                started_at: DateTime::parse_from_rfc3339(&started_str)
                    .map_err(|e| MimirError::Storage(e.to_string()))?
                    .with_timezone(&Utc),
                ended_at: ended_str
                    .map(|s| DateTime::parse_from_rfc3339(&s).map(|d| d.with_timezone(&Utc)))
                    .transpose()
                    .map_err(|e| MimirError::Storage(e.to_string()))?,
                utterance_count: count.max(0) as usize,
                archived,
            });
        }
        Ok(out)
    }

    /// Renames a session title. Returns `true` if the session existed.
    pub fn rename_session(&self, session_id: Uuid, new_title: &str) -> Result<bool> {
        let trimmed = new_title.trim();
        let title_val: Option<&str> = if trimmed.is_empty() { None } else { Some(trimmed) };
        let rows = self
            .conn
            .execute(
                "UPDATE sessions SET title = ?1 WHERE id = ?2",
                params![title_val, session_id.to_string()],
            )
            .map_err(|e| MimirError::Storage(e.to_string()))?;
        Ok(rows > 0)
    }

    /// Archives or unarchives a session. Returns `true` if the session existed.
    pub fn set_session_archived(&self, session_id: Uuid, archived: bool) -> Result<bool> {
        let rows = self
            .conn
            .execute(
                "UPDATE sessions SET archived = ?1 WHERE id = ?2",
                params![i32::from(archived), session_id.to_string()],
            )
            .map_err(|e| MimirError::Storage(e.to_string()))?;
        Ok(rows > 0)
    }

    /// Updates an utterance's cleaned text (preserving `raw_text`) and re-indexes FTS5.
    pub fn update_utterance_text(
        &self,
        utterance_id: Uuid,
        new_text: &str,
        raw_text: Option<&str>,
    ) -> Result<()> {
        let uid = utterance_id.to_string();
        self.conn
            .execute(
                "UPDATE utterances SET text = ?1, raw_text = COALESCE(raw_text, ?2) WHERE id = ?3",
                params![new_text, raw_text, &uid],
            )
            .map_err(|e| MimirError::Storage(e.to_string()))?;
        self.conn
            .execute(
                "UPDATE utterances_fts SET text = ?1 WHERE utterance_id = ?2",
                params![new_text, &uid],
            )
            .map_err(|e| MimirError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Runs a cleanup callback across all utterances in a session and persists changed text.
    /// Returns the number of utterances updated.
    pub fn cleanup_session<F>(&self, session_id: Uuid, mut cleaner: F) -> Result<usize>
    where
        F: FnMut(&Utterance) -> Result<Option<String>>,
    {
        let Some(session) = self.get_session(session_id)? else {
            return Err(MimirError::Storage(format!("session {session_id} not found")));
        };
        let mut updated = 0usize;
        for utt in &session.utterances {
            let source = utt.raw_text.as_deref().unwrap_or(&utt.text);
            if let Some(cleaned) = cleaner(utt)? {
                let cleaned = cleaned.trim();
                if !cleaned.is_empty() && cleaned != utt.text {
                    self.update_utterance_text(utt.id, cleaned, Some(source))?;
                    updated += 1;
                }
            }
        }
        Ok(updated)
    }

    /// Permanently deletes a session, its utterances, FTS index entries, and JSONL WAL file.
    pub fn delete_session(&self, session_id: Uuid) -> Result<bool> {
        let id_str = session_id.to_string();
        self.conn
            .execute(
                "DELETE FROM utterances_fts WHERE session_id = ?1",
                params![&id_str],
            )
            .map_err(|e| MimirError::Storage(e.to_string()))?;
        let rows = self
            .conn
            .execute("DELETE FROM sessions WHERE id = ?1", params![&id_str])
            .map_err(|e| MimirError::Storage(e.to_string()))?;
        let wal_path = self.sessions_dir.join(format!("{session_id}.jsonl"));
        if wal_path.exists() {
            let _ = fs::remove_file(wal_path);
        }
        Ok(rows > 0)
    }

    /// Saves or updates an enrolled local speaker voiceprint.
    pub fn save_voiceprint(&self, label: &str, embedding: &[f32]) -> Result<()> {
        let emb_json =
            serde_json::to_string(embedding).map_err(|e| MimirError::Storage(e.to_string()))?;
        self.conn
            .execute(
                "INSERT INTO voiceprints (label, embedding_json, updated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(label) DO UPDATE SET
                   embedding_json = excluded.embedding_json,
                   updated_at = excluded.updated_at",
                params![label.trim(), emb_json, Utc::now().to_rfc3339()],
            )
            .map_err(|e| MimirError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Lists all enrolled local speaker voiceprints.
    pub fn list_voiceprints(&self) -> Result<Vec<StoredVoiceprint>> {
        let mut stmt = self
            .conn
            .prepare("SELECT label, embedding_json FROM voiceprints ORDER BY label ASC")
            .map_err(|e| MimirError::Storage(e.to_string()))?;
        let rows = stmt
            .query_map([], |r| {
                let label: String = r.get(0)?;
                let emb_json: String = r.get(1)?;
                Ok((label, emb_json))
            })
            .map_err(|e| MimirError::Storage(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            let (label, emb_json) = r.map_err(|e| MimirError::Storage(e.to_string()))?;
            let embedding: Vec<f32> = serde_json::from_str(&emb_json).unwrap_or_default();
            if !embedding.is_empty() {
                out.push(StoredVoiceprint { label, embedding });
            }
        }
        Ok(out)
    }

    /// Loads a full `Session` (including speakers and chronologically ordered utterances).
    pub fn get_session(&self, session_id: Uuid) -> Result<Option<Session>> {
        let row: Option<(String, Option<String>, String, Option<String>, String, String)> = self
            .conn
            .query_row(
                "SELECT id, title, started_at, ended_at, languages_json, keywords_json
                 FROM sessions WHERE id = ?1",
                params![session_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .optional()
            .map_err(|e| MimirError::Storage(e.to_string()))?;

        let Some((id_str, title, started_str, ended_str, langs_json, kws_json)) = row else {
            return Ok(None);
        };

        let mut speakers = HashMap::new();
        let mut spk_stmt = self
            .conn
            .prepare(
                "SELECT id, label, channel, is_host FROM speakers WHERE session_id = ?1",
            )
            .map_err(|e| MimirError::Storage(e.to_string()))?;
        let spk_rows = spk_stmt
            .query_map(params![&id_str], |r| {
                let id: String = r.get(0)?;
                let label: String = r.get(1)?;
                let ch_str: String = r.get(2)?;
                let is_host: i32 = r.get(3)?;
                Ok((id, label, ch_str, is_host))
            })
            .map_err(|e| MimirError::Storage(e.to_string()))?;

        for sr in spk_rows {
            let (id, label, ch_str, is_host) =
                sr.map_err(|e| MimirError::Storage(e.to_string()))?;
            let channel = if ch_str == "mic" {
                ChannelId::Mic
            } else {
                ChannelId::System
            };
            speakers.insert(
                id.clone(),
                SpeakerInfo {
                    id,
                    label,
                    channel,
                    is_host: is_host != 0,
                },
            );
        }

        let mut utterances = Vec::new();
        let mut utt_stmt = self
            .conn
            .prepare(
                "SELECT id, speaker_id, channel, start_ms, end_ms, text, raw_text, language, avg_confidence, words_json
                 FROM utterances WHERE session_id = ?1 ORDER BY start_ms ASC, end_ms ASC",
            )
            .map_err(|e| MimirError::Storage(e.to_string()))?;
        let utt_rows = utt_stmt
            .query_map(params![&id_str], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, f32>(8)?,
                    r.get::<_, String>(9)?,
                ))
            })
            .map_err(|e| MimirError::Storage(e.to_string()))?;

        for ur in utt_rows {
            let (uid, speaker_id, ch_str, start_ms, end_ms, text, raw_text, language, avg_confidence, words_json) =
                ur.map_err(|e| MimirError::Storage(e.to_string()))?;
            let words: Vec<WordToken> = serde_json::from_str(&words_json).unwrap_or_default();
            utterances.push(Utterance {
                id: Uuid::parse_str(&uid).map_err(|e| MimirError::Storage(e.to_string()))?,
                speaker_id,
                channel: if ch_str == "mic" {
                    ChannelId::Mic
                } else {
                    ChannelId::System
                },
                start_ms: start_ms.max(0) as u64,
                end_ms: end_ms.max(0) as u64,
                text,
                raw_text,
                language,
                avg_confidence,
                words,
            });
        }

        Ok(Some(Session {
            id: Uuid::parse_str(&id_str).map_err(|e| MimirError::Storage(e.to_string()))?,
            title,
            started_at: DateTime::parse_from_rfc3339(&started_str)
                .map_err(|e| MimirError::Storage(e.to_string()))?
                .with_timezone(&Utc),
            ended_at: ended_str
                .map(|s| DateTime::parse_from_rfc3339(&s).map(|d| d.with_timezone(&Utc)))
                .transpose()
                .map_err(|e| MimirError::Storage(e.to_string()))?,
            languages_detected: serde_json::from_str(&langs_json).unwrap_or_default(),
            keywords_used: serde_json::from_str(&kws_json).unwrap_or_default(),
            speakers,
            utterances,
        }))
    }

    /// Returns the most recently started session, if any.
    pub fn latest_session(&self) -> Result<Option<Session>> {
        let summaries = self.list_sessions(1)?;
        match summaries.first() {
            Some(s) => self.get_session(s.id),
            None => Ok(None),
        }
    }

    /// Full-text searches across all stored utterances using SQLite FTS5.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }

        let mut stmt = self
            .conn
            .prepare(
                "SELECT u.session_id, s.title, u.id, u.speaker_id, COALESCE(sp.label, u.speaker_id), u.start_ms, u.text
                 FROM utterances_fts f
                 JOIN utterances u ON u.id = f.utterance_id
                 JOIN sessions s ON s.id = u.session_id
                 LEFT JOIN speakers sp ON sp.session_id = u.session_id AND sp.id = u.speaker_id
                 WHERE utterances_fts MATCH ?1
                 ORDER BY rank
                 LIMIT ?2",
            )
            .map_err(|e| MimirError::Storage(e.to_string()))?;

        let rows = stmt
            .query_map(params![trimmed, limit as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })
            .map_err(|e| MimirError::Storage(e.to_string()))?;

        let mut hits = Vec::new();
        for r in rows {
            let (sid, title, uid, spk_id, spk_label, start_ms, text) =
                r.map_err(|e| MimirError::Storage(e.to_string()))?;
            hits.push(SearchHit {
                session_id: Uuid::parse_str(&sid).map_err(|e| MimirError::Storage(e.to_string()))?,
                session_title: title,
                utterance_id: Uuid::parse_str(&uid).map_err(|e| MimirError::Storage(e.to_string()))?,
                speaker_id: spk_id,
                speaker_label: spk_label,
                start_ms: start_ms.max(0) as u64,
                text,
            });
        }
        Ok(hits)
    }
}

/// Atomically writes `.md` and `.jsonl` exports of `session` into `drop_dir`.
pub fn sync_to_drop_dir(session: &Session, drop_dir: &Path) -> Result<(PathBuf, PathBuf)> {
    fs::create_dir_all(drop_dir)?;
    let date_prefix = session.started_at.format("%Y-%m-%d_%H%M");
    let stem = format!("{date_prefix}_{}", &session.id.to_string()[..8]);

    let md_path = drop_dir.join(format!("{stem}.md"));
    let jsonl_path = drop_dir.join(format!("{stem}.jsonl"));

    let md_content = export::render_session(session, TranscriptFormat::Markdown)?;
    let jsonl_content = export::render_session(session, TranscriptFormat::Jsonl)?;

    atomic_write(&md_path, md_content.as_bytes())?;
    atomic_write(&jsonl_path, jsonl_content.as_bytes())?;

    Ok((md_path, jsonl_path))
}

fn atomic_write(dest: &Path, data: &[u8]) -> Result<()> {
    let tmp = dest.with_extension("tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, dest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_utterance(channel: ChannelId, speaker_id: &str, start_ms: u64, text: &str) -> Utterance {
        Utterance {
            id: Uuid::new_v4(),
            speaker_id: speaker_id.into(),
            channel,
            start_ms,
            end_ms: start_ms + 1200,
            text: text.into(),
            raw_text: None,
            language: "en".into(),
            avg_confidence: 0.94,
            words: vec![],
        }
    }

    #[test]
    fn store_roundtrip_fts_search_and_rename() {
        let dir = TempDir::new().unwrap();
        let store = SessionStore::open(dir.path()).unwrap();

        let mut session = Session::new(Some("Architecture Review".into()), vec!["Whistle".into()]);
        session.speakers.insert(
            "remote_1".into(),
            SpeakerInfo {
                id: "remote_1".into(),
                label: "Speaker 1".into(),
                channel: ChannelId::System,
                is_host: false,
            },
        );
        store.create_session(&session).unwrap();

        let u1 = sample_utterance(ChannelId::Mic, "host", 1000, "Let's keep Mimir completely local.");
        let u2 = sample_utterance(ChannelId::System, "remote_1", 3000, "Agreed, Whistle runs fast on CPU.");
        store.append_utterance(session.id, &u1).unwrap();
        store.append_utterance(session.id, &u2).unwrap();

        assert!(store.rename_speaker(session.id, "remote_1", "Alice").unwrap());
        store.finish_session(session.id, Utc::now(), &["en".into()]).unwrap();

        let loaded = store.latest_session().unwrap().unwrap();
        assert_eq!(loaded.utterances.len(), 2);
        assert_eq!(loaded.speakers["remote_1"].label, "Alice");

        let hits = store.search("Whistle", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].speaker_label, "Alice");

        let md = export::render_session(&loaded, TranscriptFormat::Markdown).unwrap();
        assert!(md.contains("[00:00:03] **Alice:** Agreed, Whistle runs fast on CPU."));

        let srt = export::render_session(&loaded, TranscriptFormat::Srt).unwrap();
        assert!(srt.contains("00:00:01,000 --> 00:00:02,200"));

        let (md_drop, jsonl_drop) = sync_to_drop_dir(&loaded, &dir.path().join("drop")).unwrap();
        assert!(md_drop.exists());
        assert!(jsonl_drop.exists());

        assert!(store.rename_session(session.id, "Renamed Review").unwrap());
        assert_eq!(
            store.latest_session().unwrap().unwrap().title.as_deref(),
            Some("Renamed Review")
        );

        assert!(store.set_session_archived(session.id, true).unwrap());
        assert!(store.list_sessions(10).unwrap().is_empty());
        assert_eq!(store.list_sessions_filtered(10, true).unwrap().len(), 1);

        assert!(store.delete_session(session.id).unwrap());
        assert!(store.list_sessions_filtered(10, true).unwrap().is_empty());
        assert!(store.search("Whistle", 10).unwrap().is_empty());
    }

    #[test]
    fn encryption_at_rest_roundtrip_and_tamper_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let plain = b"RIFF....WAV_OR_JSONL_PAYLOAD";
        let mut enc = encrypt_bytes_at_rest(dir.path(), plain).unwrap();
        assert_ne!(&enc[..], plain);
        let dec = decrypt_bytes_at_rest(dir.path(), &enc).unwrap();
        assert_eq!(dec, plain);

        let last = enc.len() - 1;
        enc[last] ^= 0xFF;
        assert!(decrypt_bytes_at_rest(dir.path(), &enc).is_err());
    }
}

/// Ensures `dir` exists and has `0700` permissions on Unix.
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

fn load_or_create_at_rest_key(data_dir: &Path) -> Result<[u8; 32]> {
    use sha2::{Digest, Sha256};
    ensure_private_dir(data_dir)?;
    let key_path = data_dir.join("mimir.key");
    if key_path.exists() {
        let bytes = fs::read(&key_path)?;
        if bytes.len() == 32 {
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes);
            return Ok(key);
        }
    }
    let mut hasher = Sha256::new();
    hasher.update(Uuid::new_v4().as_bytes());
    hasher.update(Uuid::new_v4().as_bytes());
    let digest = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&digest);
    fs::write(&key_path, key)?;
    let _ = restrict_file_permissions(&key_path);
    Ok(key)
}

/// Encrypts bytes with ChaCha20-Poly1305 using a local `0600` key in `data_dir`.
/// Format: `"MIMIRENC1"` (9 bytes) || `nonce` (12 bytes) || `ciphertext`.
pub fn encrypt_bytes_at_rest(data_dir: &Path, plaintext: &[u8]) -> Result<Vec<u8>> {
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{ChaCha20Poly1305, Nonce};
    use sha2::{Digest, Sha256};

    let key = load_or_create_at_rest_key(data_dir)?;
    let cipher = ChaCha20Poly1305::new((&key).into());
    let nonce_hash = Sha256::digest(Uuid::new_v4().as_bytes());
    let mut nonce_bytes = [0u8; 12];
    nonce_bytes.copy_from_slice(&nonce_hash[..12]);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
        .map_err(|e| MimirError::Storage(format!("encryption failed: {e}")))?;

    let mut out = Vec::with_capacity(9 + 12 + ciphertext.len());
    out.extend_from_slice(b"MIMIRENC1");
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypts bytes produced by `encrypt_bytes_at_rest`.
pub fn decrypt_bytes_at_rest(data_dir: &Path, blob: &[u8]) -> Result<Vec<u8>> {
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{ChaCha20Poly1305, Nonce};

    if blob.len() < 21 || &blob[..9] != b"MIMIRENC1" {
        return Err(MimirError::Storage("invalid encrypted Mimir blob header".into()));
    }
    let key = load_or_create_at_rest_key(data_dir)?;
    let cipher = ChaCha20Poly1305::new((&key).into());
    cipher
        .decrypt(Nonce::from_slice(&blob[9..21]), &blob[21..])
        .map_err(|e| MimirError::Storage(format!("decryption failed: {e}")))
}
