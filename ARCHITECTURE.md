# Mimir — Software Architecture

**Status:** v0.1.0  
**Last Updated:** 2026-11-15  
**Traces To:** [REQUIREMENTS.md](file:///Users/joepeterson/development/Mimir/REQUIREMENTS.md)

---

## 1. Architectural Goals

1. **Zero-Drop Real-Time Audio:** Audio capture runs on dedicated OS real-time threads communicating via lock-free ring buffers—never blocked by transcription, disk I/O, or network clients.
2. **Strict Channel Provenance:** `mic` (Host) and `system` (Remote Loopback) remain separate pipelines from OS capture through Whistle inference, merging only at the final timeline layer.
3. **Single-Binary Native Distribution:** Pure Rust workspace linking Cactus Compute's native C engine (`libneedle` / `cactus.h`) via FFI. No Python runtime required for end users.
4. **Approachable Modularity:** Split into small, single-purpose Cargo crates so each component can be understood, tested, and compiled independently.

---

## 2. Cargo Workspace Layout

```text
Mimir/
├── Cargo.toml                  # Workspace root
├── REQUIREMENTS.md
├── ARCHITECTURE.md
└── crates/
    ├── mimir-core/             # Shared domain types, schemas, config, errors
    ├── mimir-audio/            # OS dual-channel capture, resampling, bleed gate, VAD
    ├── mimir-whistle/          # Safe Rust wrapper + C FFI (-sys) for Cactus/Whistle
    ├── mimir-diarize/          # Embedding clustering, voiceprints, timeline merge
    ├── mimir-store/            # SQLite (FTS5), crash-safe JSONL WAL, MD/SRT export
    ├── mimir-bridge/           # MCP server (stdio/HTTP), Local HTTP/SSE API, `connect`
    ├── mimir-cli/              # Binary entrypoint (`mimir`) + pipeline orchestration
    └── mimir-ui/               # Cross-platform egui desktop app (`mimir-desktop`)
macos/                          # Native macOS SwiftUI menu-bar & desktop app (`Mimir.app`)
```

> **Note for Rust newcomers:** A Cargo *workspace* lets us keep each concern in its own small `crates/<name>/src/` folder with explicit dependencies between them. `cargo test` runs tests across all crates, and `cargo build --release` compiles everything into a single `mimir` executable.

---

## 3. End-to-End Data Flow

```text
[OS Mic Thread] ──(ringbuf)──┐
                             ├──> [Bleed Gate & 16kHz Resampler]
[OS Loopback Thread] ────────┘           │
                                         ├── Mic PCM (16kHz) ──> [VAD + <=30s Chunker] ──┐
                                         └── Sys PCM (16kHz) ──> [VAD + <=30s Chunker] ──┤
                                                                                         v
                                                                           [WhistleWorker Pool]
                                                                            (cactus.h C FFI)
                                                                            - text + lang
                                                                            - word_timestamps
                                                                            - 80ms embeddings
                                                                                         │
                                         ┌───────────────────────────────────────────────┘
                                         v
                            [mimir-diarize: Timeline & Speaker Engine]
                             - Mic channel  ──> "Host" (deterministic)
                             - Sys channel  ──> Cosine cluster on embeddings ("Speaker 1..N")
                             - Merge turns chronologically by word timestamps
                                         │
                                         v
                            [mimir-store: Persistence & Live Broadcast]
                             - Append to live session JSONL (crash-safe WAL)
                             - Index utterances in SQLite FTS5
                             - Update drop-folder .md / .jsonl
                             - Broadcast live utterance event (tokio::sync::broadcast)
                                         │
                      ┌──────────────────┼──────────────────┐
                      v                  v                  v
               [MCP Server]      [Local HTTP/SSE]     [CLI `tail`]
            (Claude/Gemini/      (OpenAI tools /      (Unix pipes /
             Cursor/etc.)         Custom scripts)      Stdout)
```

---

## 4. Component Design

### 4.1 `mimir-core`
Defines the canonical types shared across all crates:
- `ChannelId`: `Mic` | `System`
- `AudioChunk`: `{ channel: ChannelId, start_mono_ns: u64, sample_rate: 16000, samples: Vec<f32> }`
- `WordToken`: `{ word: String, start_ms: u64, end_ms: u64, prob: f32 }`
- `Utterance`: `{ id: Uuid, speaker_id: String, channel: ChannelId, start_ms: u64, end_ms: u64, text: String, language: String, avg_confidence: f32, words: Vec<WordToken> }`
- `Session`: `{ id: Uuid, title: Option<String>, started_at: DateTime<Utc>, ended_at: Option<DateTime<Utc>>, speakers: HashMap<String, SpeakerInfo>, utterances: Vec<Utterance> }`
- `MimirConfig`: Paths (`~/.local/share/mimir` / `~/Library/Application Support/Mimir`), drop-folder path, default language/keywords, audio retention policy (`ephemeral` default vs `archive_wav`), bind port (`127.0.0.1:7331`).

### 4.2 `mimir-audio` (`FR-1`, `FR-2.2`)
- **Cross-Platform Capture Backends:**
  - **macOS:** ScreenCaptureKit (`SCStream` audio-only tap, macOS 14.2+) for driverless system loopback + `cpal` (CoreAudio) for host mic.
  - **Windows:** `wasapi` crate for WASAPI Loopback (`system`) + WASAPI Capture (`mic`).
  - **Linux:** `pipewire` / `libpulse-binding` for PipeWire/PulseAudio monitor source (`system`) + default source (`mic`).
- **Clock Synchronization:** Both streams stamp incoming frames against a single `std::time::Instant` session epoch so `start_ms` / `end_ms` align accurately even if hardware clocks drift slightly.
- **Resampling:** `rubato` converts arbitrary OS sample rates (44.1 kHz, 48 kHz, 96 kHz) to **16 kHz mono `f32`**.
- **Acoustic Bleed Suppression:**
  - Before VAD/Whistle, a lightweight cross-channel energy & lag-correlation check compares `mic` frames against recent `system` frames (0–150 ms acoustic delay window).
  - If `mic` energy is strongly correlated with `system` output and below a local-speech ratio threshold, the `mic` frame is gated/attenuated so speaker-to-mic bleed does not create duplicate `Host` transcripts.
- **VAD & Windowing:**
  - Per-channel VAD (Silero VAD via `ort`/ONNX or lightweight WebRTC VAD) detects speech boundaries.
  - Speech segments are flushed on natural pauses (> ~500 ms silence) or hard-capped at **28 seconds** (safely under Whistle's 30s single-pass limit) with a 0.5s overlap guard at forced splits.

### 4.3 `mimir-whistle` (`FR-2`)
- **FFI Layer (`whistle-sys`):** Uses `bindgen` at build time against Cactus Compute's `cactus.h` / `needle.h` and links `libneedle` / `libcactus`.
- **Safe Rust Wrapper (`WhistleEngine`):**
  - Loads `whistle.cact` (default 16.9 MB model, or user-specified fine-tuned `.cact`).
  - Exposes `transcribe(&self, pcm_16khz: &[f32], opts: &TranscribeOptions) -> Result<TranscribeResult>` returning text, detected language, `WordToken` list, and pooled/frame **80 ms speech embeddings**.
  - Supports **Aho-Corasick keyword biasing** list passed per-session or updated live (e.g., participant names, repo symbols).
- **Concurrency:** Whistle inference runs on a dedicated `tokio::task::spawn_blocking` / worker thread pool so CPU inference never stalls the async runtime.

### 4.4 `mimir-diarize` (`FR-3`)
- **Host Channel (`Mic`):** Deterministically labeled `host` (e.g., `"Me"` or configured name). Zero clustering needed.
- **Remote Channel (`System`):**
  1. Mean-pool (L2-normalized) Whistle's 80 ms speech embeddings over each voiced utterance to produce a compact utterance vector.
  2. Compare against known local voiceprints (if enrolled) and active session speaker centroids via cosine similarity.
  3. Assign to best-matching `Speaker N` if similarity ≥ threshold (e.g., `0.72`), updating the running centroid; otherwise spawn `Speaker N+1`.
- **Timeline Merge:** Utterances from both channels are inserted into the session timeline ordered by `start_ms` (using word timestamps for sub-second crosstalk ordering).

### 4.5 `mimir-store` (`FR-4`)
- **Storage Engine:** SQLite (`rusqlite` with bundled SQLite + `FTS5`) for fast metadata queries and full-text search across all past meetings.
- **Crash-Safe Live Log:** Every finalized utterance is immediately appended to `sessions/<id>.jsonl` (fsync/flush) so a mid-call crash loses at most the current in-flight chunk.
- **Exporters & Drop Folder:**
  - Renders clean, assistant-ready **Markdown** (`[00:12:04] **Host:** ...`), **JSONL**, and **SRT/VTT**.
  - Optionally mirrors `.md` / `.jsonl` files into a configured drop folder (e.g., `~/Documents/Mimir/`) on session end or live interval.

### 4.6 `mimir-bridge` (`FR-5`)
- **MCP Server (`rmcp` or lightweight JSON-RPC 2.0 over stdio + HTTP/SSE):**
  - **Tools:** `list_sessions`, `get_transcript`, `search_transcripts`, `get_live_transcript`, `start_session`, `stop_session`, `set_keywords`, `rename_speaker`.
  - **Resources:** `mimir://sessions/latest`, `mimir://sessions/{id}`, `mimir://live`.
- **Local HTTP / SSE API (`axum` on `127.0.0.1:7331`):**
  - `GET /v1/sessions`, `GET /v1/sessions/:id?format=md|json`
  - `GET /v1/live/stream` (Server-Sent Events for real-time consumers)
  - `GET /openapi.json` (OpenAI-compatible tool schema)
- **`mimir connect` Auto-Discovery:**
  - Detects installed assistant configs on the host (e.g., Claude Desktop `claude_desktop_config.json`, Claude Code `~/.claude.json`, Cursor `~/.cursor/mcp.json`, Gemini CLI, Antigravity) and offers one-step registration of the `mimir mcp` stdio server.

---

## 5. Threading & Concurrency Model

| Thread / Task | Role | Blocking? |
| :--- | :--- | :--- |
| **Audio Capture (x2)** | OS Mic & Loopback callbacks | **Never** (pushes into `rtrb` / `ringbuf` lock-free SPSC) |
| **Audio Processor** | Resample, bleed gate, VAD, chunking | CPU-light, deterministic loop |
| **Whistle Worker** | Runs `whistle.cact` C FFI inference | CPU-bound (`spawn_blocking` / dedicated thread) |
| **Session Coordinator** | Diarization, timeline merge, SQLite/JSONL write, broadcast | Async (`tokio`) |
| **Bridge Servers** | MCP (`stdio` / HTTP) + `axum` HTTP/SSE | Async (`tokio`) |
