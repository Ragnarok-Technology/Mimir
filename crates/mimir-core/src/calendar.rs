//! Cross-platform local calendar types, RFC 5545 `.ics` parser, and meeting URL extraction.

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

/// Unified local calendar event representation shared across macOS EventKit and `.ics` feeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub uid: String,
    pub title: String,
    pub start_ts: i64,
    pub end_ts: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meeting_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attendees: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_or_agenda: Option<String>,
    pub source: String,
}

/// Extracts the first supported video-conferencing URL (Zoom, Google Meet, Microsoft Teams, Webex)
/// from arbitrary event location, URL, or description text.
pub fn extract_meeting_url(text: &str) -> Option<String> {
    let mut best: Option<(usize, String)> = None;

    for raw_token in text.split(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '(' | ')')) {
        let token = raw_token.trim_end_matches(|c: char| matches!(c, ',' | '.' | ';' | '\\'));
        let lower = token.to_ascii_lowercase();
        let is_http = lower.starts_with("https://") || lower.starts_with("http://");
        let is_zoom_scheme = lower.starts_with("zoommtg://");
        if !is_http && !is_zoom_scheme {
            continue;
        }

        let is_meeting = is_zoom_scheme
            || (lower.contains("zoom.us/") && (lower.contains("/j/") || lower.contains("/my/") || lower.contains("/wc/")))
            || lower.contains("meet.google.com/")
            || lower.contains("teams.microsoft.com/l/meetup-join")
            || lower.contains("teams.live.com/meet/")
            || (lower.contains("webex.com/") && (lower.contains("/meet/") || lower.contains("/join/") || lower.contains("j.php")));

        if is_meeting {
            if let Some(idx) = text.find(token) {
                match &best {
                    Some((prev_idx, _)) if *prev_idx <= idx => {}
                    _ => best = Some((idx, token.to_string())),
                }
            }
        }
    }

    best.map(|(_, url)| url)
}

/// Unfolds RFC 5545 folded lines (`\r\n ` / `\n ` / `\r\n\t` / `\n\t`) into logical lines.
fn unfold_ics_lines(content: &str) -> Vec<String> {
    let normalized = content.replace("\r\n", "\n");
    let mut lines: Vec<String> = Vec::new();
    for raw_line in normalized.split('\n') {
        if let Some(rest) = raw_line.strip_prefix(' ').or_else(|| raw_line.strip_prefix('\t')) {
            if let Some(last) = lines.last_mut() {
                last.push_str(rest);
                continue;
            }
        }
        lines.push(raw_line.to_string());
    }
    lines
}

fn unescape_ics_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek().copied() {
                Some('n') | Some('N') => {
                    out.push('\n');
                    chars.next();
                }
                Some(',') => {
                    out.push(',');
                    chars.next();
                }
                Some(';') => {
                    out.push(';');
                    chars.next();
                }
                Some('\\') => {
                    out.push('\\');
                    chars.next();
                }
                _ => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_ics_timestamp(val: &str) -> Option<i64> {
    let trimmed = val.trim();
    if let Some(utc_str) = trimmed.strip_suffix('Z') {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(utc_str, "%Y%m%dT%H%M%S") {
            return Some(DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc).timestamp());
        }
    }
    if let Ok(ndt) = NaiveDateTime::parse_from_str(trimmed, "%Y%m%dT%H%M%S") {
        // Treat floating / TZID timestamps as local time converted to UTC epoch seconds.
        if let Some(local_dt) = chrono::Local.from_local_datetime(&ndt).single() {
            return Some(local_dt.with_timezone(&Utc).timestamp());
        }
        return Some(DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc).timestamp());
    }
    if let Ok(nd) = NaiveDate::parse_from_str(trimmed, "%Y%m%d") {
        let ndt = nd.and_hms_opt(0, 0, 0)?;
        return Some(DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc).timestamp());
    }
    None
}

fn parse_attendee_label(prop_params: &str, prop_value: &str) -> Option<String> {
    // Check for CN="Alice Smith" or CN=Alice Smith in property parameters.
    for param in prop_params.split(';').skip(1) {
        let trimmed = param.trim();
        if let Some(cn) = trimmed
            .strip_prefix("CN=")
            .or_else(|| trimmed.strip_prefix("cn="))
        {
            let cleaned = cn.trim().trim_matches('"').trim();
            if !cleaned.is_empty() {
                return Some(unescape_ics_text(cleaned));
            }
        }
    }
    let val = prop_value.trim();
    let email = val
        .strip_prefix("mailto:")
        .or_else(|| val.strip_prefix("MAILTO:"))
        .unwrap_or(val)
        .trim();
    if email.is_empty() {
        None
    } else {
        Some(email.to_string())
    }
}

/// Parses an RFC 5545 `.ics` calendar string into a list of `CalendarEvent`s.
pub fn parse_ics(ics_content: &str, source: &str) -> Vec<CalendarEvent> {
    let lines = unfold_ics_lines(ics_content);
    let mut events = Vec::new();
    let mut in_event = false;

    let mut uid = String::new();
    let mut title = String::new();
    let mut start_ts: Option<i64> = None;
    let mut end_ts: Option<i64> = None;
    let mut location = String::new();
    let mut description = String::new();
    let mut url_prop = String::new();
    let mut attendees: Vec<String> = Vec::new();

    for line in lines {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("BEGIN:VEVENT") {
            in_event = true;
            uid.clear();
            title.clear();
            start_ts = None;
            end_ts = None;
            location.clear();
            description.clear();
            url_prop.clear();
            attendees.clear();
            continue;
        }
        if trimmed.eq_ignore_ascii_case("END:VEVENT") {
            if in_event {
                if let Some(start) = start_ts {
                    let end = end_ts.unwrap_or(start + 3600);
                    let meeting_url = extract_meeting_url(&url_prop)
                        .or_else(|| extract_meeting_url(&location))
                        .or_else(|| extract_meeting_url(&description));
                    let notes_or_agenda = {
                        let desc = description.trim();
                        if desc.is_empty() {
                            None
                        } else {
                            Some(desc.to_string())
                        }
                    };
                    let event_uid = if uid.trim().is_empty() {
                        format!("{source}:{start}:{title}")
                    } else {
                        uid.trim().to_string()
                    };
                    let event_title = if title.trim().is_empty() {
                        "Untitled Meeting".to_string()
                    } else {
                        title.trim().to_string()
                    };
                    events.push(CalendarEvent {
                        uid: event_uid,
                        title: event_title,
                        start_ts: start,
                        end_ts: end.max(start),
                        meeting_url,
                        attendees: attendees.clone(),
                        notes_or_agenda,
                        source: source.to_string(),
                    });
                }
            }
            in_event = false;
            continue;
        }
        if !in_event {
            continue;
        }

        let Some((prop_part, value_part)) = line.split_once(':') else {
            continue;
        };
        let prop_name = prop_part.split(';').next().unwrap_or("").trim().to_ascii_uppercase();
        match prop_name.as_str() {
            "UID" => uid = unescape_ics_text(value_part.trim()),
            "SUMMARY" => title = unescape_ics_text(value_part.trim()),
            "DTSTART" => start_ts = parse_ics_timestamp(value_part),
            "DTEND" => end_ts = parse_ics_timestamp(value_part),
            "LOCATION" => location = unescape_ics_text(value_part.trim()),
            "DESCRIPTION" => description = unescape_ics_text(value_part.trim()),
            "URL" => url_prop = value_part.trim().to_string(),
            "ATTENDEE" => {
                if let Some(att) = parse_attendee_label(prop_part, value_part) {
                    if !attendees.iter().any(|a| a.eq_ignore_ascii_case(&att)) {
                        attendees.push(att);
                    }
                }
            }
            _ => {}
        }
    }

    events.sort_by_key(|e| e.start_ts);
    events
}

/// Filters `events` to those currently ongoing or starting within `±window_secs` of `now_ts`.
pub fn upcoming_or_active_events(
    events: &[CalendarEvent],
    now_ts: i64,
    window_secs: i64,
) -> Vec<CalendarEvent> {
    let mut matching: Vec<CalendarEvent> = events
        .iter()
        .filter(|e| {
            let is_ongoing = e.start_ts <= now_ts && now_ts <= e.end_ts;
            let starts_near = (e.start_ts - now_ts).abs() <= window_secs;
            is_ongoing || starts_near
        })
        .cloned()
        .collect();
    matching.sort_by_key(|e| e.start_ts);
    matching
}

/// Opens a URL in the system default browser/handler.
pub fn open_url_in_system_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_zoom_meet_teams_and_webex_urls() {
        assert_eq!(
            extract_meeting_url("Join Zoom: https://acme.zoom.us/j/123456789?pwd=abc please"),
            Some("https://acme.zoom.us/j/123456789?pwd=abc".to_string())
        );
        assert_eq!(
            extract_meeting_url("Google Meet: <https://meet.google.com/abc-defg-hij>"),
            Some("https://meet.google.com/abc-defg-hij".to_string())
        );
        assert_eq!(
            extract_meeting_url("Teams: https://teams.microsoft.com/l/meetup-join/19%3ameeting_abc/0"),
            Some("https://teams.microsoft.com/l/meetup-join/19%3ameeting_abc/0".to_string())
        );
        assert_eq!(
            extract_meeting_url("Webex: https://acme.webex.com/meet/jdoe"),
            Some("https://acme.webex.com/meet/jdoe".to_string())
        );
        assert_eq!(extract_meeting_url("No meeting link here https://example.com"), None);
    }

    #[test]
    fn parses_folded_ics_events_with_attendees_and_window_filtering() {
        let ics = concat!(
            "BEGIN:VCALENDAR\r\n",
            "VERSION:2.0\r\n",
            "BEGIN:VEVENT\r\n",
            "UID:evt-1\r\n",
            "SUMMARY:Mimir Architecture\\, Sync\r\n",
            "DTSTART:20261115T220000Z\r\n",
            "DTEND:20261115T223000Z\r\n",
            "LOCATION:https://meet.google.com/xyz-1234-abc\r\n",
            "DESCRIPTION:Agenda: Review Qwen summaries\\nDiscuss\r\n",
            "  calendar integration.\r\n",
            "ATTENDEE;CN=\"Alice Smith\":mailto:alice@example.com\r\n",
            "ATTENDEE:mailto:bob@example.com\r\n",
            "END:VEVENT\r\n",
            "END:VCALENDAR\r\n"
        );

        let events = parse_ics(ics, "test.ics");
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.uid, "evt-1");
        assert_eq!(ev.title, "Mimir Architecture, Sync");
        assert_eq!(
            ev.meeting_url.as_deref(),
            Some("https://meet.google.com/xyz-1234-abc")
        );
        assert_eq!(ev.attendees, vec!["Alice Smith", "bob@example.com"]);
        assert_eq!(
            ev.notes_or_agenda.as_deref(),
            Some("Agenda: Review Qwen summaries\nDiscuss calendar integration.")
        );

        // Within 15 minutes before start -> included
        assert_eq!(upcoming_or_active_events(&events, ev.start_ts - 600, 900).len(), 1);
        // Ongoing -> included
        assert_eq!(upcoming_or_active_events(&events, ev.start_ts + 900, 900).len(), 1);
        // 2 hours before -> excluded
        assert_eq!(upcoming_or_active_events(&events, ev.start_ts - 7200, 900).len(), 0);
    }
}
