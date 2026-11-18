//! Local, zero-bot OS call-app detection (Zoom, Teams, Google Meet, Slack, Discord, FaceTime, Webex).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallProvider {
    Zoom,
    MicrosoftTeams,
    GoogleMeet,
    Slack,
    Discord,
    FaceTime,
    Webex,
}

impl CallProvider {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Zoom => "Zoom",
            Self::MicrosoftTeams => "Microsoft Teams",
            Self::GoogleMeet => "Google Meet",
            Self::Slack => "Slack",
            Self::Discord => "Discord",
            Self::FaceTime => "FaceTime",
            Self::Webex => "Webex",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveCallInfo {
    pub provider: CallProvider,
    pub app_name: String,
    pub meeting_title: Option<String>,
    pub participants: Vec<String>,
    pub active_speaker: Option<String>,
    pub detected_via: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CallDetectionSnapshot {
    pub active_call: Option<ActiveCallInfo>,
    pub accessibility_granted: bool,
    pub status_note: Option<String>,
}

/// Detects an active call from a browser tab / window title (cross-platform).
pub fn parse_call_from_window_title(app_name: &str, title: &str) -> Option<ActiveCallInfo> {
    let t = title.trim();
    if t.is_empty() {
        return None;
    }
    let app_lower = app_name.to_ascii_lowercase();
    let t_lower = t.to_ascii_lowercase();

    // Google Meet in browser: "Meet - <code/title>" or "Google Meet - <title>"
    if t_lower.starts_with("meet - ") || t_lower.starts_with("google meet - ") {
        let meeting = t
            .split_once(" - ")
            .map(|(_, r)| r.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        return Some(ActiveCallInfo {
            provider: CallProvider::GoogleMeet,
            app_name: app_name.to_string(),
            meeting_title: meeting,
            participants: Vec::new(),
            active_speaker: None,
            detected_via: format!("window:{app_name}"),
        });
    }

    // Zoom native or web client
    if (app_lower.contains("zoom") && !t_lower.eq("zoom workplace") && !t_lower.eq("zoom"))
        && (t_lower.contains("zoom meeting")
            || t_lower.contains("meeting")
            || t_lower.contains("webinar"))
    {
        let meeting = t
            .strip_prefix("Zoom Meeting")
            .or_else(|| Some(t))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        return Some(ActiveCallInfo {
            provider: CallProvider::Zoom,
            app_name: "Zoom".to_string(),
            meeting_title: meeting,
            participants: Vec::new(),
            active_speaker: None,
            detected_via: format!("window:{app_name}"),
        });
    }

    // Microsoft Teams native or web
    if (app_lower.contains("teams") || t_lower.contains("microsoft teams"))
        && (t_lower.contains("call") || t_lower.contains("meeting") || t_lower.contains("|"))
        && !t_lower.starts_with("activity |")
        && !t_lower.starts_with("chat |")
        && !t_lower.starts_with("calendar |")
    {
        let meeting = t
            .split('|')
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("microsoft teams"))
            .map(|s| s.to_string());
        return Some(ActiveCallInfo {
            provider: CallProvider::MicrosoftTeams,
            app_name: "Microsoft Teams".to_string(),
            meeting_title: meeting,
            participants: Vec::new(),
            active_speaker: None,
            detected_via: format!("window:{app_name}"),
        });
    }

    // Slack Huddle
    if app_lower.contains("slack") && t_lower.contains("huddle") {
        return Some(ActiveCallInfo {
            provider: CallProvider::Slack,
            app_name: "Slack".to_string(),
            meeting_title: Some(t.to_string()),
            participants: Vec::new(),
            active_speaker: None,
            detected_via: format!("window:{app_name}"),
        });
    }

    // Discord voice/call
    if app_lower.contains("discord") && (t_lower.contains("voice") || t_lower.contains("call")) {
        return Some(ActiveCallInfo {
            provider: CallProvider::Discord,
            app_name: "Discord".to_string(),
            meeting_title: Some(t.to_string()),
            participants: Vec::new(),
            active_speaker: None,
            detected_via: format!("window:{app_name}"),
        });
    }

    // Webex
    if app_lower.contains("webex") && (t_lower.contains("meeting") || t_lower.contains("call")) {
        return Some(ActiveCallInfo {
            provider: CallProvider::Webex,
            app_name: "Webex".to_string(),
            meeting_title: Some(t.to_string()),
            participants: Vec::new(),
            active_speaker: None,
            detected_via: format!("window:{app_name}"),
        });
    }

    // FaceTime
    if app_lower.contains("facetime") && !t_lower.eq("facetime") {
        return Some(ActiveCallInfo {
            provider: CallProvider::FaceTime,
            app_name: "FaceTime".to_string(),
            meeting_title: Some(t.to_string()),
            participants: Vec::new(),
            active_speaker: None,
            detected_via: format!("window:{app_name}"),
        });
    }

    None
}

/// Filters raw Accessibility / UI text labels down to plausible participant display names.
pub fn filter_participant_candidates(raw_labels: &[String], self_label: Option<&str>) -> Vec<String> {
    const UI_CHROME: [&str; 32] = [
        "mute",
        "unmute",
        "audio",
        "video",
        "start video",
        "stop video",
        "participants",
        "chat",
        "share",
        "share screen",
        "record",
        "leave",
        "end",
        "reactions",
        "security",
        "apps",
        "more",
        "view",
        "speaker view",
        "gallery view",
        "settings",
        "minimize",
        "maximize",
        "close",
        "search",
        "raise hand",
        "lower hand",
        "captions",
        "turn on captions",
        "background",
        "present now",
        "you",
    ];

    let mut out: Vec<String> = Vec::new();
    for raw in raw_labels {
        let cleaned = raw
            .trim()
            .trim_end_matches("(Host)")
            .trim_end_matches("(Me)")
            .trim_end_matches("(You)")
            .trim_end_matches(", Speaking")
            .trim_end_matches("is speaking")
            .trim();
        if cleaned.len() < 2 || cleaned.len() > 48 {
            continue;
        }
        // Reject timestamps, counts, or purely numeric/symbol strings.
        if !cleaned.chars().any(|c| c.is_alphabetic()) {
            continue;
        }
        if cleaned.contains(':') && cleaned.chars().filter(|c| c.is_ascii_digit()).count() >= 2 {
            continue;
        }
        let lower = cleaned.to_ascii_lowercase();
        if UI_CHROME.iter().any(|c| lower == *c || lower.starts_with(&format!("{c} ("))) {
            continue;
        }
        if let Some(me) = self_label
            && lower == me.trim().to_ascii_lowercase()
        {
            continue;
        }
        if !out.iter().any(|existing| existing.eq_ignore_ascii_case(cleaned)) {
            out.push(cleaned.to_string());
            if out.len() >= 24 {
                break;
            }
        }
    }
    out
}

/// Opens macOS Accessibility Privacy settings so the user can grant UI inspection permission.
pub fn open_accessibility_settings() {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            .spawn();
    }
}

/// Probes the local OS for an active call and (when permitted) visible participant/speaker names.
pub fn detect_active_call(self_label: Option<&str>) -> CallDetectionSnapshot {
    #[cfg(target_os = "macos")]
    {
        return detect_macos_active_call(self_label);
    }
    #[cfg(target_os = "windows")]
    {
        return detect_windows_active_call(self_label);
    }
    #[cfg(target_os = "linux")]
    {
        return detect_linux_active_call(self_label);
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = self_label;
        CallDetectionSnapshot::default()
    }
}

#[cfg(target_os = "macos")]
fn detect_macos_active_call(self_label: Option<&str>) -> CallDetectionSnapshot {
    // 1. Fast check for active FaceTime media daemon or Zoom CptHost (screen-share/call helper)
    //    and running call/browser processes.
    let ps_out = std::process::Command::new("ps")
        .args(["-axo", "comm="])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let ps_lower = ps_out.to_ascii_lowercase();

    let mut running_hints: Vec<&'static str> = Vec::new();
    if ps_lower.contains("zoom.us") || ps_lower.contains("cpthost") {
        running_hints.push("zoom");
    }
    if ps_lower.contains("microsoft teams") || ps_lower.contains("msteams") {
        running_hints.push("teams");
    }
    if ps_lower.contains("slack") {
        running_hints.push("slack");
    }
    if ps_lower.contains("discord") {
        running_hints.push("discord");
    }
    if ps_lower.contains("facetime") {
        running_hints.push("facetime");
    }
    if ps_lower.contains("webex") {
        running_hints.push("webex");
    }

    // 2. Check browser tabs for Google Meet / Teams / Zoom web without requiring Accessibility.
    if let Some(browser_call) = detect_macos_browser_call() {
        return CallDetectionSnapshot {
            active_call: Some(browser_call),
            accessibility_granted: macos_accessibility_trusted(),
            status_note: None,
        };
    }

    let ax_trusted = macos_accessibility_trusted();
    if running_hints.is_empty() {
        return CallDetectionSnapshot {
            active_call: None,
            accessibility_granted: ax_trusted,
            status_note: None,
        };
    }

    // 3. When Accessibility is trusted, inspect call app windows + participant/speaker labels via JXA.
    if ax_trusted {
        if let Some(call) = detect_macos_ax_call(self_label) {
            return CallDetectionSnapshot {
                active_call: Some(call),
                accessibility_granted: true,
                status_note: None,
            };
        }
    }

    // 4. Fallback: Zoom CptHost is only spawned during an active Zoom meeting.
    if ps_lower.contains("cpthost") {
        return CallDetectionSnapshot {
            active_call: Some(ActiveCallInfo {
                provider: CallProvider::Zoom,
                app_name: "Zoom".to_string(),
                meeting_title: Some("Zoom Meeting".to_string()),
                participants: Vec::new(),
                active_speaker: None,
                detected_via: "process:CptHost".to_string(),
            }),
            accessibility_granted: ax_trusted,
            status_note: (!ax_trusted).then(|| {
                "Grant Accessibility permission in Settings for Zoom participant names".to_string()
            }),
        };
    }

    CallDetectionSnapshot {
        active_call: None,
        accessibility_granted: ax_trusted,
        status_note: None,
    }
}

#[cfg(target_os = "macos")]
fn macos_accessibility_trusted() -> bool {
    let out = std::process::Command::new("osascript")
        .args([
            "-l",
            "JavaScript",
            "-e",
            "ObjC.import('ApplicationServices'); $.AXIsProcessTrusted();",
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim() == "true",
        _ => false,
    }
}

#[cfg(target_os = "macos")]
fn detect_macos_browser_call() -> Option<ActiveCallInfo> {
    let jxa = r#"
const apps = ["Google Chrome", "Arc", "Brave Browser", "Microsoft Edge"];
for (const name of apps) {
  try {
    const app = Application(name);
    if (!app.running()) continue;
    const wins = app.windows();
    for (let i = 0; i < wins.length; i++) {
      const tabs = wins[i].tabs();
      for (let j = 0; j < tabs.length; j++) {
        const url = String(tabs[j].url() || "");
        const title = String(tabs[j].title() || "");
        if (url.includes("meet.google.com/") && !url.endsWith("meet.google.com/")) {
          JSON.stringify({ app: name, title: title, url: url });
          throw "FOUND";
        }
        if (url.includes("teams.microsoft.com") && (title.toLowerCase().includes("meeting") || title.toLowerCase().includes("call"))) {
          JSON.stringify({ app: name, title: title, url: url });
          throw "FOUND";
        }
        if (url.includes("zoom.us/wc/")) {
          JSON.stringify({ app: name, title: title, url: url });
          throw "FOUND";
        }
      }
    }
  } catch (e) {
    if (e === "FOUND") break;
  }
}
"#;
    // Wrap so the found JSON is returned cleanly as script output.
    let script = format!(
        "(function(){{ let res = ''; {jxa_body} return res; }})()",
        jxa_body = jxa.replace(
            "JSON.stringify({ app: name, title: title, url: url });",
            "res = JSON.stringify({ app: name, title: title, url: url });"
        )
    );
    let out = std::process::Command::new("osascript")
        .args(["-l", "JavaScript", "-e", &script])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let app = v.get("app")?.as_str()?;
    let title = v.get("title")?.as_str().unwrap_or("");
    let url = v.get("url")?.as_str().unwrap_or("");
    if url.contains("meet.google.com/") {
        let cleaned_title = title
            .strip_prefix("Meet - ")
            .or_else(|| title.strip_prefix("Google Meet - "))
            .unwrap_or(title)
            .trim();
        return Some(ActiveCallInfo {
            provider: CallProvider::GoogleMeet,
            app_name: app.to_string(),
            meeting_title: (!cleaned_title.is_empty()).then(|| cleaned_title.to_string()),
            participants: Vec::new(),
            active_speaker: None,
            detected_via: format!("browser_tab:{app}"),
        });
    }
    parse_call_from_window_title(app, title)
}

#[cfg(target_os = "macos")]
fn detect_macos_ax_call(self_label: Option<&str>) -> Option<ActiveCallInfo> {
    let jxa = r#"
(function() {
  const se = Application("System Events");
  const targets = ["zoom.us", "Microsoft Teams", "Slack", "Discord", "FaceTime", "Webex"];
  const procs = se.processes();
  for (let i = 0; i < procs.length; i++) {
    const p = procs[i];
    let pname = "";
    try { pname = p.name(); } catch (e) { continue; }
    if (!targets.some(t => pname.toLowerCase().includes(t.toLowerCase()))) continue;
    let wins = [];
    try { wins = p.windows(); } catch (e) { continue; }
    for (let w = 0; w < wins.length; w++) {
      let wtitle = "";
      try { wtitle = String(wins[w].name() || ""); } catch (e) {}
      const low = wtitle.toLowerCase();
      const looksLikeCall = low.includes("zoom meeting")
        || (pname.toLowerCase().includes("teams") && (low.includes("call") || low.includes("meeting") || low.includes("|")))
        || (pname.toLowerCase().includes("slack") && low.includes("huddle"))
        || (pname.toLowerCase().includes("discord") && (low.includes("voice") || low.includes("call")))
        || (pname.toLowerCase().includes("facetime") && wtitle.length > 0 && low !== "facetime")
        || (pname.toLowerCase().includes("webex") && (low.includes("meeting") || low.includes("call")));
      if (!looksLikeCall) continue;
      let labels = [];
      try {
        const elems = wins[w].entireContents();
        const cap = Math.min(elems.length, 160);
        for (let k = 0; k < cap; k++) {
          try {
            const role = elems[k].role();
            if (role === "AXStaticText" || role === "AXButton" || role === "AXGroup") {
              const nm = elems[k].name() || elems[k].value() || elems[k].description();
              if (nm && typeof nm === "string" && nm.length >= 2 && nm.length <= 64) {
                labels.push(nm);
              }
            }
          } catch (e) {}
        }
      } catch (e) {}
      return JSON.stringify({ app: pname, title: wtitle, labels: labels });
    }
  }
  return "";
})();
"#;
    let out = std::process::Command::new("osascript")
        .args(["-l", "JavaScript", "-e", jxa])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let app = v.get("app")?.as_str()?;
    let title = v.get("title")?.as_str().unwrap_or("");
    let mut call = parse_call_from_window_title(app, title)?;
    let raw_labels: Vec<String> = v
        .get("labels")
        .and_then(|l| l.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let mut active_speaker = None;
    for l in &raw_labels {
        if l.to_ascii_lowercase().contains("speaking") {
            let cleaned = filter_participant_candidates(std::slice::from_ref(l), self_label);
            if let Some(first) = cleaned.into_iter().next() {
                active_speaker = Some(first);
                break;
            }
        }
    }
    call.participants = filter_participant_candidates(&raw_labels, self_label);
    call.active_speaker = active_speaker;
    call.detected_via = format!("accessibility:{app}");
    Some(call)
}

#[cfg(target_os = "windows")]
fn detect_windows_active_call(_self_label: Option<&str>) -> CallDetectionSnapshot {
    let ps = r#"Get-Process | Where-Object { $_.MainWindowTitle -ne '' } | Select-Object ProcessName,MainWindowTitle | ConvertTo-Json -Compress"#;
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", ps])
        .output();
    if let Ok(o) = out
        && o.status.success()
    {
        let raw = String::from_utf8_lossy(&o.stdout);
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(raw.trim()) {
            let items = if let Some(arr) = val.as_array() {
                arr.clone()
            } else {
                vec![val]
            };
            for item in items {
                let proc_name = item
                    .get("ProcessName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let title = item
                    .get("MainWindowTitle")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if let Some(call) = parse_call_from_window_title(proc_name, title) {
                    return CallDetectionSnapshot {
                        active_call: Some(call),
                        accessibility_granted: true,
                        status_note: None,
                    };
                }
            }
        }
    }
    CallDetectionSnapshot {
        active_call: None,
        accessibility_granted: true,
        status_note: None,
    }
}

#[cfg(target_os = "linux")]
fn detect_linux_active_call(_self_label: Option<&str>) -> CallDetectionSnapshot {
    if let Ok(o) = std::process::Command::new("wmctrl").arg("-l").output()
        && o.status.success()
    {
        let raw = String::from_utf8_lossy(&o.stdout);
        for line in raw.lines() {
            // Format: <id> <desk> <host> <title...>
            let parts: Vec<&str> = line.splitn(4, char::is_whitespace).collect();
            if parts.len() == 4 {
                let title = parts[3].trim();
                if let Some(call) = parse_call_from_window_title("desktop", title) {
                    return CallDetectionSnapshot {
                        active_call: Some(call),
                        accessibility_granted: true,
                        status_note: None,
                    };
                }
            }
        }
    }
    CallDetectionSnapshot {
        active_call: None,
        accessibility_granted: true,
        status_note: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_google_meet_zoom_teams_and_slack_windows() {
        let meet = parse_call_from_window_title("Google Chrome", "Meet - Architecture Review")
            .unwrap();
        assert_eq!(meet.provider, CallProvider::GoogleMeet);
        assert_eq!(meet.meeting_title.as_deref(), Some("Architecture Review"));

        let zoom = parse_call_from_window_title("zoom.us", "Zoom Meeting").unwrap();
        assert_eq!(zoom.provider, CallProvider::Zoom);

        let teams = parse_call_from_window_title(
            "Microsoft Teams",
            "Weekly Sync | Microsoft Teams",
        )
        .unwrap();
        assert_eq!(teams.provider, CallProvider::MicrosoftTeams);
        assert_eq!(teams.meeting_title.as_deref(), Some("Weekly Sync"));

        let slack = parse_call_from_window_title("Slack", "Huddle in #eng-core").unwrap();
        assert_eq!(slack.provider, CallProvider::Slack);
    }

    #[test]
    fn filters_ui_chrome_and_self_from_participant_candidates() {
        let raw = vec![
            "Mute".to_string(),
            "Share Screen".to_string(),
            "Alice Johnson (Host)".to_string(),
            "Bob Smith, Speaking".to_string(),
            "Host".to_string(),
            "12:34".to_string(),
            "Alice Johnson".to_string(),
        ];
        let filtered = filter_participant_candidates(&raw, Some("Host"));
        assert_eq!(filtered, vec!["Alice Johnson", "Bob Smith"]);
    }
}
