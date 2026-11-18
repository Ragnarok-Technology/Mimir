//! Cross-platform calendar application integrations:
//! - macOS: Apple Calendar (`EventKit` via bundled `mimir-calendar` helper)
//! - Windows: Microsoft Outlook (COM) + Windows Calendar (`AppointmentManager` WinRT)
//! - Linux: GNOME Calendar / Evolution Data Server + KDE KOrganizer
//! - Cross-platform: Mozilla Thunderbird (`cal_events` SQLite), CalDAV (`REPORT`),
//!   Google Calendar API, Microsoft Graph / Outlook 365 API, and `.ics` feeds/files.

use chrono::{DateTime, Duration, Utc};
use mimir_core::calendar::{
    CalendarEvent, CalendarSourceStatus, base64_encode, extract_caldav_ics_blocks,
    extract_meeting_url, parse_google_calendar_json, parse_ics, parse_microsoft_graph_json,
    upcoming_or_active_events,
};
use mimir_core::{CalDavAccount, MimirConfig, OAuthCalendarAccount};
use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Combined result returned to the UI after polling all calendar apps and feeds.
#[derive(Debug, Clone)]
pub struct CalendarRefreshResult {
    pub events: Vec<CalendarEvent>,
    pub statuses: Vec<CalendarSourceStatus>,
    pub apple_status: Option<String>,
    pub updated_oauth_accounts: Option<Vec<OAuthCalendarAccount>>,
}

/// Polls all OS-native calendar apps, Thunderbird, CalDAV, Google/Microsoft OAuth, and `.ics` feeds.
pub fn fetch_all_calendar_events(
    cfg: &MimirConfig,
    request_macos_access: bool,
) -> CalendarRefreshResult {
    let now = Utc::now();
    let now_ts = now.timestamp();
    let mut all_events: Vec<CalendarEvent> = Vec::new();
    let mut statuses: Vec<CalendarSourceStatus> = Vec::new();
    #[cfg(target_os = "macos")]
    let apple_status = {
        let (evs, st, raw_status) = fetch_apple_calendar(request_macos_access);
        statuses.push(st);
        all_events.extend(evs);
        Some(raw_status)
    };
    #[cfg(not(target_os = "macos"))]
    let apple_status: Option<String> = {
        let _ = request_macos_access;
        None
    };

    #[cfg(target_os = "windows")]
    {
        let (evs, win_statuses) = fetch_windows_calendars();
        statuses.extend(win_statuses);
        all_events.extend(evs);
    }

    #[cfg(target_os = "linux")]
    {
        let (evs, linux_statuses) = fetch_linux_desktop_calendars();
        statuses.extend(linux_statuses);
        all_events.extend(evs);
    }

    // 2. Mozilla Thunderbird (cross-platform)
    let (tb_evs, tb_status) = fetch_thunderbird_calendars(now_ts);
    statuses.push(tb_status);
    all_events.extend(tb_evs);

    // 3. CalDAV accounts (iCloud, Fastmail, Nextcloud, Radicale, etc.)
    for account in &cfg.caldav_accounts {
        let (evs, st) = fetch_caldav_events(account, now);
        statuses.push(st);
        all_events.extend(evs);
    }

    // 4. Google Calendar & Microsoft Graph OAuth accounts
    let mut oauth_accounts = cfg.oauth_calendar_accounts.clone();
    let mut oauth_changed = false;
    for account in &mut oauth_accounts {
        let provider = account.provider.trim().to_ascii_lowercase();
        let (evs, st, refreshed) = if provider == "google" {
            fetch_google_calendar_events(account, now)
        } else if provider == "microsoft" {
            fetch_microsoft_graph_events(account, now)
        } else {
            (
                Vec::new(),
                CalendarSourceStatus {
                    name: format!("OAuth ({})", account.label),
                    status: format!("Unsupported provider '{}'", account.provider),
                    event_count: 0,
                },
                false,
            )
        };
        if refreshed {
            oauth_changed = true;
        }
        statuses.push(st);
        all_events.extend(evs);
    }

    // 5. `.ics` URLs and local `.ics` files
    for src in &cfg.calendar_ics_urls {
        let trimmed = src.trim();
        if trimmed.is_empty() {
            continue;
        }
        let content = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            ureq::get(trimmed)
                .call()
                .ok()
                .and_then(|mut r| r.body_mut().read_to_string().ok())
        } else {
            std::fs::read_to_string(trimmed).ok()
        };
        match content {
            Some(ics) => {
                let parsed = parse_ics(&ics, trimmed);
                let count = upcoming_or_active_events(&parsed, now_ts, 900).len();
                statuses.push(CalendarSourceStatus {
                    name: format!("ICS: {trimmed}"),
                    status: "Connected".to_string(),
                    event_count: count,
                });
                all_events.extend(parsed);
            }
            None => {
                statuses.push(CalendarSourceStatus {
                    name: format!("ICS: {trimmed}"),
                    status: "Failed to fetch/read".to_string(),
                    event_count: 0,
                });
            }
        }
    }

    // Filter to active or starting within +/- 15m, then deduplicate and sort
    let upcoming = upcoming_or_active_events(&all_events, now_ts, 900);
    let deduped = deduplicate_events(upcoming);

    CalendarRefreshResult {
        events: deduped,
        statuses,
        apple_status,
        updated_oauth_accounts: if oauth_changed {
            Some(oauth_accounts)
        } else {
            None
        },
    }
}

fn deduplicate_events(events: Vec<CalendarEvent>) -> Vec<CalendarEvent> {
    let mut by_key: HashMap<(String, i64), CalendarEvent> = HashMap::new();
    for ev in events {
        let key = (ev.title.trim().to_ascii_lowercase(), ev.start_ts);
        match by_key.get(&key) {
            Some(existing) => {
                let existing_score = usize::from(existing.meeting_url.is_some()) * 10
                    + existing.attendees.len();
                let new_score =
                    usize::from(ev.meeting_url.is_some()) * 10 + ev.attendees.len();
                if new_score > existing_score {
                    by_key.insert(key, ev);
                }
            }
            None => {
                by_key.insert(key, ev);
            }
        }
    }
    let mut out: Vec<CalendarEvent> = by_key.into_values().collect();
    out.sort_by_key(|e| e.start_ts);
    out
}

// ---------------------------------------------------------------------------
// macOS: Apple Calendar (EventKit via `mimir-calendar`)
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
fn fetch_apple_calendar(
    request_access: bool,
) -> (Vec<CalendarEvent>, CalendarSourceStatus, String) {
    #[derive(serde::Deserialize)]
    struct HelperOut {
        authorized: bool,
        status: String,
        events: Vec<CalendarEvent>,
    }

    let helper_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("mimir-calendar")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("mimir-calendar"));

    let mut cmd = std::process::Command::new(&helper_path);
    if request_access {
        cmd.arg("--request-access");
    }

    let Ok(output) = cmd.output() else {
        return (
            Vec::new(),
            CalendarSourceStatus {
                name: "Apple Calendar".to_string(),
                status: "Helper not found".to_string(),
                event_count: 0,
            },
            "unavailable".to_string(),
        );
    };

    if !output.status.success() {
        return (
            Vec::new(),
            CalendarSourceStatus {
                name: "Apple Calendar".to_string(),
                status: format!("Error (exit {:?})", output.status.code()),
                event_count: 0,
            },
            "error".to_string(),
        );
    }

    let Ok(parsed) = serde_json::from_slice::<HelperOut>(&output.stdout) else {
        return (
            Vec::new(),
            CalendarSourceStatus {
                name: "Apple Calendar".to_string(),
                status: "Invalid helper output".to_string(),
                event_count: 0,
            },
            "error".to_string(),
        );
    };

    let status_text = if parsed.authorized {
        "Connected".to_string()
    } else {
        match parsed.status.as_str() {
            "not_determined" => "Permission needed".to_string(),
            "denied" => "Access denied".to_string(),
            "restricted" => "Restricted".to_string(),
            "write_only" => "Write-only (need full access)".to_string(),
            other => other.to_string(),
        }
    };

    let count = parsed.events.len();
    (
        parsed.events,
        CalendarSourceStatus {
            name: "Apple Calendar".to_string(),
            status: status_text,
            event_count: count,
        },
        parsed.status,
    )
}

// ---------------------------------------------------------------------------
// Windows: Microsoft Outlook (COM) + Windows Calendar (AppointmentManager)
// ---------------------------------------------------------------------------
#[cfg(target_os = "windows")]
fn fetch_windows_calendars() -> (Vec<CalendarEvent>, Vec<CalendarSourceStatus>) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    #[derive(serde::Deserialize)]
    struct WinCalItem {
        uid: String,
        title: String,
        start_ts: i64,
        end_ts: i64,
        location: Option<String>,
        body: Option<String>,
        attendees: Vec<String>,
        source: String,
    }
    #[derive(serde::Deserialize)]
    struct WinCalOutput {
        outlook_status: String,
        windows_calendar_status: String,
        events: Vec<WinCalItem>,
    }

    let ps_script = r#"
$ErrorActionPreference = 'SilentlyContinue'
$now = [DateTimeOffset]::UtcNow
$startUtc = $now.AddMinutes(-15)
$endUtc = $now.AddHours(24)
$events = @()
$outlookStatus = 'Not detected'
$winCalStatus = 'Not detected'

# 1. Classic Microsoft Outlook COM
try {
    $outlook = New-Object -ComObject Outlook.Application
    if ($outlook -ne $null) {
        $ns = $outlook.GetNamespace('MAPI')
        $folder = $ns.GetDefaultFolder(9)
        $items = $folder.Items
        $items.IncludeRecurrences = $true
        $items.Sort('[Start]')
        $startStr = $startUtc.LocalDateTime.ToString('g')
        $endStr = $endUtc.LocalDateTime.ToString('g')
        $restricted = $items.Restrict("[Start] >= '$startStr' AND [Start] <= '$endStr'")
        $outlookStatus = 'Connected'
        foreach ($item in $restricted) {
            try {
                $sTs = [DateTimeOffset]::new($item.Start.ToUniversalTime(), [TimeSpan]::Zero).ToUnixTimeSeconds()
                $eTs = [DateTimeOffset]::new($item.End.ToUniversalTime(), [TimeSpan]::Zero).ToUnixTimeSeconds()
                $atts = @()
                if ($item.RequiredAttendees) {
                    $atts += ($item.RequiredAttendees -split ';' | ForEach-Object { $_.Trim() } | Where-Object { $_ -ne '' })
                }
                if ($item.OptionalAttendees) {
                    $atts += ($item.OptionalAttendees -split ';' | ForEach-Object { $_.Trim() } | Where-Object { $_ -ne '' })
                }
                $events += [PSCustomObject]@{
                    uid = [string]$item.EntryID
                    title = [string]$item.Subject
                    start_ts = [int64]$sTs
                    end_ts = [int64]$eTs
                    location = [string]$item.Location
                    body = [string]$item.Body
                    attendees = $atts
                    source = 'outlook'
                }
            } catch {}
        }
    }
} catch {
    $outlookStatus = 'Not running / unavailable'
}

# 2. Windows Calendar (WinRT AppointmentManager)
try {
    Add-Type -AssemblyName System.Runtime.WindowsRuntime | Out-Null
    $null = [Windows.ApplicationModel.Appointments.AppointmentManager, Windows.ApplicationModel.Appointments, ContentType=WindowsRuntime]
    $asTaskGeneric = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object { $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1' })[0]
    function Await-WinRT($WinRtTask, $ResultType) {
        $netTask = $asTaskGeneric.MakeGenericMethod($ResultType).Invoke($null, @($WinRtTask))
        $netTask.Wait(-1) | Out-Null
        return $netTask.Result
    }
    $storeOp = [Windows.ApplicationModel.Appointments.AppointmentManager]::RequestStoreAsync([Windows.ApplicationModel.Appointments.AppointmentStoreAccessType]::AllCalendarsReadOnly)
    $store = Await-WinRT $storeOp ([Windows.ApplicationModel.Appointments.AppointmentStore])
    if ($store -ne $null) {
        $winCalStatus = 'Connected'
        $findOp = $store.FindAppointmentsAsync($startUtc, [TimeSpan]::FromHours(24.25))
        $appts = Await-WinRT $findOp ([System.Collections.Generic.IReadOnlyList[Windows.ApplicationModel.Appointments.Appointment]])
        foreach ($a in $appts) {
            $sTs = $a.StartTime.ToUnixTimeSeconds()
            $eTs = $a.StartTime.Add($a.Duration).ToUnixTimeSeconds()
            $atts = @()
            foreach ($inv in $a.Invitees) {
                if ($inv.DisplayName) { $atts += [string]$inv.DisplayName }
                elseif ($inv.Address) { $atts += [string]$inv.Address }
            }
            $events += [PSCustomObject]@{
                uid = [string]$a.LocalId
                title = [string]$a.Subject
                start_ts = [int64]$sTs
                end_ts = [int64]$eTs
                location = [string]$a.Location
                body = [string]$a.Details
                attendees = $atts
                source = 'windows_calendar'
            }
        }
    }
} catch {
    $winCalStatus = 'Unavailable'
}

[PSCustomObject]@{
    outlook_status = $outlookStatus
    windows_calendar_status = $winCalStatus
    events = $events
} | ConvertTo-Json -Depth 5 -Compress
"#;

    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", ps_script])
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    let Ok(out) = output else {
        return (
            Vec::new(),
            vec![
                CalendarSourceStatus {
                    name: "Microsoft Outlook".to_string(),
                    status: "PowerShell unavailable".to_string(),
                    event_count: 0,
                },
                CalendarSourceStatus {
                    name: "Windows Calendar".to_string(),
                    status: "PowerShell unavailable".to_string(),
                    event_count: 0,
                },
            ],
        );
    };

    let Ok(parsed) = serde_json::from_slice::<WinCalOutput>(&out.stdout) else {
        return (
            Vec::new(),
            vec![
                CalendarSourceStatus {
                    name: "Microsoft Outlook".to_string(),
                    status: "Unavailable".to_string(),
                    event_count: 0,
                },
                CalendarSourceStatus {
                    name: "Windows Calendar".to_string(),
                    status: "Unavailable".to_string(),
                    event_count: 0,
                },
            ],
        );
    };

    let mut events = Vec::new();
    let mut outlook_count = 0usize;
    let mut wincal_count = 0usize;
    for item in parsed.events {
        let loc = item.location.as_deref().unwrap_or("");
        let body = item.body.as_deref().unwrap_or("");
        let meeting_url = extract_meeting_url(loc).or_else(|| extract_meeting_url(body));
        let title = if item.title.trim().is_empty() {
            "Untitled Meeting".to_string()
        } else {
            item.title.trim().to_string()
        };
        if item.source == "outlook" {
            outlook_count += 1;
        } else {
            wincal_count += 1;
        }
        events.push(CalendarEvent {
            uid: item.uid,
            title,
            start_ts: item.start_ts,
            end_ts: item.end_ts.max(item.start_ts),
            meeting_url,
            attendees: item.attendees,
            notes_or_agenda: item.body.filter(|b| !b.trim().is_empty()),
            source: item.source,
        });
    }

    (
        events,
        vec![
            CalendarSourceStatus {
                name: "Microsoft Outlook".to_string(),
                status: parsed.outlook_status,
                event_count: outlook_count,
            },
            CalendarSourceStatus {
                name: "Windows Calendar".to_string(),
                status: parsed.windows_calendar_status,
                event_count: wincal_count,
            },
        ],
    )
}

// ---------------------------------------------------------------------------
// Linux: GNOME Calendar / Evolution Data Server + KDE KOrganizer
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
fn fetch_linux_desktop_calendars() -> (Vec<CalendarEvent>, Vec<CalendarSourceStatus>) {
    let mut events = Vec::new();
    let mut statuses = Vec::new();
    let now_ts = Utc::now().timestamp();

    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share")));
    let cache_home = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".cache")));

    // GNOME Calendar / Evolution Data Server
    let mut eds_dirs = Vec::new();
    if let Some(ref d) = data_home {
        eds_dirs.push(d.join("evolution/calendar"));
    }
    if let Some(ref c) = cache_home {
        eds_dirs.push(c.join("evolution/calendar"));
    }
    let eds_files = collect_ics_files(&eds_dirs, 4);
    let mut eds_events = Vec::new();
    for path in &eds_files {
        if let Ok(content) = std::fs::read_to_string(path) {
            eds_events.extend(parse_ics(&content, "gnome_evolution"));
        }
    }
    let eds_upcoming = upcoming_or_active_events(&eds_events, now_ts, 900).len();
    statuses.push(CalendarSourceStatus {
        name: "GNOME / Evolution Calendar".to_string(),
        status: if eds_files.is_empty() {
            "Not detected".to_string()
        } else {
            format!("Connected ({} store(s))", eds_files.len())
        },
        event_count: eds_upcoming,
    });
    events.extend(eds_events);

    // KDE KOrganizer
    let mut kde_dirs = Vec::new();
    if let Some(ref d) = data_home {
        kde_dirs.push(d.join("korganizer"));
        kde_dirs.push(d.join("apps/korganizer"));
    }
    let kde_files = collect_ics_files(&kde_dirs, 4);
    let mut kde_events = Vec::new();
    for path in &kde_files {
        if let Ok(content) = std::fs::read_to_string(path) {
            kde_events.extend(parse_ics(&content, "kde_korganizer"));
        }
    }
    let kde_upcoming = upcoming_or_active_events(&kde_events, now_ts, 900).len();
    statuses.push(CalendarSourceStatus {
        name: "KDE KOrganizer".to_string(),
        status: if kde_files.is_empty() {
            "Not detected".to_string()
        } else {
            format!("Connected ({} store(s))", kde_files.len())
        },
        event_count: kde_upcoming,
    });
    events.extend(kde_events);

    (events, statuses)
}

#[cfg(target_os = "linux")]
fn collect_ics_files(roots: &[PathBuf], max_depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in roots {
        if root.exists() {
            walk_ics(root, max_depth, &mut out);
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn walk_ics(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_ics(&path, depth - 1, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("ics") {
            out.push(path);
        }
    }
}

// ---------------------------------------------------------------------------
// Cross-platform: Mozilla Thunderbird (`calendar-data/*.sqlite`)
// ---------------------------------------------------------------------------
fn thunderbird_sqlite_paths() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    #[cfg(target_os = "linux")]
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".thunderbird"));
    }
    #[cfg(target_os = "macos")]
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join("Library/Thunderbird/Profiles"));
    }
    #[cfg(target_os = "windows")]
    if let Some(roaming) = dirs::config_dir() {
        roots.push(roaming.join("Thunderbird/Profiles"));
    }

    let mut dbs = Vec::new();
    for root in roots {
        let Ok(profiles) = std::fs::read_dir(&root) else {
            continue;
        };
        for prof in profiles.flatten() {
            let cal_dir = prof.path().join("calendar-data");
            for name in ["local.sqlite", "cache.sqlite"] {
                let candidate = cal_dir.join(name);
                if candidate.is_file() {
                    dbs.push(candidate);
                }
            }
        }
    }
    dbs
}

fn fetch_thunderbird_calendars(now_ts: i64) -> (Vec<CalendarEvent>, CalendarSourceStatus) {
    let db_paths = thunderbird_sqlite_paths();
    if db_paths.is_empty() {
        return (
            Vec::new(),
            CalendarSourceStatus {
                name: "Mozilla Thunderbird".to_string(),
                status: "Not detected".to_string(),
                event_count: 0,
            },
        );
    }

    let min_us = (now_ts - 900) * 1_000_000;
    let max_us = (now_ts + 86_400) * 1_000_000;
    let mut events = Vec::new();

    for db_path in &db_paths {
        if let Ok(mut parsed) = query_thunderbird_sqlite(db_path, min_us, max_us) {
            events.append(&mut parsed);
        }
    }

    let upcoming = upcoming_or_active_events(&events, now_ts, 900).len();
    (
        events,
        CalendarSourceStatus {
            name: "Mozilla Thunderbird".to_string(),
            status: format!("Connected ({} DB(s))", db_paths.len()),
            event_count: upcoming,
        },
    )
}

fn query_thunderbird_sqlite(
    db_path: &Path,
    min_us: i64,
    max_us: i64,
) -> anyhow::Result<Vec<CalendarEvent>> {
    let mut temp_copy: Option<PathBuf> = None;
    let conn = match Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(_) => {
            let tmp = std::env::temp_dir().join(format!(
                "mimir_tb_{}_{}.sqlite",
                std::process::id(),
                Utc::now().timestamp_nanos_opt().unwrap_or(0)
            ));
            std::fs::copy(db_path, &tmp)?;
            temp_copy = Some(tmp.clone());
            Connection::open_with_flags(
                &tmp,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?
        }
    };

    let mut stmt = conn.prepare(
        "SELECT id, title, event_start, event_end
         FROM cal_events
         WHERE event_end >= ?1 AND event_start <= ?2",
    )?;

    let rows: Vec<(String, String, i64, i64)> = stmt
        .query_map([min_us, max_us], |row| {
            let id: String = row.get(0)?;
            let title: Option<String> = row.get(1)?;
            let start_us: i64 = row.get(2)?;
            let end_us: i64 = row.get(3)?;
            Ok((
                id,
                title.unwrap_or_else(|| "Untitled Meeting".to_string()),
                start_us / 1_000_000,
                end_us / 1_000_000,
            ))
        })?
        .filter_map(Result::ok)
        .collect();

    let mut events = Vec::with_capacity(rows.len());
    for (id, title, start_ts, end_ts) in rows {
        let mut location = String::new();
        let mut description = String::new();
        let mut url_prop = String::new();

        if let Ok(mut prop_stmt) =
            conn.prepare("SELECT key, value FROM cal_properties WHERE item_id = ?1")
        {
            if let Ok(prop_rows) = prop_stmt.query_map([&id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                ))
            }) {
                for (k, v) in prop_rows.flatten() {
                    match k.to_ascii_uppercase().as_str() {
                        "LOCATION" => location = v,
                        "DESCRIPTION" => description = v,
                        "URL" => url_prop = v,
                        _ => {}
                    }
                }
            }
        }

        let mut attendees: Vec<String> = Vec::new();
        if let Ok(mut att_stmt) = conn.prepare(
            "SELECT common_name, attendee_id FROM cal_attendees WHERE item_id = ?1",
        ) {
            if let Ok(att_rows) = att_stmt.query_map([&id], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                ))
            }) {
                for (cn, att_id) in att_rows.flatten() {
                    let label = if !cn.trim().is_empty() {
                        cn.trim().to_string()
                    } else {
                        att_id
                            .trim()
                            .strip_prefix("mailto:")
                            .or_else(|| att_id.trim().strip_prefix("MAILTO:"))
                            .unwrap_or(att_id.trim())
                            .to_string()
                    };
                    if !label.is_empty()
                        && !attendees
                            .iter()
                            .any(|a: &String| a.eq_ignore_ascii_case(&label))
                    {
                        attendees.push(label);
                    }
                }
            }
        }

        let meeting_url = extract_meeting_url(&url_prop)
            .or_else(|| extract_meeting_url(&location))
            .or_else(|| extract_meeting_url(&description));

        events.push(CalendarEvent {
            uid: format!("thunderbird:{id}"),
            title: if title.trim().is_empty() {
                "Untitled Meeting".to_string()
            } else {
                title.trim().to_string()
            },
            start_ts,
            end_ts: end_ts.max(start_ts),
            meeting_url,
            attendees,
            notes_or_agenda: if description.trim().is_empty() {
                None
            } else {
                Some(description.trim().to_string())
            },
            source: "thunderbird".to_string(),
        });
    }

    drop(stmt);
    drop(conn);
    if let Some(tmp) = temp_copy {
        let _ = std::fs::remove_file(tmp);
    }

    Ok(events)
}

// ---------------------------------------------------------------------------
// Cross-platform: CalDAV (iCloud, Fastmail, Nextcloud, Radicale, etc.)
// ---------------------------------------------------------------------------
fn fetch_caldav_events(
    account: &CalDavAccount,
    now: DateTime<Utc>,
) -> (Vec<CalendarEvent>, CalendarSourceStatus) {
    let label = if account.name.trim().is_empty() {
        account.url.clone()
    } else {
        account.name.clone()
    };
    let start_str = (now - Duration::minutes(15))
        .format("%Y%m%dT%H%M%SZ")
        .to_string();
    let end_str = (now + Duration::hours(24))
        .format("%Y%m%dT%H%M%SZ")
        .to_string();

    let report_xml = format!(
        r#"<?xml version="1.0" encoding="utf-8" ?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VEVENT">
        <C:time-range start="{start_str}" end="{end_str}"/>
      </C:comp-filter>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>"#
    );

    let auth_raw = format!("{}:{}", account.username, account.password);
    let auth_header = format!("Basic {}", base64_encode(auth_raw.as_bytes()));

    let req = match ureq::http::Request::builder()
        .method("REPORT")
        .uri(account.url.trim())
        .header("Authorization", &auth_header)
        .header("Depth", "1")
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(report_xml)
    {
        Ok(r) => r,
        Err(e) => {
            return (
                Vec::new(),
                CalendarSourceStatus {
                    name: format!("CalDAV: {label}"),
                    status: format!("Invalid request: {e}"),
                    event_count: 0,
                },
            );
        }
    };

    let Ok(mut resp) = ureq::run(req) else {
        return (
            Vec::new(),
            CalendarSourceStatus {
                name: format!("CalDAV: {label}"),
                status: "Connection failed".to_string(),
                event_count: 0,
            },
        );
    };

    let Ok(body) = resp.body_mut().read_to_string() else {
        return (
            Vec::new(),
            CalendarSourceStatus {
                name: format!("CalDAV: {label}"),
                status: "Empty response".to_string(),
                event_count: 0,
            },
        );
    };

    let blocks = extract_caldav_ics_blocks(&body);
    let source_tag = format!("caldav:{label}");
    let mut events = Vec::new();
    for ics in blocks {
        events.extend(parse_ics(&ics, &source_tag));
    }
    let count = upcoming_or_active_events(&events, now.timestamp(), 900).len();
    (
        events,
        CalendarSourceStatus {
            name: format!("CalDAV: {label}"),
            status: "Connected".to_string(),
            event_count: count,
        },
    )
}

// ---------------------------------------------------------------------------
// Cross-platform: Google Calendar & Microsoft Graph (OAuth2)
// ---------------------------------------------------------------------------
fn maybe_refresh_oauth_token(
    account: &mut OAuthCalendarAccount,
    token_endpoint: &str,
    now_ts: i64,
) -> bool {
    let needs_refresh = match account.expires_at {
        Some(exp) => now_ts + 60 >= exp,
        None => account.access_token.trim().is_empty(),
    };
    if !needs_refresh {
        return false;
    }
    let (Some(rt), Some(cid)) = (
        account.refresh_token.as_deref().filter(|s| !s.trim().is_empty()),
        account.client_id.as_deref().filter(|s| !s.trim().is_empty()),
    ) else {
        return false;
    };

    let form_body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        url_encode_component(rt),
        url_encode_component(cid)
    );
    let Ok(mut resp) = ureq::post(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(form_body)
    else {
        return false;
    };
    let Ok(json_str) = resp.body_mut().read_to_string() else {
        return false;
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&json_str) else {
        return false;
    };
    if let Some(new_token) = val.get("access_token").and_then(|v| v.as_str()) {
        account.access_token = new_token.to_string();
        if let Some(expires_in) = val.get("expires_in").and_then(|v| v.as_i64()) {
            account.expires_at = Some(now_ts + expires_in);
        }
        return true;
    }
    false
}

fn url_encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn fetch_google_calendar_events(
    account: &mut OAuthCalendarAccount,
    now: DateTime<Utc>,
) -> (Vec<CalendarEvent>, CalendarSourceStatus, bool) {
    let refreshed = maybe_refresh_oauth_token(
        account,
        "https://oauth2.googleapis.com/token",
        now.timestamp(),
    );
    let label = if account.label.trim().is_empty() {
        "Primary".to_string()
    } else {
        account.label.clone()
    };

    let time_min = (now - Duration::minutes(15)).to_rfc3339();
    let time_max = (now + Duration::hours(24)).to_rfc3339();
    let url = format!(
        "https://www.googleapis.com/calendar/v3/calendars/primary/events?singleEvents=true&orderBy=startTime&timeMin={}&timeMax={}",
        url_encode_component(&time_min),
        url_encode_component(&time_max)
    );

    let auth = format!("Bearer {}", account.access_token.trim());
    let Ok(mut resp) = ureq::get(&url).header("Authorization", &auth).call() else {
        return (
            Vec::new(),
            CalendarSourceStatus {
                name: format!("Google Calendar ({label})"),
                status: "Auth / request failed".to_string(),
                event_count: 0,
            },
            refreshed,
        );
    };

    let Ok(body) = resp.body_mut().read_to_string() else {
        return (
            Vec::new(),
            CalendarSourceStatus {
                name: format!("Google Calendar ({label})"),
                status: "Empty response".to_string(),
                event_count: 0,
            },
            refreshed,
        );
    };

    let events = parse_google_calendar_json(&body, &format!("google:{label}"));
    let count = upcoming_or_active_events(&events, now.timestamp(), 900).len();
    (
        events,
        CalendarSourceStatus {
            name: format!("Google Calendar ({label})"),
            status: "Connected".to_string(),
            event_count: count,
        },
        refreshed,
    )
}

fn fetch_microsoft_graph_events(
    account: &mut OAuthCalendarAccount,
    now: DateTime<Utc>,
) -> (Vec<CalendarEvent>, CalendarSourceStatus, bool) {
    let refreshed = maybe_refresh_oauth_token(
        account,
        "https://login.microsoftonline.com/common/oauth2/v2.0/token",
        now.timestamp(),
    );
    let label = if account.label.trim().is_empty() {
        "Outlook 365".to_string()
    } else {
        account.label.clone()
    };

    let time_min = (now - Duration::minutes(15)).to_rfc3339();
    let time_max = (now + Duration::hours(24)).to_rfc3339();
    let url = format!(
        "https://graph.microsoft.com/v1.0/me/calendarView?startDateTime={}&endDateTime={}&$top=50",
        url_encode_component(&time_min),
        url_encode_component(&time_max)
    );

    let auth = format!("Bearer {}", account.access_token.trim());
    let Ok(mut resp) = ureq::get(&url)
        .header("Authorization", &auth)
        .header("Prefer", "outlook.timezone=\"UTC\"")
        .call()
    else {
        return (
            Vec::new(),
            CalendarSourceStatus {
                name: format!("Microsoft 365 ({label})"),
                status: "Auth / request failed".to_string(),
                event_count: 0,
            },
            refreshed,
        );
    };

    let Ok(body) = resp.body_mut().read_to_string() else {
        return (
            Vec::new(),
            CalendarSourceStatus {
                name: format!("Microsoft 365 ({label})"),
                status: "Empty response".to_string(),
                event_count: 0,
            },
            refreshed,
        );
    };

    let events = parse_microsoft_graph_json(&body, &format!("microsoft:{label}"));
    let count = upcoming_or_active_events(&events, now.timestamp(), 900).len();
    (
        events,
        CalendarSourceStatus {
            name: format!("Microsoft 365 ({label})"),
            status: "Connected".to_string(),
            event_count: count,
        },
        refreshed,
    )
}
