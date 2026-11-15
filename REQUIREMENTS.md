# Mimir — Software Requirements Specification (SRS)

**Status:** Draft v1.0  
**Last Updated:** 2026-11-14  
**Repository:** Mimir (FOSS, 100% Local)

---

## 1. Executive Summary

**Mimir** is a Free and Open-Source (FOSS), 100% local, covert meeting and conversation transcription daemon built on **Whistle AI** (Cactus Compute's 16.9 MB on-device CPU speech recognition model).

Mimir does **not** join calls as a bot, does **not** send audio or transcripts to any cloud, and does **not** attempt to be a standalone AI chatbot or summarizer. Instead, Mimir acts as a silent, OS-level **audio-to-structured-record engine**: it captures both the host microphone and incoming system/speaker audio simultaneously, keeps them strictly separated, tracks speakers individually using Whistle's speech embeddings and word-level timestamps, and outputs clean, high-signal transcription records that **any AI assistant** (Gemini, Claude, ChatGPT/OpenAI, Claude Code, Codex, Cursor, Antigravity, local LLMs, or custom scripts) can consume effortlessly.

---

## 2. Core Principles & Non-Negotiables

1. **100% Local Execution:** All audio capture, Voice Activity Detection (VAD), speech-to-text inference, speaker embedding/diarization, storage, and assistant serving happen strictly on the user's machine. No cloud APIs, no internet dependency at runtime.
2. **Zero Telemetry:** No analytics, no crash-reporting phone-home, no update pings without explicit user action, no hidden network sockets. Network listeners (when enabled for local API/MCP) bind strictly to loopback (`127.0.0.1` / `::1`).
3. **FOSS:** Open-source code, open schemas, permissive/copyleft FOSS license (TBD: MIT or Apache-2.0), no proprietary blobs beyond user-supplied or official open-weight `.cact` models.
4. **Covert by Architecture:** Mimir never joins a meeting as a participant, never interacts with meeting provider APIs (Zoom/Meet/Teams/etc.), never triggers "X has started recording" bot notices, and leaves zero footprint visible to remote participants. *(Note: Mimir respects host OS privacy indicators and permissions on the user's own machine.)*
5. **Assistant-Agnostic Record Producer:** Mimir produces **only usable, high-quality transcription records**—not opinionated summaries—and exposes them through universal, zero-friction interfaces (MCP, local HTTP/SSE, CLI/pipes, and structured files) so any assistant just works.
6. **Cross-Platform & App-Agnostic:** Operates at the OS audio subsystem level so it works identically across macOS, Windows, and Linux, and across all VoIP/meeting/media apps (Zoom, Google Meet, Microsoft Teams, Discord, Slack Huddles, Webex, FaceTime, browser calls, etc.).

---

## 3. High-Level Architecture

```text
+-----------------------------------------------------------------------+
|                        OS Audio Subsystem                             |
|   [Host Microphone]                        [System / Speaker Output]  |
+----------+----------------------------------------------+-------------+
           | (Channel 0: Local / Host)                    | (Channel 1: Remote Loopback)
           v                                              v
+-----------------------------------------------------------------------+
|                 Dual-Channel Capture & Pre-Processing                 |
|  - Synchronized timestamps (monotonic clock)                          |
|  - Acoustic bleed / echo suppression (mic vs loopback correlation)    |
|  - Resample to 16 kHz mono PCM16 / float32                            |
|  - Per-channel VAD & <=30s sliding/utterance windowing                |
+----------+----------------------------------------------+-------------+
           |                                              |
           v                                              v
+-----------------------------------------------------------------------+
|                    Whistle AI Engine (Local CPU)                      |
|  - 16.9 MB whistle.cact (or custom fine-tuned .cact)                  |
|  - Transcription + Language ID (EN, DE, FR, ES, IT, NL, PL)           |
|  - Word-level timestamps & token probabilities                        |
|  - 80 ms frame speech embeddings (encoder representations)              |
|  - Aho-Corasick keyword biasing (names, jargon, project terms)        |
+----------+----------------------------------------------+-------------+
           |                                              |
           | (Deterministic: "Host")                      | (Diarized: "Remote 1..N")
           v                                              v
+-----------------------------------------------------------------------+
|              Speaker Attribution, Alignment & Record Store            |
|  - Embedding clustering / voiceprint matching for remote speakers     |
|  - Cross-channel chronological merge (handles crosstalk cleanly)      |
|  - Canonical local store (SQLite/JSONL + Markdown export)             |
+-----------------------------------------------------------------------+
           |              |                  |                 |
           v              v                  v                 v
     [MCP Server]  [Local HTTP/SSE]    [CLI / Pipes]   [Drop Folder / MD]
   (Claude, Gemini,  (OpenAI tools,    (Scripts, Unix,  (Drag & drop,
    Cursor, etc.)     Web apps)         Coding agents)   Obsidian, IDEs)
```

---

## 4. Functional Requirements

### FR-1: Dual-Channel Audio Capture & OS Portability
- **FR-1.1 Simultaneous Dual-Stream Capture:** Mimir shall capture two independent audio streams simultaneously:
  1. **Host Microphone (`mic`):** The user's local input device.
  2. **System Loopback (`system`):** Incoming audio routed to the host's speakers/headphones.
- **FR-1.2 Strict Channel Separation:** The `mic` and `system` streams must never be mixed into a single mono track prior to transcription. Channel provenance must be preserved end-to-end so local speech is never misattributed to remote participants.
- **FR-1.3 Acoustic Bleed / Echo Mitigation:** When the user is on speakers (not headphones), the microphone may pick up remote audio. Mimir shall suppress cross-channel bleed using lightweight acoustic echo cancellation (AEC) / time-aligned energy comparison **and** transcript-level ghost deduplication (suppressing `Host` utterances that duplicate `System` text within a ±500 ms window at lower mic RMS).
- **FR-1.4 Cross-Platform Backends:** Mimir shall provide native loopback + mic capture across all major desktop OSes without requiring third-party virtual audio cables (unless unavoidable on older OS versions, in which case setup is automated/guided):
  - **macOS:** CoreAudio Process Tap (macOS 14.2+, preferred—requires only System Audio Recording permission) with ScreenCaptureKit fallback + CoreAudio input.
  - **Windows:** WASAPI Loopback + WASAPI Capture.
  - **Linux:** PipeWire (primary) and PulseAudio monitor sources (fallback).
- **FR-1.5 Whistle-Ready Normalization:** Both streams shall be resampled in real time to **16 kHz mono** with synchronized monotonic timestamps.
- **FR-1.6 Device Hot-Swapping & Bluetooth Hygiene:** Mimir shall never hold the microphone open while idle/paused (preventing Bluetooth A2DP-to-HFP quality degradation), and shall gracefully handle mid-call device or sample-rate changes without dropping the session or corrupting timestamps.

### FR-2: Whistle AI Transcription Engine
- **FR-2.1 Local CPU Runtime:** Mimir shall run Whistle AI (`whistle.cact`, ~16.9 MB) locally on CPU via Cactus Compute's runtime (`cactus-compute` / `Needle` SIMD engine), requiring zero GPU.
- **FR-2.2 VAD-Guided Chunking:** Because Whistle processes up to 30-second audio windows per pass, Mimir shall use lightweight local Voice Activity Detection (e.g., Silero VAD or WebRTC VAD) per channel to segment speech at natural pauses, capping chunks at ≤ 30 seconds with small overlap guards so words are never clipped.
- **FR-2.3 Full Feature Utilization:** Mimir shall enable and expose Whistle's native capabilities:
  - **Word-Level Timestamps (`word_timestamps=True`):** Capture start time, end time, and confidence/probability per word.
  - **Speech Embeddings:** Extract Whistle's 80 ms frame encoder embeddings for downstream speaker diarization and voiceprint matching without loading a massive secondary diarization model.
  - **Keyword Biasing:** Support user-supplied and context-supplied biasing lists (proper nouns, attendee names, company/repo terminology, acronyms) passed to Whistle's Aho-Corasick decoder biasing.
  - **Multilingual Support:** Support auto-detection and explicit language hints across Whistle's 7 supported languages (English, German, French, Spanish, Italian, Dutch, Polish). Unsupported languages are out of scope for the base `whistle.cact` model unless using a custom fine-tuned `.cact`.
- **FR-2.4 Custom / Fine-Tuned Models:** Users shall be able to point Mimir to a custom `.cact` model built via `cactus whistle finetune` / `cactus whistle build` for domain-specific accents or vocabularies.
- **FR-2.5 Live & Post-Hoc Modes:** Support both near-real-time streaming transcription (low latency during an active call) and batch/file transcription of local audio files.

### FR-3: Speaker Tracking & Diarization
- **FR-3.1 Deterministic Host Attribution:** All speech originating from the `mic` channel (after bleed filtering) shall be deterministically attributed to `Host` (or the user's configured display name, e.g., `Joe`).
- **FR-3.2 Remote Speaker Diarization:** Speech on the `system` channel shall be diarized into distinct remote speakers (`Speaker 1`, `Speaker 2`, ...) by clustering Whistle's speech embeddings across utterances (online cosine-similarity clustering with configurable threshold). *Fallback:* Because ASR embeddings can be phoneme-sensitive, the diarizer shall sit behind a clean interface allowing a small local ONNX speaker-embedding model if benchmarks show insufficient separation.
- **FR-3.3 Local Speaker Enrollment (Optional):** Users shall be able to enroll short local voice samples or retroactively label a `Speaker N` in a session so Mimir remembers and auto-labels recurring colleagues in future sessions—stored 100% locally.
- **FR-3.4 Crosstalk & Turn Alignment:** Utterances from `mic` and `system` shall be merged into a single chronological timeline using word-level timestamps so overlapping speech/interruptions remain readable and accurately ordered.

### FR-4: Usable Transcription Records & Data Model
- **FR-4.1 Pure Record Output (No Built-In Fluff):** Mimir shall output clean, structured, high-signal transcription records optimized for consumption by external AI assistants. It shall not inject opinionated LLM summaries or hallucinated action items.
- **FR-4.2 Canonical Record Schema:** Each session shall produce a structured record containing:
  - **Session Metadata:** `session_id` (UUID), `title` (optional/editable), `started_at` / `ended_at` (ISO-8601 UTC), `duration_ms`, `languages_detected`, `source_app_hint` (optional, e.g., if detectable locally without hooks), `model_id`, `keywords_used`.
  - **Speaker Registry:** Map of `speaker_id` -> `{ label, channel ("mic" | "system"), is_host }`.
  - **Utterances / Turns:** Ordered list of `{ id, speaker_id, channel, start_ms, end_ms, text, language, avg_confidence, words?: [{ word, start_ms, end_ms, prob }] }`.
- **FR-4.3 Output Formats:**
  - **Assistant-Optimized Markdown (`.md`):** Clean header metadata + compact, timestamped `[HH:MM:SS] Speaker: text` transcript ready for direct paste or context injection.
  - **Structured JSON / JSONL (`.json` / `.jsonl`):** Full fidelity including word timestamps and confidence scores for programmatic agents.
  - **Standard Subtitles (`.srt` / `.vtt`):** Optional export for media workflows.
- **FR-4.4 Audio Retention Policy (Privacy by Default):** Raw audio buffers shall be **ephemeral in memory** and discarded immediately after transcription by default. Users may opt in to saving local `.wav` recordings per session for re-transcription or fine-tuning.

### FR-5: Universal Assistant Connectivity ("Just Work" Integration)
Mimir must connect effortlessly to **any** AI assistant ecosystem without manual copy-paste gymnastics:
- **FR-5.1 First-Class MCP Server (Model Context Protocol):**
  - Expose a local MCP server (supporting `stdio` and local `Streamable HTTP / SSE`) compatible with **Claude Desktop, Claude Code, Gemini CLI, Cursor, Antigravity, Windsurf, Zed**, and any MCP client.
  - **MCP Tools:** `list_sessions`, `get_transcript` (with format/verbosity options: compact markdown vs full JSON), `search_transcripts` (full-text + keyword search across local history), `get_live_transcript` (tail current ongoing session), `start_session`, `stop_session`, `set_keywords`, `rename_speaker`.
  - **MCP Resources:** `mimir://sessions/latest`, `mimir://sessions/{id}`, `mimir://live`.
  - **Zero-Config Setup (`mimir connect`):** A single CLI command (or UI button) that detects installed assistants on the machine and automatically registers Mimir's MCP config (with user confirmation).
- **FR-5.2 Local HTTP REST + SSE / WebSocket API:**
  - Bind strictly to `127.0.0.1:<port>` (configurable) with a required per-user local bearer token (`0600`), strict `Host` header validation (DNS-rebinding protection), and no wildcard CORS.
  - Provide OpenAPI 3.1 spec + OpenAI-compatible function/tool definitions so ChatGPT/OpenAI agents, custom Python/JS scripts, or browser extensions can query transcripts or subscribe to live utterance streams via SSE/WebSocket.
- **FR-5.3 CLI & Unix Pipe Integration:**
  - `mimir start` / `mimir stop` / `mimir status`
  - `mimir tail` (streams live utterances to `stdout` in Markdown or JSONL for piping into `llm`, `gemini`, `claude`, `codex`, etc.)
  - `mimir latest --format md | pbcopy` (or `--copy` flag for instant clipboard copy)
  - `mimir export <session_id> --format md|jsonl|srt`
- **FR-5.4 Auto-Synced Local Drop Folder:**
  - Automatically write finalized (and optionally live-updating) `.md` and `.jsonl` files to a user-configured local directory (e.g., `~/Documents/Mimir/` or an Obsidian vault / project workspace) so any file-watching assistant or web chat drag-and-drop works immediately.
- **FR-5.5 Prompt-Injection Hygiene:** Transcript payloads returned via MCP or HTTP shall clearly delimit untrusted spoken content (e.g., structured JSON fields or explicit `<transcript>` data boundaries) so downstream assistants treat remote speech as data rather than instructions.

### FR-6: Covert & Frictionless Operation
- **FR-6.1 Background Daemon + Lightweight Tray/CLI:** Run silently as a background service with an optional minimal menu-bar/system-tray icon (macOS/Windows/Linux) showing idle/recording state and quick start/stop/copy controls.
- **FR-6.2 Global Hotkeys:** Configurable global keyboard shortcuts to start/stop capture or copy the latest transcript/last N minutes to clipboard.
- **FR-6.3 Optional Auto-Detection:** Optional mode to detect when a microphone or active call audio stream begins and automatically start/prompt recording (strictly local OS audio-session inspection).

---

## 5. Non-Functional Requirements

- **NFR-1 Privacy & Security:**
  - Zero outbound network requests during normal operation.
  - Local database/files stored with user-only filesystem permissions (`0600` / `0700`).
  - Optional local encryption at rest for archived transcripts.
- **NFR-2 Performance & Resource Footprint:**
  - Model footprint: ~16.9 MB (`whistle.cact`).
  - CPU usage: < 15% of a single modern CPU core during active dual-channel transcription; near-zero (< 1%) when idle.
  - RAM usage: < 250 MB resident memory during active session.
  - Latency: Live utterance appearance within < 1.5 seconds of speech pause.
- **NFR-3 Reliability & Crash Safety:**
  - Incremental append-only persistence (WAL/JSONL) during a live session so a crash or power loss loses at most a few seconds of audio, never the whole meeting.
- **NFR-4 Portability & Packaging:**
  - Single-command install per OS (Homebrew / `.dmg` on macOS, `winget` / `.msi` on Windows, AppImage / `.deb` / Flatpak / `cargo`/`pip` on Linux).

---

## 6. Out of Scope (v1)

- Cloud sync, hosted accounts, or team-shared SaaS dashboards.
- Meeting-bot integrations (no Zoom/Teams/Google Calendar OAuth bots).
- Built-in LLM summarization, chat UI, or action-item generation (delegated entirely to the user's connected assistant).
- Mobile OS support (iOS/Android) in v1 (desktop-first: macOS, Windows, Linux).
- Video or screen-frame capture (audio-only; ScreenCaptureKit on macOS is used strictly for audio tap).

---

## 7. Proposed Technical Stack (For Next Phase)

| Layer | Candidate Technology | Rationale |
| :--- | :--- | :--- |
| **Core Daemon & Audio** | **Rust** (or Python + C/Rust audio helper) | Rust provides single-binary cross-platform distribution, low memory, safe concurrency, and native CoreAudio/WASAPI/PipeWire bindings (`cpal` + OS loopback crates). Python is faster to prototype `cactus-compute` bindings if Whistle's primary SDK is Python/C. |
| **ASR & Embeddings** | **Whistle AI (`whistle.cact`)** via Cactus Compute C/Python engine | 16.9 MB, fast CPU inference, word timestamps, 80 ms speech embeddings, Aho-Corasick keyword biasing. |
| **VAD** | **Silero VAD** (ONNX/C) or **WebRTC VAD** | Sub-millisecond CPU voice activity detection for clean ≤ 30s chunking. |
| **Storage** | **SQLite (FTS5)** + **Markdown / JSONL** files | Fast full-text search across thousands of meetings + human/LLM-readable flat files. |
| **Assistant Bridge** | **MCP (stdio + HTTP/SSE)** + **Local REST/SSE** + **CLI** | Universal compatibility with Claude, Gemini, OpenAI, coding agents, and shell pipelines. |

---

## 8. Acceptance Criteria (MVP Definition of Done)

1. User installs Mimir on macOS, Windows, or Linux with zero cloud accounts or API keys.
2. User joins a live call (Zoom, Google Meet, or Teams) without any bot joining the call.
3. Mimir captures both the user's mic and the remote speakers simultaneously, suppresses speaker-to-mic bleed, and transcribes both locally via Whistle AI.
4. Output transcript cleanly separates `Host` from diarized `Speaker 1`, `Speaker 2`, etc., in chronological order with accurate timestamps.
5. Running `mimir connect` (or adding Mimir's MCP config) lets an external assistant (e.g., Claude, Gemini, Cursor, or a local script) immediately query `get_transcript` / `get_live_transcript` and receive clean, usable Markdown/JSON records.
6. Network inspection confirms **zero** outbound packets during capture, transcription, and storage.

---

## 9. Open Decisions for Architecture Phase

1. **Primary Implementation Language:** Pure **Rust** core (calling Cactus/Whistle C FFI) for single-binary distribution vs. **Python** daemon (using `cactus-compute` directly) + lightweight native audio capture helper.
2. **FOSS License:** **MIT** vs. **Apache-2.0** vs. **GPL-3.0**.
3. **macOS Audio Permission UX:** Guided onboarding for macOS Screen & System Audio Recording permission required by ScreenCaptureKit.
