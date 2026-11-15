# Mimir

**100% local, zero-telemetry, covert dual-channel transcription for AI assistants** — powered by Cactus Compute's **Whistle AI** (`whistle.cact`, 16.9 MB CPU model).

Mimir captures both your local microphone and system audio simultaneously, suppresses speaker-to-mic bleed, diarizes speakers locally, and exposes clean transcription records directly to any AI assistant via **MCP**, **loopback HTTP/SSE**, **CLI pipes**, or a **local drop folder**.

---

## Features

- **100% Local & Zero Telemetry**: Runs Whistle AI (`whistle.cact`) directly on CPU via `libcactus` (`libloading` C FFI) — no cloud accounts, no API keys, no outbound telemetry.
- **Dual-Channel Capture**: Separates local mic (`Host`) from system audio (`System`) with acoustic bleed gating, ghost-echo deduplication, and optional multi-speaker mic diarization (`--diarize-mic`).
- **Universal Assistant Connectivity**:
  - **MCP (`stdio`)**: `mimir mcp` + one-click `mimir connect` auto-registration for Claude Desktop, Claude Code, Cursor, Gemini CLI, Antigravity, and Windsurf.
  - **Local HTTP REST + SSE + OpenAPI 3.1**: Loopback-only (`127.0.0.1`) with per-user `0600` bearer token and strict `Host` validation.
  - **CLI & Drop Folder**: Markdown, JSON/JSONL, SRT, and WebVTT exports + FTS5 full-text search.
- **Native macOS & Cross-Platform Desktop UIs**:
  - **macOS (`Mimir.app` / `Mimir.dmg`)**: SwiftUI app with `ScreenCaptureKit` system audio tap, menu bar extra, and GitHub Releases update checker.
  - **Cross-Platform (`mimir-desktop`)**: Native `eframe`/`egui` desktop app for macOS, Windows, and Linux.

---

## Quick Start

### 1. Download `whistle.cact` & Build

```bash
# Build Rust CLI + cross-platform desktop app
cargo build --release --bin mimir --bin mimir-desktop

# Download and SHA-256 verify whistle.cact (16.9 MB)
./target/release/mimir fetch

# Auto-register Mimir MCP with installed AI assistants
./target/release/mimir connect
```

### 2. CLI Usage

```bash
# Start live dual-channel capture with tail output and local HTTP/SSE server
mimir start --tail --serve

# Multi-speaker room mode (diarize multiple local mic speakers)
mimir start --diarize-mic --tail

# List, search, rename, archive, delete, and export transcripts
mimir sessions
mimir search "architecture"
mimir rename-session <uuid> "Design Sync"
mimir archive <uuid>
mimir delete <uuid>
mimir export <uuid> --format md
mimir latest --format md | pbcopy
```

### 3. macOS `.app` & `.dmg` Build

```bash
./macos/scripts/bundle.sh
./macos/scripts/package_dmg.sh
# Outputs: macos/build/Mimir.app and macos/build/Mimir.dmg
```

---

## Testing

```bash
cargo test --workspace
./macos/scripts/build.sh && ./macos/.build/*/debug/MimirCoreTests
```

---

## License

Released into the public domain under **The Unlicense**. See [LICENSE](LICENSE).
