# Mimir Platform Roadmap: Local Summaries, Context Presets & Calendar "Join & Record"

> **Target baseline:** `feat/local-qwen-and-onnx` (PR #3, commit `9877e88`) once merged to `main`.
>
> **Core principles:**
> - **100% local-first:** Zero external model servers (no Ollama), zero cloud meeting bots joining calls. All LLM inference runs in-process via pure-Rust `candle-transformers` + `tokenizers` (`QwenCleanupModel::complete` in `crates/mimir-whistle/src/qwen.rs`).
> - **RAM discipline:** Keep Qwen 2.5 0.5B Instruct strictly post-processing (loaded after capture stops or on demand, never held resident during live capture).
> - **Cross-platform parity:** Every UI feature must ship in lockstep across **macOS** (`macos/Sources/MimirApp/MimirController.swift`, `MimirApp.swift`) and **Windows/Linux** (`crates/mimir-ui/src/main.rs`).

---

## Phase 1: Local Qwen Summaries, Action Items, Auto-Titling, Presets & Drop-Folder Sync

### 1. Core Types & Config (`crates/mimir-core/src/lib.rs`)
- Extend `Session` with optional summary & meeting metadata:
  - `summary_markdown: Option<String>`
  - `summary_template: Option<String>` (e.g., `"general"`, `"1on1"`, `"standup"`, `"sales_discovery"`, `"technical_review"`)
  - `action_items_json: Option<String>` (structured `{ owner, task, due_hint, timestamp }` list)
  - `calendar_event_uid: Option<String>`
  - `meeting_url: Option<String>`
  - `attendees_json: Option<String>`
- Add `KeywordPreset` (`name: String`, `keywords: Vec<String>`) and extend `MimirConfig` with:
  - `auto_summarize: bool` (default `true` when Qwen is downloaded)
  - `default_summary_template: String` (default `"general"`)
  - `keyword_presets: Vec<KeywordPreset>`
  - `drop_sync_enabled: bool` (sync Markdown/JSON exports into `MimirConfig.drop_dir` on session stop, cleanup, or summarize)

### 2. SQLite Schema & Post-Processing Pipeline (`crates/mimir-store/src/lib.rs`, `crates/mimir-store/src/export.rs`)
- Add idempotent `ALTER TABLE sessions ADD COLUMN ...` migrations in `SessionStore` for the new `Session` fields above.
- Add `SessionStore::summarize_session_with_progress(...)`:
  - Uses `QwenCleanupModel::complete(&mut self, system: &str, user: &str, max_new_tokens: usize)` in `crates/mimir-whistle/src/qwen.rs`.
  - Uses chunked/map-reduce summarization when transcripts exceed the 0.5B context window, reporting progress callbacks (reusing the same progress pattern as `cleanup_session_with_context_and_progress`).
  - Produces:
    1. **Auto-title** (applied only if the session title is still default/timestamped).
    2. **Templated Markdown summary** (Key Decisions, Discussion Points, Open Questions, Next Steps).
    3. **Structured action items JSON** (with speaker attribution from the `speakers` table).
- Update `render_session` / `render_markdown_with_view` in `crates/mimir-store/src/export.rs` to include `## Summary` and `## Action Items` sections when present, and write synced exports to `MimirConfig.drop_dir` when `drop_sync_enabled` is enabled.

### 3. MCP & CLI (`crates/mimir-bridge/src/mcp.rs`, `crates/mimir-cli/src/main.rs`)
- **MCP (`tools_manifest()` & `handle_tool_call()`):**
  - Add `get_summary` tool (`session_id` -> returns `summary_markdown`, `action_items`, `template`).
  - Add `summarize_session` tool (`session_id`, optional `template`).
  - Include summary metadata in `get_transcript` when available.
- **CLI (`Commands` enum):**
  - `mimir summarize <session_id> [--template <name>]`
  - `mimir presets list | save <name> --keywords <k1,k2,...> | delete <name>`

### 4. Desktop UI (`Mimir.app` & `mimir-desktop`)
- **Transcripts tab:**
  - Add a **Summary** panel alongside `Cleaned | Raw | Diff` showing the structured summary + extracted action items, a template picker, and a **Summarize** button with live progress bar (`cleanup_progress` / `UiEvent` in `crates/mimir-ui/src/main.rs`; `MimirController` in `macos/Sources/MimirApp/MimirController.swift`).
- **Live / Assistants tabs:**
  - Add Keyword Preset selector + save/delete controls.
  - Add `Auto-summarize after recording` and `Sync exports to Drop Folder` toggles.

---

## Phase 2: Local Calendar Integration & 1-Click "Join & Record"

> **Why local-first instead of Outlook M365 Add-ins:** Rewriting the native "Join" button inside Microsoft Outlook requires an M365 Add-in / Graph API OAuth and often IT admin consent. Reading local calendars directly delivers the same 1-click "Join & Record" UX with zero cloud auth.

### 1. Calendar Ingestion (`crates/mimir-calendar` or `mimir-core` + platform bridge)
- Define a unified `CalendarEvent` struct:
  - `uid: String`
  - `title: String`
  - `start_ts: i64`
  - `end_ts: i64`
  - `meeting_url: Option<String>`
  - `attendees: Vec<String>`
  - `notes_or_agenda: Option<String>`
  - `source: String`
- Parse meeting URLs (Zoom `zoom.us/j/...`, Google Meet `meet.google.com/...`, Microsoft Teams `teams.microsoft.com/l/meetup-join/...`, Webex) from event location/notes.
- **macOS (`macos/Sources/MimirApp/`):**
  - Use `EventKit` (`EKEventStore`) to read Apple Calendar (which already syncs iCloud, Google, and Outlook/Exchange calendars locally on macOS).
  - Handle calendar permission requests gracefully with a clear fallback if denied.
- **Windows / Linux (`crates/mimir-ui` / Rust core):**
  - Add configurable `.ics` / iCalendar feed URL(s) (or local `.ics` file path) polled in the background and parsed into `CalendarEvent`s.

### 2. Meeting-Aware Prefill & "Join & Record" UX
- **Upcoming / Active Meeting Banner** in both `Mimir.app` (`MimirMacApp`) and `mimir-desktop` (`MimirDesktopApp`):
  - Displays meetings starting within ±15 minutes (and ongoing meetings).
  - **1-Click "Join & Record"** button:
    1. Opens `meeting_url` in the system browser/app (`open` on macOS, `ShellExecute`/default opener on Windows, `xdg-open` on Linux).
    2. Starts Mimir capture immediately with:
       - Session title prefilled from `CalendarEvent.title`
       - Session keywords pre-seeded with attendee names/emails and agenda terms
       - `calendar_event_uid`, `meeting_url`, and `attendees_json` persisted on the `Session` so Qwen cleanup & summarization have full attendee/agenda context.
  - **Record Only** button (for in-person or already-joined calls) that still applies the prefill without launching a URL.

---

## Verification Checklist (Run After Each Phase)
- `cargo test --workspace` (unit & integration tests for SQLite migrations, Qwen prompt/chunking logic, `.ics` parser, and MCP tool schema/handlers)
- `./macos/scripts/build.sh` (verifies `Mimir.app` Swift compilation + Rust staticlib linking)
- Manual smoke check of `mimir-desktop` (`cargo run -p mimir-ui`) and `Mimir.app` to verify responsive layout and progress bars remain intact across both UIs
