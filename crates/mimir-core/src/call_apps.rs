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

    // Discord voice/call (includes "Voice Connected" or popout call windows)
    if app_lower.contains("discord")
        && (t_lower.contains("voice") || t_lower.contains("call") || t_lower.contains("huddle"))
    {
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
    const UI_CHROME: [&str; 38] = [
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
        "voice connected",
        "disconnect",
        "deafen",
        "undeafen",
        "leave huddle",
        "invite",
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
        if !cleaned.chars().any(|c| c.is_alphabetic()) {
            continue;
        }
        if cleaned.contains(':') && cleaned.chars().filter(|c| c.is_ascii_digit()).count() >= 2 {
            continue;
        }
        let lower = cleaned.to_ascii_lowercase();
        if UI_CHROME
            .iter()
            .any(|c| lower == *c || lower.starts_with(&format!("{c} (")))
        {
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
    let ps_out = std::process::Command::new("ps")
        .args(["-axo", "comm="])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let ps_lower = ps_out.to_ascii_lowercase();

    let ax_trusted = macos_accessibility_trusted();

    // 1. Check browser tabs (Chrome, Arc, Brave, Edge, Safari) for Meet / Teams / Zoom web.
    if let Some(mut browser_call) = detect_macos_browser_call() {
        if ax_trusted {
            let (participants, speaker) =
                detect_macos_ax_participants_for_app(&browser_call.app_name, self_label);
            browser_call.participants = participants;
            browser_call.active_speaker = speaker;
        }
        return CallDetectionSnapshot {
            active_call: Some(browser_call),
            accessibility_granted: ax_trusted,
            status_note: None,
        };
    }

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
    if ps_lower.contains("facetime") || ps_lower.contains("avconferenced") {
        running_hints.push("facetime");
    }
    if ps_lower.contains("webex") {
        running_hints.push("webex");
    }

    if running_hints.is_empty() {
        return CallDetectionSnapshot {
            active_call: None,
            accessibility_granted: ax_trusted,
            status_note: None,
        };
    }

    // 2. When Accessibility is trusted, inspect native call app windows + participant/speaker labels.
    if ax_trusted
        && let Some(call) = detect_macos_ax_call(self_label)
    {
        return CallDetectionSnapshot {
            active_call: Some(call),
            accessibility_granted: true,
            status_note: None,
        };
    }

    // 3. Process-level fallbacks that only exist during an active call.
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
(function() {
  const chromium = ["Google Chrome", "Arc", "Brave Browser", "Microsoft Edge"];
  for (const name of chromium) {
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
            return JSON.stringify({ app: name, title: title, url: url });
          }
          if (url.includes("teams.microsoft.com") && (title.toLowerCase().includes("meeting") || title.toLowerCase().includes("call"))) {
            return JSON.stringify({ app: name, title: title, url: url });
          }
          if (url.includes("zoom.us/wc/")) {
            return JSON.stringify({ app: name, title: title, url: url });
          }
        }
      }
    } catch (e) {}
  }
  try {
    const safari = Application("Safari");
    if (safari.running()) {
      const wins = safari.windows();
      for (let i = 0; i < wins.length; i++) {
        const tabs = wins[i].tabs();
        for (let j = 0; j < tabs.length; j++) {
          const url = String(tabs[j].url() || "");
          const title = String(tabs[j].name() || "");
          if (url.includes("meet.google.com/") && !url.endsWith("meet.google.com/")) {
            return JSON.stringify({ app: "Safari", title: title, url: url });
          }
          if (url.includes("teams.microsoft.com") && (title.toLowerCase().includes("meeting") || title.toLowerCase().includes("call"))) {
            return JSON.stringify({ app: "Safari", title: title, url: url });
          }
          if (url.includes("zoom.us/wc/")) {
            return JSON.stringify({ app: "Safari", title: title, url: url });
          }
        }
      }
    }
  } catch (e) {}
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
fn detect_macos_ax_participants_for_app(
    app_name: &str,
    self_label: Option<&str>,
) -> (Vec<String>, Option<String>) {
    let escaped = app_name.replace('\\', "\\\\").replace('"', "\\\"");
    let jxa = format!(
        r#"
(function() {{
  const se = Application("System Events");
  const procs = se.processes.whose({{ name: "{escaped}" }})();
  if (!procs || procs.length === 0) return "[]";
  const wins = procs[0].windows();
  if (!wins || wins.length === 0) return "[]";
  const labels = [];
  try {{
    const elems = wins[0].entireContents();
    const cap = Math.min(elems.length, 140);
    for (let k = 0; k < cap; k++) {{
      try {{
        const role = elems[k].role();
        if (role === "AXStaticText" || role === "AXButton" || role === "AXGroup") {{
          const nm = elems[k].name() || elems[k].value() || elems[k].description();
          if (nm && typeof nm === "string" && nm.length >= 2 && nm.length <= 64) {{
            labels.push(nm);
          }}
        }}
      }} catch (e) {{}}
    }}
  }} catch (e) {{}}
  return JSON.stringify(labels);
}})();
"#
    );
    let out = std::process::Command::new("osascript")
        .args(["-l", "JavaScript", "-e", &jxa])
        .output();
    let Ok(o) = out else {
        return (Vec::new(), None);
    };
    if !o.status.success() {
        return (Vec::new(), None);
    }
    let raw = String::from_utf8_lossy(&o.stdout);
    let raw_labels: Vec<String> = serde_json::from_str(raw.trim()).unwrap_or_default();
    extract_participants_and_speaker(&raw_labels, self_label)
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
      let labels = [];
      let looksLikeCall = low.includes("zoom meeting")
        || (pname.toLowerCase().includes("teams") && (low.includes("call") || low.includes("meeting") || low.includes("|")))
        || (pname.toLowerCase().includes("slack") && low.includes("huddle"))
        || (pname.toLowerCase().includes("discord") && (low.includes("voice") || low.includes("call")))
        || (pname.toLowerCase().includes("facetime") && wtitle.length > 0 && low !== "facetime")
        || (pname.toLowerCase().includes("webex") && (low.includes("meeting") || low.includes("call")));

      // For Electron/native apps whose main window title stays static during a call (Discord/Slack),
      // inspect shallow AX labels for "Voice Connected" or "Leave huddle".
      try {
        const elems = wins[w].entireContents();
        const cap = Math.min(elems.length, 140);
        for (let k = 0; k < cap; k++) {
          try {
            const role = elems[k].role();
            if (role === "AXStaticText" || role === "AXButton" || role === "AXGroup") {
              const nm = elems[k].name() || elems[k].value() || elems[k].description();
              if (nm && typeof nm === "string" && nm.length >= 2 && nm.length <= 64) {
                labels.push(nm);
                const nl = nm.toLowerCase();
                if (nl === "voice connected" || nl.includes("leave huddle") || nl === "end call") {
                  looksLikeCall = true;
                }
              }
            }
          } catch (e) {}
        }
      } catch (e) {}
      if (!looksLikeCall) continue;
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
    let raw_labels: Vec<String> = v
        .get("labels")
        .and_then(|l| l.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let mut call = parse_call_from_window_title(app, title).or_else(|| {
        let app_low = app.to_ascii_lowercase();
        let provider = if app_low.contains("discord") {
            CallProvider::Discord
        } else if app_low.contains("slack") {
            CallProvider::Slack
        } else if app_low.contains("teams") {
            CallProvider::MicrosoftTeams
        } else if app_low.contains("zoom") {
            CallProvider::Zoom
        } else if app_low.contains("facetime") {
            CallProvider::FaceTime
        } else if app_low.contains("webex") {
            CallProvider::Webex
        } else {
            return None;
        };
        Some(ActiveCallInfo {
            provider,
            app_name: app.to_string(),
            meeting_title: (!title.trim().is_empty()).then(|| title.trim().to_string()),
            participants: Vec::new(),
            active_speaker: None,
            detected_via: format!("accessibility:{app}"),
        })
    })?;

    let (participants, active_speaker) = extract_participants_and_speaker(&raw_labels, self_label);
    call.participants = participants;
    call.active_speaker = active_speaker;
    call.detected_via = format!("accessibility:{app}");
    Some(call)
}

fn extract_participants_and_speaker(
    raw_labels: &[String],
    self_label: Option<&str>,
) -> (Vec<String>, Option<String>) {
    let mut active_speaker = None;
    for l in raw_labels {
        if l.to_ascii_lowercase().contains("speaking") {
            let cleaned = filter_participant_candidates(std::slice::from_ref(l), self_label);
            if let Some(first) = cleaned.into_iter().next() {
                active_speaker = Some(first);
                break;
            }
        }
    }
    let participants = filter_participant_candidates(raw_labels, self_label);
    (participants, active_speaker)
}

#[cfg(target_os = "windows")]
fn detect_windows_active_call(_self_label: Option<&str>) -> CallDetectionSnapshot {
    let ps = r#"Get-Process | Where-Object { $_.MainWindowTitle -ne '' -or $_.ProcessName -eq 'CptHost' } | Select-Object ProcessName,MainWindowTitle | ConvertTo-Json -Compress"#;
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
                if proc_name.eq_ignore_ascii_case("CptHost") {
                    return CallDetectionSnapshot {
                        active_call: Some(ActiveCallInfo {
                            provider: CallProvider::Zoom,
                            app_name: "Zoom".to_string(),
                            meeting_title: Some("Zoom Meeting".to_string()),
                            participants: Vec::new(),
                            active_speaker: None,
                            detected_via: "process:CptHost".to_string(),
                        }),
                        accessibility_granted: true,
                        status_note: None,
                    };
                }
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
    fn parses_google_meet_zoom_teams_slack_discord_webex_and_facetime() {
        let meet = parse_call_from_window_title("Google Chrome", "Meet - Architecture Review")
            .unwrap();
        assert_eq!(meet.provider, CallProvider::GoogleMeet);
        assert_eq!(meet.meeting_title.as_deref(), Some("Architecture Review"));

        let safari_meet = parse_call_from_window_title("Safari", "Google Meet - abc-defg-hij")
            .unwrap();
        assert_eq!(safari_meet.provider, CallProvider::GoogleMeet);
        assert_eq!(safari_meet.meeting_title.as_deref(), Some("abc-defg-hij"));

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

        let discord = parse_call_from_window_title("Discord", "Voice Connected - Lounge").unwrap();
        assert_eq!(discord.provider, CallProvider::Discord);

        let webex = parse_call_from_window_title("Webex", "Sprint Planning Meeting").unwrap();
        assert_eq!(webex.provider, CallProvider::Webex);

        let facetime = parse_call_from_window_title("FaceTime", "Alice Johnson").unwrap();
        assert_eq!(facetime.provider, CallProvider::FaceTime);
    }

    #[test]
    fn filters_ui_chrome_and_self_from_participant_candidates() {
        let raw = vec![
            "Mute".to_string(),
            "Share Screen".to_string(),
            "Voice Connected".to_string(),
            "Alice Johnson (Host)".to_string(),
            "Bob Smith, Speaking".to_string(),
            "Host".to_string(),
            "12:34".to_string(),
            "Alice Johnson".to_string(),
        ];
        let (filtered, speaker) = extract_participants_and_speaker(&raw, Some("Host"));
        assert_eq!(filtered, vec!["Alice Johnson", "Bob Smith"]);
        assert_eq!(speaker.as_deref(), Some("Bob Smith"));
    }
}
