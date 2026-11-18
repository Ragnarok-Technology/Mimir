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

/// Status summary for a single calendar app, account, or feed source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarSourceStatus {
    pub name: String,
    pub status: String,
    pub event_count: usize,
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

/// Encodes bytes as standard RFC 4648 Base64 (used for CalDAV HTTP Basic Auth).
pub fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn unescape_xml_entities(input: &str) -> String {
    let trimmed = input.trim();
    let stripped = if let Some(inner) = trimmed
        .strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
    {
        inner
    } else {
        trimmed
    };
    stripped
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&amp;", "&")
}

/// Extracts all `<calendar-data>` / `<C:calendar-data>` / `<cal:calendar-data>` VCALENDAR payloads
/// from a CalDAV `REPORT` XML multistatus response.
pub fn extract_caldav_ics_blocks(xml_body: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let lower = xml_body.to_ascii_lowercase();
    let mut cursor = 0;

    while cursor < lower.len() {
        let Some(rel_open) = lower[cursor..].find("calendar-data") else {
            break;
        };
        let tag_name_pos = cursor + rel_open;
        let Some(open_lt) = lower[..tag_name_pos].rfind('<') else {
            cursor = tag_name_pos + "calendar-data".len();
            continue;
        };
        // Ensure it's an opening tag (`<calendar-data` or `<prefix:calendar-data`), not `</...`
        let between = &lower[open_lt + 1..tag_name_pos];
        if between.starts_with('/') || (!between.is_empty() && !between.ends_with(':')) {
            cursor = tag_name_pos + "calendar-data".len();
            continue;
        }
        let Some(rel_gt) = lower[tag_name_pos..].find('>') else {
            break;
        };
        let content_start = tag_name_pos + rel_gt + 1;
        // Self-closing tag `<calendar-data/>`
        if lower[tag_name_pos..content_start - 1].trim_end().ends_with('/') {
            cursor = content_start;
            continue;
        }

        // Find matching closing tag `</...calendar-data>`
        let mut search_end = content_start;
        let mut found_end = None;
        while let Some(rel_close) = lower[search_end..].find("calendar-data>") {
            let close_name_pos = search_end + rel_close;
            if let Some(close_lt) = lower[content_start..close_name_pos].rfind("</") {
                let abs_close_lt = content_start + close_lt;
                let close_prefix = &lower[abs_close_lt + 2..close_name_pos];
                if close_prefix.is_empty() || close_prefix.ends_with(':') {
                    found_end = Some((abs_close_lt, close_name_pos + "calendar-data>".len()));
                    break;
                }
            }
            search_end = close_name_pos + "calendar-data>".len();
        }

        if let Some((content_end, next_cursor)) = found_end {
            let raw_ics = unescape_xml_entities(&xml_body[content_start..content_end]);
            if !raw_ics.trim().is_empty() {
                blocks.push(raw_ics);
            }
            cursor = next_cursor;
        } else {
            break;
        }
    }

    blocks
}

fn parse_rfc3339_or_date(dt_opt: Option<&str>, date_opt: Option<&str>) -> Option<i64> {
    if let Some(dt_str) = dt_opt {
        if let Ok(dt) = DateTime::parse_from_rfc3339(dt_str.trim()) {
            return Some(dt.with_timezone(&Utc).timestamp());
        }
        // Microsoft Graph often returns naive UTC-ish timestamps like "2026-11-15T22:00:00.0000000"
        let trimmed = dt_str.trim().split('.').next().unwrap_or(dt_str.trim());
        if let Ok(ndt) = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S") {
            return Some(DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc).timestamp());
        }
    }
    if let Some(date_str) = date_opt {
        if let Ok(nd) = NaiveDate::parse_from_str(date_str.trim(), "%Y-%m-%d") {
            let ndt = nd.and_hms_opt(0, 0, 0)?;
            return Some(DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc).timestamp());
        }
    }
    None
}

/// Parses a Google Calendar v3 `/calendars/primary/events` JSON response into `CalendarEvent`s.
pub fn parse_google_calendar_json(json_str: &str, source_label: &str) -> Vec<CalendarEvent> {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return Vec::new();
    };
    let Some(items) = root.get("items").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut events = Vec::new();
    for item in items {
        if item.get("status").and_then(|v| v.as_str()) == Some("cancelled") {
            continue;
        }
        let start_obj = item.get("start");
        let end_obj = item.get("end");
        let Some(start_ts) = parse_rfc3339_or_date(
            start_obj.and_then(|s| s.get("dateTime")).and_then(|v| v.as_str()),
            start_obj.and_then(|s| s.get("date")).and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let end_ts = parse_rfc3339_or_date(
            end_obj.and_then(|s| s.get("dateTime")).and_then(|v| v.as_str()),
            end_obj.and_then(|s| s.get("date")).and_then(|v| v.as_str()),
        )
        .unwrap_or(start_ts + 3600)
        .max(start_ts);

        let title = item
            .get("summary")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Untitled Meeting")
            .to_string();
        let uid = item
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| format!("gcal:{s}"))
            .unwrap_or_else(|| format!("gcal:{start_ts}:{title}"));

        let hangout = item.get("hangoutLink").and_then(|v| v.as_str());
        let conf_uri = item
            .get("conferenceData")
            .and_then(|c| c.get("entryPoints"))
            .and_then(|e| e.as_array())
            .and_then(|arr| {
                arr.iter().find_map(|ep| {
                    let ep_type = ep.get("entryPointType").and_then(|v| v.as_str()).unwrap_or("");
                    if ep_type == "video" || ep_type.is_empty() {
                        ep.get("uri").and_then(|v| v.as_str())
                    } else {
                        None
                    }
                })
            });
        let location = item.get("location").and_then(|v| v.as_str()).unwrap_or("");
        let description = item.get("description").and_then(|v| v.as_str()).unwrap_or("");

        let meeting_url = hangout
            .map(str::to_string)
            .or_else(|| conf_uri.map(str::to_string))
            .or_else(|| extract_meeting_url(location))
            .or_else(|| extract_meeting_url(description));

        let attendees: Vec<String> = item
            .get("attendees")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| {
                        a.get("displayName")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .or_else(|| a.get("email").and_then(|v| v.as_str()).map(str::trim))
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default();

        let notes_or_agenda = if description.trim().is_empty() {
            None
        } else {
            Some(description.trim().to_string())
        };

        events.push(CalendarEvent {
            uid,
            title,
            start_ts,
            end_ts,
            meeting_url,
            attendees,
            notes_or_agenda,
            source: source_label.to_string(),
        });
    }
    events.sort_by_key(|e| e.start_ts);
    events
}

/// Parses a Microsoft Graph `/me/calendarView` JSON response into `CalendarEvent`s.
pub fn parse_microsoft_graph_json(json_str: &str, source_label: &str) -> Vec<CalendarEvent> {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return Vec::new();
    };
    let Some(items) = root.get("value").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut events = Vec::new();
    for item in items {
        if item.get("isCancelled").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        let Some(start_ts) = parse_rfc3339_or_date(
            item.get("start")
                .and_then(|s| s.get("dateTime"))
                .and_then(|v| v.as_str()),
            None,
        ) else {
            continue;
        };
        let end_ts = parse_rfc3339_or_date(
            item.get("end")
                .and_then(|s| s.get("dateTime"))
                .and_then(|v| v.as_str()),
            None,
        )
        .unwrap_or(start_ts + 3600)
        .max(start_ts);

        let title = item
            .get("subject")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Untitled Meeting")
            .to_string();
        let uid = item
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| format!("msgraph:{s}"))
            .unwrap_or_else(|| format!("msgraph:{start_ts}:{title}"));

        let online_url = item
            .get("onlineMeeting")
            .and_then(|m| m.get("joinUrl"))
            .and_then(|v| v.as_str())
            .or_else(|| item.get("onlineMeetingUrl").and_then(|v| v.as_str()));
        let location = item
            .get("location")
            .and_then(|l| l.get("displayName"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let body_preview = item.get("bodyPreview").and_then(|v| v.as_str()).unwrap_or("");

        let meeting_url = online_url
            .map(str::to_string)
            .or_else(|| extract_meeting_url(location))
            .or_else(|| extract_meeting_url(body_preview));

        let attendees: Vec<String> = item
            .get("attendees")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| {
                        let ea = a.get("emailAddress")?;
                        ea.get("name")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .or_else(|| ea.get("address").and_then(|v| v.as_str()).map(str::trim))
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default();

        let notes_or_agenda = if body_preview.trim().is_empty() {
            None
        } else {
            Some(body_preview.trim().to_string())
        };

        events.push(CalendarEvent {
            uid,
            title,
            start_ts,
            end_ts,
            meeting_url,
            attendees,
            notes_or_agenda,
            source: source_label.to_string(),
        });
    }
    events.sort_by_key(|e| e.start_ts);
    events
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

    #[test]
    fn encodes_base64_for_caldav_basic_auth() {
        assert_eq!(base64_encode(b"alice@icloud.com:app-pass"), "YWxpY2VAaWNsb3VkLmNvbTphcHAtcGFzcw==");
        assert_eq!(base64_encode(b"a"), "YQ==");
        assert_eq!(base64_encode(b"ab"), "YWI=");
    }

    #[test]
    fn extracts_caldav_calendar_data_blocks_and_parses_events() {
        let xml = r#"<?xml version="1.0" encoding="utf-8" ?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/cal/ev1.ics</D:href>
    <D:propstat>
      <D:prop>
        <C:calendar-data>BEGIN:VCALENDAR
BEGIN:VEVENT
UID:caldav-1
SUMMARY:iCloud &amp; Fastmail Sync
DTSTART:20261116T180000Z
DTEND:20261116T183000Z
LOCATION:https://acme.zoom.us/j/987654321
END:VEVENT
END:VCALENDAR</C:calendar-data>
      </D:prop>
    </D:propstat>
  </D:response>
</D:multistatus>"#;

        let blocks = extract_caldav_ics_blocks(xml);
        assert_eq!(blocks.len(), 1);
        let events = parse_ics(&blocks[0], "caldav:iCloud");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].title, "iCloud & Fastmail Sync");
        assert_eq!(
            events[0].meeting_url.as_deref(),
            Some("https://acme.zoom.us/j/987654321")
        );
        assert_eq!(events[0].source, "caldav:iCloud");
    }

    #[test]
    fn parses_google_calendar_json_events() {
        let json = r#"{
            "items": [
                {
                    "id": "g1",
                    "status": "confirmed",
                    "summary": "Design Review",
                    "start": { "dateTime": "2026-11-16T19:00:00Z" },
                    "end": { "dateTime": "2026-11-16T19:45:00Z" },
                    "hangoutLink": "https://meet.google.com/abc-defg-hij",
                    "attendees": [
                        { "displayName": "Alice", "email": "alice@example.com" },
                        { "email": "bob@example.com" }
                    ],
                    "description": "Review Obsidian Prism UI"
                },
                {
                    "id": "g2",
                    "status": "cancelled",
                    "summary": "Cancelled Sync",
                    "start": { "dateTime": "2026-11-16T20:00:00Z" },
                    "end": { "dateTime": "2026-11-16T20:30:00Z" }
                }
            ]
        }"#;

        let events = parse_google_calendar_json(json, "google:Work");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid, "gcal:g1");
        assert_eq!(events[0].title, "Design Review");
        assert_eq!(
            events[0].meeting_url.as_deref(),
            Some("https://meet.google.com/abc-defg-hij")
        );
        assert_eq!(events[0].attendees, vec!["Alice", "bob@example.com"]);
        assert_eq!(events[0].source, "google:Work");
    }

    #[test]
    fn parses_microsoft_graph_json_events() {
        let json = r#"{
            "value": [
                {
                    "id": "m1",
                    "subject": "Quarterly Planning",
                    "isCancelled": false,
                    "start": { "dateTime": "2026-11-16T21:00:00.0000000", "timeZone": "UTC" },
                    "end": { "dateTime": "2026-11-16T22:00:00.0000000", "timeZone": "UTC" },
                    "onlineMeeting": { "joinUrl": "https://teams.microsoft.com/l/meetup-join/19%3aabc/0" },
                    "bodyPreview": "Q4 roadmap",
                    "attendees": [
                        { "emailAddress": { "name": "Carol", "address": "carol@contoso.com" } }
                    ]
                },
                {
                    "id": "m2",
                    "subject": "Cancelled",
                    "isCancelled": true,
                    "start": { "dateTime": "2026-11-16T23:00:00.0000000", "timeZone": "UTC" },
                    "end": { "dateTime": "2026-11-16T23:30:00.0000000", "timeZone": "UTC" }
                }
            ]
        }"#;

        let events = parse_microsoft_graph_json(json, "microsoft:Contoso");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid, "msgraph:m1");
        assert_eq!(events[0].title, "Quarterly Planning");
        assert_eq!(
            events[0].meeting_url.as_deref(),
            Some("https://teams.microsoft.com/l/meetup-join/19%3aabc/0")
        );
        assert_eq!(events[0].attendees, vec!["Carol"]);
        assert_eq!(events[0].source, "microsoft:Contoso");
    }
}
