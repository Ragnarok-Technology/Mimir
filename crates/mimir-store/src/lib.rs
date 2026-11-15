//! Local session storage (SQLite FTS5 + crash-safe JSONL WAL) and drop-folder sync.

pub mod cleanup;
pub mod export;
pub mod summarize;

use chrono::{DateTime, Utc};
use mimir_core::{
    ChannelId, MimirConfig, MimirError, Result, Session, SpeakerInfo, TranscriptFormat, Utterance,
    WordToken, restrict_file_permissions,
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
                archived INTEGER NOT NULL DEFAULT 0,
                summary_markdown TEXT,
                summary_template TEXT,
                action_items_json TEXT,
                calendar_event_uid TEXT,
                meeting_url TEXT,
                attendees_json TEXT
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
        for col in [
            "summary_markdown TEXT",
            "summary_template TEXT",
            "action_items_json TEXT",
            "calendar_event_uid TEXT",
            "meeting_url TEXT",
            "attendees_json TEXT",
        ] {
            let sql = format!("ALTER TABLE sessions ADD COLUMN {col}");
            let _ = conn.execute(&sql, []);
        }

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
                "INSERT INTO sessions (
                    id, title, started_at, ended_at, languages_json, keywords_json,
                    summary_markdown, summary_template, action_items_json,
                    calendar_event_uid, meeting_url, attendees_json
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    session.id.to_string(),
                    session.title,
                    session.started_at.to_rfc3339(),
                    session.ended_at.map(|d| d.to_rfc3339()),
                    langs,
                    kws,
                    session.summary_markdown,
                    session.summary_template,
                    session.action_items_json,
                    session.calendar_event_uid,
                    session.meeting_url,
                    session.attendees_json,
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
    pub fn cleanup_session<F>(&self, session_id: Uuid, cleaner: F) -> Result<usize>
    where
        F: FnMut(&Utterance) -> Result<Option<String>>,
    {
        self.cleanup_session_with_progress(session_id, cleaner, |_, _| {})
    }

    /// Runs a cleanup callback across all utterances in a session, invoking `on_progress(done, total)`.
    pub fn cleanup_session_with_progress<F, P>(
        &self,
        session_id: Uuid,
        mut cleaner: F,
        on_progress: P,
    ) -> Result<usize>
    where
        F: FnMut(&Utterance) -> Result<Option<String>>,
        P: FnMut(usize, usize),
    {
        self.cleanup_session_with_context_and_progress(
            session_id,
            |utt, _prev, _next| cleaner(utt),
            on_progress,
        )
    }

    /// Runs a context-aware cleanup callback `(utterance, prev_raw, next_raw)` across a session.
    pub fn cleanup_session_with_context_and_progress<F, P>(
        &self,
        session_id: Uuid,
        mut cleaner: F,
        mut on_progress: P,
    ) -> Result<usize>
    where
        F: FnMut(&Utterance, Option<&str>, Option<&str>) -> Result<Option<String>>,
        P: FnMut(usize, usize),
    {
        let Some(session) = self.get_session(session_id)? else {
            return Err(MimirError::Storage(format!("session {session_id} not found")));
        };
        let total = session.utterances.len();
        on_progress(0, total);
        let mut updated = 0usize;
        for (idx, utt) in session.utterances.iter().enumerate() {
            let source = utt.raw_text.as_deref().unwrap_or(&utt.text);
            let prev = idx
                .checked_sub(1)
                .and_then(|i| session.utterances.get(i))
                .map(|u| u.raw_text.as_deref().unwrap_or(&u.text));
            let next = session
                .utterances
                .get(idx + 1)
                .map(|u| u.raw_text.as_deref().unwrap_or(&u.text));
            if let Some(cleaned) = cleaner(utt, prev, next)? {
                let cleaned = cleaned.trim();
                if !cleaned.is_empty() && cleaned != utt.text {
                    self.update_utterance_text(utt.id, cleaned, Some(source))?;
                    updated += 1;
                }
            }
            on_progress(idx + 1, total);
        }
        Ok(updated)
    }

    /// Reverts all cleaned utterances in a session back to their original `raw_text`.
    pub fn revert_session_cleanup(&self, session_id: Uuid) -> Result<usize> {
        let Some(session) = self.get_session(session_id)? else {
            return Err(MimirError::Storage(format!("session {session_id} not found")));
        };
        let mut reverted = 0usize;
        for utt in &session.utterances {
            if let Some(raw) = &utt.raw_text {
                let uid = utt.id.to_string();
                self.conn
                    .execute(
                        "UPDATE utterances SET text = ?1, raw_text = NULL WHERE id = ?2",
                        params![raw, &uid],
                    )
                    .map_err(|e| MimirError::Storage(e.to_string()))?;
                self.conn
                    .execute(
                        "UPDATE utterances_fts SET text = ?1 WHERE utterance_id = ?2",
                        params![raw, &uid],
                    )
                    .map_err(|e| MimirError::Storage(e.to_string()))?;
                reverted += 1;
            }
        }
        Ok(reverted)
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

    /// Updates a session's summary markdown, template, and extracted action items JSON.
    pub fn update_session_summary(
        &self,
        session_id: Uuid,
        summary_markdown: Option<&str>,
        summary_template: Option<&str>,
        action_items_json: Option<&str>,
    ) -> Result<bool> {
        let rows = self
            .conn
            .execute(
                "UPDATE sessions
                 SET summary_markdown = ?1, summary_template = ?2, action_items_json = ?3
                 WHERE id = ?4",
                params![
                    summary_markdown,
                    summary_template,
                    action_items_json,
                    session_id.to_string()
                ],
            )
            .map_err(|e| MimirError::Storage(e.to_string()))?;
        Ok(rows > 0)
    }

    /// Updates calendar metadata attached to a session.
    pub fn update_session_calendar_meta(
        &self,
        session_id: Uuid,
        calendar_event_uid: Option<&str>,
        meeting_url: Option<&str>,
        attendees_json: Option<&str>,
    ) -> Result<bool> {
        let rows = self
            .conn
            .execute(
                "UPDATE sessions
                 SET calendar_event_uid = ?1, meeting_url = ?2, attendees_json = ?3
                 WHERE id = ?4",
                params![
                    calendar_event_uid,
                    meeting_url,
                    attendees_json,
                    session_id.to_string()
                ],
            )
            .map_err(|e| MimirError::Storage(e.to_string()))?;
        Ok(rows > 0)
    }

    /// Runs local map-reduce summarization, action-item extraction, and conditional auto-titling
    /// using the provided `complete(system, user, max_new_tokens)` callback (e.g. `QwenCleanupModel::complete`).
    pub fn summarize_session_with_progress<F, P>(
        &self,
        session_id: Uuid,
        template: &str,
        mut complete: F,
        mut on_progress: P,
    ) -> Result<summarize::SessionSummaryResult>
    where
        F: FnMut(&str, &str, usize) -> Result<String>,
        P: FnMut(usize, usize),
    {
        let Some(session) = self.get_session(session_id)? else {
            return Err(MimirError::Storage(format!("session {session_id} not found")));
        };

        let norm_template = summarize::normalize_template_name(template).to_string();
        let lines = summarize::format_transcript_lines(&session);
        if lines.is_empty() {
            let empty_json = "[]".to_string();
            self.update_session_summary(
                session_id,
                Some("No spoken utterances recorded in this session."),
                Some(&norm_template),
                Some(&empty_json),
            )?;
            on_progress(1, 1);
            return Ok(summarize::SessionSummaryResult {
                summary_markdown: "No spoken utterances recorded in this session.".to_string(),
                summary_template: norm_template,
                action_items: Vec::new(),
                action_items_json: empty_json,
                auto_title: None,
            });
        }

        let chunks = summarize::chunk_transcript_lines(&lines, summarize::DEFAULT_SUMMARY_CHUNK_CHARS);
        let map_steps = if chunks.len() > 1 { chunks.len() } else { 0 };
        let needs_title = summarize::is_default_or_timestamped_title(session.title.as_deref());
        let total_steps = map_steps + 2 + usize::from(needs_title);
        let mut done = 0usize;
        on_progress(done, total_steps);

        let combined_notes = if chunks.len() > 1 {
            let mut partials = Vec::with_capacity(chunks.len());
            for (idx, chunk) in chunks.iter().enumerate() {
                let (sys, usr) = summarize::build_chunk_map_prompt(chunk, idx, chunks.len());
                let note = complete(&sys, &usr, 220)?;
                if !note.trim().is_empty() {
                    partials.push(note.trim().to_string());
                }
                done += 1;
                on_progress(done, total_steps);
            }
            partials.join("\n\n")
        } else {
            chunks[0].clone()
        };

        let context_hint = session.title.as_deref().filter(|t| !summarize::is_default_or_timestamped_title(Some(t)));
        let (sum_sys, sum_usr) =
            summarize::build_summary_reduce_prompt(&combined_notes, &norm_template, context_hint);
        let summary_markdown = complete(&sum_sys, &sum_usr, 420)?.trim().to_string();
        done += 1;
        on_progress(done, total_steps);

        let speaker_labels: Vec<String> = session
            .speakers
            .values()
            .map(|s| s.label.clone())
            .collect();
        let (act_sys, act_usr) = summarize::build_action_items_prompt(&combined_notes, &speaker_labels);
        let raw_actions = complete(&act_sys, &act_usr, 320)?;
        let action_items = summarize::parse_action_items_response(&raw_actions);
        let action_items_json = serde_json::to_string(&action_items)
            .map_err(|e| MimirError::Storage(e.to_string()))?;
        done += 1;
        on_progress(done, total_steps);

        let auto_title = if needs_title {
            let title_input = if summary_markdown.is_empty() {
                combined_notes.as_str()
            } else {
                summary_markdown.as_str()
            };
            let (title_sys, title_usr) = summarize::build_auto_title_prompt(title_input);
            let raw_title = complete(&title_sys, &title_usr, 32)?;
            let sanitized = summarize::sanitize_auto_title(&raw_title);
            if let Some(ref t) = sanitized {
                let _ = self.rename_session(session_id, t);
            }
            done += 1;
            on_progress(done, total_steps);
            sanitized
        } else {
            None
        };

        self.update_session_summary(
            session_id,
            Some(&summary_markdown),
            Some(&norm_template),
            Some(&action_items_json),
        )?;

        Ok(summarize::SessionSummaryResult {
            summary_markdown,
            summary_template: norm_template,
            action_items,
            action_items_json,
            auto_title,
        })
    }

    /// Loads a full `Session` (including speakers and chronologically ordered utterances).
    pub fn get_session(&self, session_id: Uuid) -> Result<Option<Session>> {
        let row: Option<(
            String,
            Option<String>,
            String,
            Option<String>,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        )> = self
            .conn
            .query_row(
                "SELECT id, title, started_at, ended_at, languages_json, keywords_json,
                        summary_markdown, summary_template, action_items_json,
                        calendar_event_uid, meeting_url, attendees_json
                 FROM sessions WHERE id = ?1",
                params![session_id.to_string()],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                        r.get(10)?,
                        r.get(11)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| MimirError::Storage(e.to_string()))?;

        let Some((
            id_str,
            title,
            started_str,
            ended_str,
            langs_json,
            kws_json,
            summary_markdown,
            summary_template,
            action_items_json,
            calendar_event_uid,
            meeting_url,
            attendees_json,
        )) = row
        else {
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
            summary_markdown,
            summary_template,
            action_items_json,
            calendar_event_uid,
            meeting_url,
            attendees_json,
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

/// Syncs `session_id` to `cfg.drop_dir` when `cfg.drop_sync_enabled` is true and `drop_dir` is set.
pub fn sync_session_to_drop_dir_if_enabled(
    store: &SessionStore,
    session_id: Uuid,
    cfg: &MimirConfig,
) -> Result<Option<(PathBuf, PathBuf)>> {
    if !cfg.drop_sync_enabled {
        return Ok(None);
    }
    let Some(drop_dir) = cfg.drop_dir.as_deref() else {
        return Ok(None);
    };
    let Some(session) = store.get_session(session_id)? else {
        return Ok(None);
    };
    let paths = sync_to_drop_dir(&session, drop_dir)?;
    Ok(Some(paths))
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

    #[test]
    fn summarize_session_persists_summary_actions_autotitle_and_exports() {
        let dir = TempDir::new().unwrap();
        let store = SessionStore::open(dir.path()).unwrap();

        let mut session = Session::new(None, vec!["Mimir".into()]);
        session.calendar_event_uid = Some("cal-evt-42".into());
        session.meeting_url = Some("https://meet.google.com/abc-defg-hij".into());
        session.attendees_json = Some(r#"["Host","Alice"]"#.into());
        store.create_session(&session).unwrap();

        let u1 = sample_utterance(ChannelId::Mic, "host", 1000, "We should ship local Qwen summaries this week.");
        let u2 = sample_utterance(ChannelId::System, "host", 4000, "I will update the SQLite schema by Friday.");
        store.append_utterance(session.id, &u1).unwrap();
        store.append_utterance(session.id, &u2).unwrap();

        let mut progress_calls = Vec::new();
        let res = store
            .summarize_session_with_progress(
                session.id,
                "technical_review",
                |sys, _usr, _max| {
                    if sys.contains("JSON array") {
                        Ok(r#"[{"owner":"Host","task":"Update SQLite schema","due_hint":"Friday","timestamp":"00:00:04"}]"#.into())
                    } else if sys.contains("3 to 7 word") {
                        Ok("Local Qwen Summary Review".into())
                    } else {
                        Ok("### Key Decisions\n- Ship local Qwen summaries this week.".into())
                    }
                },
                |done, total| progress_calls.push((done, total)),
            )
            .unwrap();

        assert_eq!(res.summary_template, "technical_review");
        assert_eq!(res.auto_title.as_deref(), Some("Local Qwen Summary Review"));
        assert_eq!(res.action_items.len(), 1);
        assert_eq!(progress_calls.last(), Some(&(3, 3)));

        let loaded = store.get_session(session.id).unwrap().unwrap();
        assert_eq!(loaded.title.as_deref(), Some("Local Qwen Summary Review"));
        assert_eq!(loaded.calendar_event_uid.as_deref(), Some("cal-evt-42"));
        assert_eq!(
            loaded.meeting_url.as_deref(),
            Some("https://meet.google.com/abc-defg-hij")
        );
        assert!(loaded.summary_markdown.as_deref().unwrap().contains("Key Decisions"));

        let md = export::render_session(&loaded, TranscriptFormat::Markdown).unwrap();
        assert!(md.contains("## Summary (technical_review)"));
        assert!(md.contains("## Action Items"));
        assert!(md.contains("**Host:** Update SQLite schema (due: Friday, @ 00:00:04)"));
        assert!(md.contains("**Meeting URL:** https://meet.google.com/abc-defg-hij"));

        let mut cfg = MimirConfig::default();
        cfg.drop_sync_enabled = true;
        cfg.drop_dir = Some(dir.path().join("drop_sync"));
        let synced = sync_session_to_drop_dir_if_enabled(&store, session.id, &cfg).unwrap();
        assert!(synced.is_some());
        let (md_path, _) = synced.unwrap();
        assert!(std::fs::read_to_string(md_path).unwrap().contains("## Summary (technical_review)"));
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
