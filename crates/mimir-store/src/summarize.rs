//! Local Qwen 2.5 0.5B map-reduce summarization, action-item extraction, and auto-titling.

use mimir_core::{ActionItem, Session};

use crate::export::format_hhmmss;

/// Maximum character budget per prompt chunk for Qwen 2.5 0.5B Instruct.
pub const DEFAULT_SUMMARY_CHUNK_CHARS: usize = 4200;

/// Transcripts at or below this character count under the universal `general` template
/// use a single fast LLM pass instead of multiple sequential passes.
pub const SHORT_TRANSCRIPT_MAX_CHARS: usize = 900;

/// Output produced by `SessionStore::summarize_session_with_progress`.
#[derive(Debug, Clone)]
pub struct SessionSummaryResult {
    pub summary_markdown: String,
    pub summary_template: String,
    pub action_items: Vec<ActionItem>,
    pub action_items_json: String,
    pub auto_title: Option<String>,
}

/// Returns `true` if `title` is missing or looks like a default/timestamped placeholder.
pub fn is_default_or_timestamped_title(title: Option<&str>) -> bool {
    let Some(t) = title.map(str::trim).filter(|s| !s.is_empty()) else {
        return true;
    };
    let lower = t.to_ascii_lowercase();
    if lower == "untitled"
        || lower == "untitled session"
        || lower == "new session"
        || lower == "recording"
    {
        return true;
    }
    if lower.starts_with("session ") || lower.starts_with("recording ") {
        let rest = lower
            .split_once(' ')
            .map(|(_, r)| r.trim())
            .unwrap_or("");
        if rest.chars().all(|c| c.is_ascii_digit() || matches!(c, '-' | '_' | ':' | ' ' | 't' | 'z')) {
            return true;
        }
    }
    // Date-prefixed timestamped title like "2026-11-15 14:00"
    if t.len() >= 10
        && t.as_bytes()[0..4].iter().all(u8::is_ascii_digit)
        && t.as_bytes()[4] == b'-'
        && t.as_bytes()[5..7].iter().all(u8::is_ascii_digit)
        && t.as_bytes()[7] == b'-'
    {
        return true;
    }
    false
}

/// Formats a session's utterances as `[HH:MM:SS] Speaker: text` lines.
pub fn format_transcript_lines(session: &Session) -> Vec<String> {
    session
        .utterances
        .iter()
        .filter(|u| !u.text.trim().is_empty())
        .map(|u| {
            let speaker = session
                .speakers
                .get(&u.speaker_id)
                .map(|s| s.label.as_str())
                .unwrap_or(&u.speaker_id);
            let ts = format_hhmmss(u.start_ms);
            format!("[{ts}] {speaker}: {}", u.text.trim())
        })
        .collect()
}

/// Splits transcript lines into chunks of at most `max_chars` without breaking lines.
pub fn chunk_transcript_lines(lines: &[String], max_chars: usize) -> Vec<String> {
    if lines.is_empty() {
        return Vec::new();
    }
    let budget = max_chars.max(500);
    let mut chunks = Vec::new();
    let mut current = String::new();

    for line in lines {
        if !current.is_empty() && current.len() + line.len() + 1 > budget {
            chunks.push(current);
            current = String::new();
        }
        if !current.is_empty() {
            current.push('\n');
        }
        if line.len() > budget {
            // Truncate an unusually huge single utterance safely at char boundary.
            let mut end = budget;
            while !line.is_char_boundary(end) && end > 0 {
                end -= 1;
            }
            current.push_str(&line[..end]);
        } else {
            current.push_str(line);
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

pub fn normalize_template_name(template: &str) -> &'static str {
    match template.trim().to_ascii_lowercase().as_str() {
        "meeting" | "meeting_notes" => "meeting",
        "1on1" | "1-on-1" | "one_on_one" => "1on1",
        "standup" | "daily" => "standup",
        "sales_discovery" | "sales" | "discovery" => "sales_discovery",
        "technical_review" | "tech" | "technical" | "architecture" => "technical_review",
        _ => "general",
    }
}

pub fn template_sections_hint(template: &str) -> &'static str {
    match normalize_template_name(template) {
        "meeting" => "Use these Markdown headings: ### Key Decisions, ### Discussion Points, ### Open Questions, ### Next Steps.",
        "1on1" => "Use these Markdown headings: ### Check-in & Wins, ### Feedback & Blockers, ### Open Questions, ### Next Steps.",
        "standup" => "Use these Markdown headings: ### Completed, ### In Progress, ### Blockers, ### Next Steps.",
        "sales_discovery" => "Use these Markdown headings: ### Customer Context & Pain Points, ### Requirements & Objections, ### Key Decisions, ### Next Steps.",
        "technical_review" => "Use these Markdown headings: ### Key Decisions, ### Architecture & Trade-offs, ### Open Questions, ### Next Steps.",
        _ => "Match the actual content (voice note, conversation, lecture, video, or meeting). Use concise bullet points or a brief paragraph. Only include headings if the content clearly benefits from them; never force meeting structure onto non-meeting audio.",
    }
}

pub fn build_chunk_map_prompt(chunk: &str, chunk_idx: usize, total_chunks: usize) -> (String, String) {
    let system = "Summarize the key points, facts, questions, and any explicit commitments from this transcript segment in 3-6 crisp bullet points. Preserve speaker names and timestamps when relevant. Do not invent facts or assume it is a meeting if it is not.";
    let user = format!("Transcript segment {}/{}:\n\n{}", chunk_idx + 1, total_chunks, chunk);
    (system.to_string(), user)
}

pub fn build_summary_reduce_prompt(
    transcript_or_notes: &str,
    template: &str,
    context_hint: Option<&str>,
) -> (String, String) {
    let norm = normalize_template_name(template);
    let sections = template_sections_hint(norm);
    let ctx = context_hint
        .map(|c| format!(" Topic/Title: {c}."))
        .unwrap_or_default();
    let role = if norm == "general" {
        "You are a concise, accurate summarizer."
    } else {
        "You are an executive note-taker."
    };
    let system = format!(
        "{role} Produce a clean, factual Markdown summary strictly grounded in the transcript.{ctx} {sections} Keep it proportional to the input length and do not invent details."
    );
    (system, transcript_or_notes.to_string())
}

pub fn build_action_items_prompt(transcript_or_notes: &str, speakers: &[String]) -> (String, String) {
    let speaker_hint = if speakers.is_empty() {
        String::new()
    } else {
        format!(" Known speakers: {}.", speakers.join(", "))
    };
    let system = format!(
        "Extract ONLY explicit tasks, commitments, or follow-ups stated in the transcript.{speaker_hint} Output ONLY a valid JSON array of objects with keys \"owner\" (string or null), \"task\" (string), \"due_hint\" (string or null), and \"timestamp\" (string like \"00:04:12\" or null). If none were stated, immediately output []."
    );
    (system, transcript_or_notes.to_string())
}

pub fn build_auto_title_prompt(summary_or_transcript: &str) -> (String, String) {
    let system = "Generate a concise 3 to 7 word title describing the main topic of this transcript. Output ONLY the title text with no quotes, no markdown, and no prefix.";
    (system.to_string(), summary_or_transcript.to_string())
}

/// Single-pass prompt for short transcripts under the universal `general` mode.
pub fn build_short_universal_summary_prompt(
    transcript: &str,
    needs_title: bool,
    context_hint: Option<&str>,
) -> (String, String) {
    let ctx = context_hint
        .map(|c| format!(" Topic/Title: {c}."))
        .unwrap_or_default();
    let title_instr = if needs_title {
        "Start with \"Title: <3-6 word title>\" on the first line, then a blank line, then the summary. "
    } else {
        ""
    };
    let system = format!(
        "You are a concise, accurate summarizer.{ctx} {title_instr}Summarize this short transcript in 1-3 brief sentences or bullet points, matching what the speaker actually said (note, thought, conversation, or clip—do not force meeting-notes formatting). Only add an \"Action Items:\" bullet list at the end if explicit tasks or commitments were stated; otherwise omit it. Do not invent facts."
    );
    (system, transcript.to_string())
}

/// Parses output from `build_short_universal_summary_prompt` into `(auto_title, summary_markdown, action_items)`.
pub fn parse_short_universal_response(raw: &str, needs_title: bool) -> (Option<String>, String, Vec<ActionItem>) {
    let mut lines: Vec<&str> = raw.trim().lines().collect();
    let mut auto_title = None;

    if needs_title && !lines.is_empty() {
        let first = lines[0].trim();
        if let Some(rest) = first
            .strip_prefix("Title:")
            .or_else(|| first.strip_prefix("title:"))
            .or_else(|| first.strip_prefix("Topic:"))
        {
            auto_title = sanitize_auto_title(rest);
            lines.remove(0);
            while lines.first().is_some_and(|l| l.trim().is_empty()) {
                lines.remove(0);
            }
        }
    }

    let mut summary_lines = Vec::new();
    let mut action_items = Vec::new();
    let mut in_actions = false;

    for line in lines {
        let trimmed = line.trim();
        let lower = trimmed.trim_start_matches('#').trim().to_ascii_lowercase();
        if lower == "action items:" || lower == "action items" || lower == "next steps:" {
            in_actions = true;
            continue;
        }
        if in_actions {
            let task = trimmed
                .strip_prefix("- [ ] ")
                .or_else(|| trimmed.strip_prefix("- "))
                .or_else(|| trimmed.strip_prefix("* "))
                .or_else(|| trimmed.strip_prefix("• "))
                .map(str::trim);
            if let Some(t) = task {
                if !t.is_empty() && !t.eq_ignore_ascii_case("none") && !t.eq_ignore_ascii_case("n/a") {
                    action_items.push(ActionItem {
                        owner: None,
                        task: t.to_string(),
                        due_hint: None,
                        timestamp: None,
                    });
                }
            } else if !trimmed.is_empty() {
                in_actions = false;
                summary_lines.push(line);
            }
        } else {
            summary_lines.push(line);
        }
    }

    let summary_markdown = summary_lines.join("\n").trim().to_string();
    (auto_title, summary_markdown, action_items)
}

/// Heuristic check to avoid running an extra action-items LLM pass on monologues/notes with no task cues.
pub fn should_extract_action_items(text: &str, speaker_count: usize) -> bool {
    if speaker_count > 1 {
        return true;
    }
    let lower = text.to_ascii_lowercase();
    const CUES: [&str; 14] = [
        "will ",
        "need to ",
        "needs to ",
        "have to ",
        "should ",
        "todo",
        "action item",
        "follow up",
        "follow-up",
        "remind me",
        "deadline",
        "by monday",
        "by friday",
        "next step",
    ];
    CUES.iter().any(|cue| lower.contains(cue))
}

/// Parses Qwen's action-items response into `Vec<ActionItem>`, tolerating code fences or bullet fallback.
pub fn parse_action_items_response(raw: &str) -> Vec<ActionItem> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    // Strip optional ```json ... ``` fences.
    let unfenced = trimmed
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    if let (Some(start), Some(end)) = (unfenced.find('['), unfenced.rfind(']')) {
        if start <= end {
            let slice = &unfenced[start..=end];
            if let Ok(items) = serde_json::from_str::<Vec<ActionItem>>(slice) {
                if !items.is_empty() || slice.trim() == "[]" {
                    return items
                        .into_iter()
                        .filter(|i| !i.task.trim().is_empty())
                        .collect();
                }
            }
        }
    }

    // Fallback: parse markdown/numbered bullets if the 0.5B model didn't emit valid JSON.
    let mut fallback = Vec::new();
    for line in unfenced.lines() {
        let l = line.trim();
        let rest = l
            .strip_prefix("- [ ] ")
            .or_else(|| l.strip_prefix("- "))
            .or_else(|| l.strip_prefix("* "))
            .or_else(|| l.strip_prefix("• "));
        if let Some(task) = rest {
            let t = task.trim();
            if !t.is_empty() && !t.eq_ignore_ascii_case("none") {
                fallback.push(ActionItem {
                    owner: None,
                    task: t.to_string(),
                    due_hint: None,
                    timestamp: None,
                });
            }
        }
    }
    fallback
}

/// Sanitizes an auto-generated session title.
pub fn sanitize_auto_title(raw: &str) -> Option<String> {
    let first_line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())?;
    let mut cleaned = first_line
        .trim_start_matches('#')
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim();
    for prefix in ["Title:", "title:", "Meeting Title:", "Topic:"] {
        if let Some(rest) = cleaned.strip_prefix(prefix) {
            cleaned = rest.trim().trim_matches('"');
        }
    }
    if cleaned.is_empty() || cleaned.eq_ignore_ascii_case("untitled") {
        return None;
    }
    let truncated: String = cleaned.chars().take(80).collect();
    Some(truncated.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_default_and_timestamped_titles() {
        assert!(is_default_or_timestamped_title(None));
        assert!(is_default_or_timestamped_title(Some("")));
        assert!(is_default_or_timestamped_title(Some("Untitled Session")));
        assert!(is_default_or_timestamped_title(Some("Session 20261115")));
        assert!(is_default_or_timestamped_title(Some("2026-11-15 14:00")));
        assert!(!is_default_or_timestamped_title(Some("Q4 Platform Roadmap")));
    }

    #[test]
    fn chunks_transcript_respecting_line_boundaries() {
        let lines: Vec<String> = (0..10)
            .map(|i| format!("[00:00:{i:02}] Host: Discussing item number {i} in detail."))
            .collect();
        let chunks = chunk_transcript_lines(&lines, 180);
        assert!(chunks.len() >= 2);
        for c in &chunks {
            assert!(c.len() <= 500);
        }
    }

    #[test]
    fn parses_fenced_json_and_bullet_fallback_action_items() {
        let fenced = "```json\n[{\"owner\":\"Alice\",\"task\":\"Send roadmap\",\"due_hint\":\"Friday\",\"timestamp\":\"00:02:10\"}]\n```";
        let items = parse_action_items_response(fenced);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].owner.as_deref(), Some("Alice"));
        assert_eq!(items[0].task, "Send roadmap");

        let bullets = "- Update SQLite schema\n- [ ] Test macOS build";
        let fallback = parse_action_items_response(bullets);
        assert_eq!(fallback.len(), 2);
        assert_eq!(fallback[0].task, "Update SQLite schema");
        assert_eq!(fallback[1].task, "Test macOS build");
    }

    #[test]
    fn sanitizes_auto_title() {
        assert_eq!(
            sanitize_auto_title("Title: \"Local Qwen Roadmap Sync\"\nExtra line"),
            Some("Local Qwen Roadmap Sync".to_string())
        );
        assert_eq!(sanitize_auto_title("   "), None);
    }

    #[test]
    fn parses_short_universal_response_without_forcing_meeting_format() {
        let raw = "Title: Quick Grocery Reminder\n\nPick up oat milk and coffee beans on the way home.\n\nAction Items:\n- Buy oat milk\n- Buy coffee beans";
        let (title, summary, actions) = parse_short_universal_response(raw, true);
        assert_eq!(title.as_deref(), Some("Quick Grocery Reminder"));
        assert_eq!(summary, "Pick up oat milk and coffee beans on the way home.");
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].task, "Buy oat milk");
    }

    #[test]
    fn skips_action_items_for_plain_monologue_without_cues() {
        assert!(!should_extract_action_items("Just thinking out loud about how nice the weather is today.", 1));
        assert!(should_extract_action_items("I need to send the invoice by Friday.", 1));
        assert!(should_extract_action_items("Hello there.", 2));
    }
}
