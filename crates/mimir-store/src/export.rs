//! Assistant-optimized and standard transcript format renderers.

use mimir_core::{ActionItem, MimirError, Result, Session, TranscriptFormat};
use std::fmt::Write as _;

/// Renders a `Session` into the requested `TranscriptFormat`.
pub fn render_session(session: &Session, format: TranscriptFormat) -> Result<String> {
    match format {
        TranscriptFormat::Markdown => Ok(render_markdown(session)),
        TranscriptFormat::Json => serde_json::to_string_pretty(session)
            .map_err(|e| MimirError::Storage(format!("json export failed: {e}"))),
        TranscriptFormat::Jsonl => render_jsonl(session),
        TranscriptFormat::Srt => Ok(render_srt(session)),
        TranscriptFormat::Vtt => Ok(render_vtt(session)),
    }
}

/// Selects which text representation to render in Markdown exports and UI views.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TranscriptView {
    #[default]
    Cleaned,
    Raw,
    Diff,
}

/// Renders a `Session` into Markdown using the specified `TranscriptView`.
pub fn render_markdown_with_view(session: &Session, view: TranscriptView) -> String {
    let mut out = String::new();
    let title = session.title.as_deref().unwrap_or("Untitled Session");
    let _ = writeln!(out, "# {title}");
    let _ = writeln!(out);
    let _ = writeln!(out, "- **Session ID:** `{}`", session.id);
    let _ = writeln!(out, "- **Started:** {}", session.started_at.to_rfc3339());
    if let Some(ended) = session.ended_at {
        let _ = writeln!(out, "- **Ended:** {}", ended.to_rfc3339());
    }
    if !session.languages_detected.is_empty() {
        let _ = writeln!(
            out,
            "- **Languages:** {}",
            session.languages_detected.join(", ")
        );
    }
    if let Some(url) = session.meeting_url.as_deref().filter(|s| !s.trim().is_empty()) {
        let _ = writeln!(out, "- **Meeting URL:** {url}");
    }
    if let Some(att_json) = session.attendees_json.as_deref()
        && let Ok(attendees) = serde_json::from_str::<Vec<String>>(att_json)
        && !attendees.is_empty()
    {
        let _ = writeln!(out, "- **Attendees:** {}", attendees.join(", "));
    }

    if let Some(summary) = session
        .summary_markdown
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let _ = writeln!(out);
        if let Some(tmpl) = session.summary_template.as_deref().filter(|s| !s.is_empty()) {
            let _ = writeln!(out, "## Summary ({tmpl})");
        } else {
            let _ = writeln!(out, "## Summary");
        }
        let _ = writeln!(out);
        let _ = writeln!(out, "{summary}");
    }

    if let Some(items_json) = session.action_items_json.as_deref()
        && let Ok(items) = serde_json::from_str::<Vec<ActionItem>>(items_json)
        && !items.is_empty()
    {
        let _ = writeln!(out);
        let _ = writeln!(out, "## Action Items");
        let _ = writeln!(out);
        for item in items {
            let owner = item
                .owner
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|o| format!("**{o}:** "))
                .unwrap_or_default();
            let mut meta_parts = Vec::new();
            if let Some(due) = item.due_hint.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                meta_parts.push(format!("due: {due}"));
            }
            if let Some(ts) = item.timestamp.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                meta_parts.push(format!("@ {ts}"));
            }
            let suffix = if meta_parts.is_empty() {
                String::new()
            } else {
                format!(" ({})", meta_parts.join(", "))
            };
            let _ = writeln!(out, "- [ ] {owner}{}{suffix}", item.task.trim());
        }
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "## Transcript");
    let _ = writeln!(out);

    for u in &session.utterances {
        let speaker_label = session
            .speakers
            .get(&u.speaker_id)
            .map(|s| s.label.as_str())
            .unwrap_or(&u.speaker_id);
        let ts = format_hhmmss(u.start_ms);
        match view {
            TranscriptView::Cleaned => {
                let _ = writeln!(out, "[{ts}] **{speaker_label}:** {}", u.text.trim());
            }
            TranscriptView::Raw => {
                let raw = u.raw_text.as_deref().unwrap_or(&u.text).trim();
                let _ = writeln!(out, "[{ts}] **{speaker_label}:** {raw}");
            }
            TranscriptView::Diff => {
                if let Some(raw) = &u.raw_text {
                    let _ = writeln!(out, "[{ts}] **{speaker_label}:**");
                    let _ = writeln!(out, "  - raw:     {}", raw.trim());
                    let _ = writeln!(out, "  - cleaned: {}", u.text.trim());
                } else {
                    let _ = writeln!(out, "[{ts}] **{speaker_label}:** {}", u.text.trim());
                }
            }
        }
    }

    out
}

pub fn render_markdown(session: &Session) -> String {
    render_markdown_with_view(session, TranscriptView::Cleaned)
}

fn render_jsonl(session: &Session) -> Result<String> {
    let mut out = String::new();
    for u in &session.utterances {
        let line = serde_json::to_string(u)
            .map_err(|e| MimirError::Storage(format!("jsonl export failed: {e}")))?;
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}

fn render_srt(session: &Session) -> String {
    let mut out = String::new();
    for (idx, u) in session.utterances.iter().enumerate() {
        let speaker_label = session
            .speakers
            .get(&u.speaker_id)
            .map(|s| s.label.as_str())
            .unwrap_or(&u.speaker_id);
        let _ = writeln!(out, "{}", idx + 1);
        let _ = writeln!(
            out,
            "{} --> {}",
            format_sub_ts(u.start_ms, ','),
            format_sub_ts(u.end_ms.max(u.start_ms + 1), ',')
        );
        let _ = writeln!(out, "{speaker_label}: {}", u.text.trim());
        let _ = writeln!(out);
    }
    out
}

fn render_vtt(session: &Session) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for u in &session.utterances {
        let speaker_label = session
            .speakers
            .get(&u.speaker_id)
            .map(|s| s.label.as_str())
            .unwrap_or(&u.speaker_id);
        let _ = writeln!(
            out,
            "{} --> {}",
            format_sub_ts(u.start_ms, '.'),
            format_sub_ts(u.end_ms.max(u.start_ms + 1), '.')
        );
        let _ = writeln!(out, "<v {speaker_label}>{}", u.text.trim());
        let _ = writeln!(out);
    }
    out
}

pub fn format_hhmmss(ms: u64) -> String {
    let total_secs = ms / 1000;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

fn format_sub_ts(ms: u64, decimal_sep: char) -> String {
    let total_secs = ms / 1000;
    let millis = ms % 1000;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    format!("{h:02}:{m:02}:{s:02}{decimal_sep}{millis:03}")
}
