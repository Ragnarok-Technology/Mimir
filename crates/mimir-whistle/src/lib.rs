//! Safe Rust wrapper around Cactus Compute's native `libcactus` C ABI for Whistle ASR.
//!
//! Dynamically loads `libcactus.{dylib,so,dll}` at runtime via `libloading`,
//! completely bypassing the Python package and any Python-side telemetry.

pub mod artifact;

use libloading::{Library, Symbol};
use mimir_core::{MimirError, Result, WHISTLE_SAMPLE_RATE, WordToken};
use serde::{Deserialize, Serialize};
use std::ffi::{CStr, CString, c_char, c_float, c_int};
use std::path::Path;
use std::ptr;
use std::sync::Mutex;

const OUTPUT_BYTES_PER_SECOND: usize = 4096;
const MIN_OUTPUT_BUFFER_BYTES: usize = 1 << 18; // 256 KiB

type FnLoad = unsafe extern "C" fn(*const c_char, u64) -> c_int;
type FnLastError = unsafe extern "C" fn() -> *const c_char;
type FnSetThreads = unsafe extern "C" fn(c_int);
type FnTranscribe = unsafe extern "C" fn(
    *const c_float,
    c_int,
    *const c_char,
    *const c_char,
    c_int,
    *mut c_char,
    c_int,
) -> c_int;
type FnEmbed =
    unsafe extern "C" fn(*const c_char, *const c_float, c_int, *mut c_float, c_int) -> c_int;

/// Options for a single Whistle transcription pass.
#[derive(Debug, Clone, Default)]
pub struct TranscribeOptions {
    /// ISO 639-1 code (one of `en`, `de`, `fr`, `es`, `it`, `nl`, `pl`), or `None` to auto-detect.
    pub language: Option<String>,
    /// Words or phrases boosted via Whistle's Aho-Corasick decoder biasing.
    pub keywords: Vec<String>,
    /// Enable word-level start/end timestamps and probabilities.
    pub word_timestamps: bool,
}

/// Parsed transcription output from Whistle.
#[derive(Debug, Clone, Deserialize)]
pub struct TranscribeResult {
    pub text: String,
    pub language: String,
    #[serde(default)]
    pub ttft_ms: f64,
    #[serde(default)]
    pub decode_tps: f64,
    #[serde(default)]
    pub words: Vec<RawWordToken>,
}

/// Raw word timestamp structure emitted by `cactus_transcribe` JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawWordToken {
    pub word: String,
    pub start: f64,
    pub end: f64,
    #[serde(default)]
    pub probability: f32,
}

impl TranscribeResult {
    /// Converts Whistle's second-based word timestamps into session-relative millisecond tokens.
    pub fn into_word_tokens(&self, chunk_offset_ms: u64) -> Vec<WordToken> {
        self.words
            .iter()
            .map(|w| WordToken {
                word: w.word.clone(),
                start_ms: chunk_offset_ms + (w.start * 1000.0).round() as u64,
                end_ms: chunk_offset_ms + (w.end * 1000.0).round() as u64,
                probability: w.probability,
            })
            .collect()
    }
}

/// Thread-safe handle to a loaded Whistle `.cact` model on the local CPU engine.
///
/// The underlying Cactus C runtime maintains global model state, so all FFI calls
/// are serialized through an internal `Mutex`.
pub struct WhistleEngine {
    // Keep `_lib` alive for the lifetime of the engine.
    _lib: Library,
    inner: Mutex<CactusFfi>,
}

struct CactusFfi {
    cactus_load: FnLoad,
    cactus_last_error: FnLastError,
    cactus_set_threads: FnSetThreads,
    cactus_transcribe: FnTranscribe,
    cactus_embed: FnEmbed,
}

// SAFETY: All FFI function pointers belong to the loaded `Library` kept alive by `WhistleEngine`,
// and every call is serialized by `WhistleEngine::inner` (`Mutex`).
unsafe impl Send for CactusFfi {}
unsafe impl Sync for CactusFfi {}

impl WhistleEngine {
    /// Loads `libcactus` from `lib_path` and initializes Whistle weights from `weights_path`.
    pub fn load(lib_path: &Path, weights_path: &Path) -> Result<Self> {
        let lib = unsafe { Library::new(lib_path) }
            .map_err(|e| MimirError::Whistle(format!("failed to load {}: {e}", lib_path.display())))?;

        let ffi = unsafe {
            let load: Symbol<FnLoad> = lib
                .get(b"cactus_load\0")
                .map_err(|e| MimirError::Whistle(format!("missing cactus_load: {e}")))?;
            let last_error: Symbol<FnLastError> = lib
                .get(b"cactus_last_error\0")
                .map_err(|e| MimirError::Whistle(format!("missing cactus_last_error: {e}")))?;
            let set_threads: Symbol<FnSetThreads> = lib
                .get(b"cactus_set_threads\0")
                .map_err(|e| MimirError::Whistle(format!("missing cactus_set_threads: {e}")))?;
            let transcribe: Symbol<FnTranscribe> = lib
                .get(b"cactus_transcribe\0")
                .map_err(|e| MimirError::Whistle(format!("missing cactus_transcribe: {e}")))?;
            let embed: Symbol<FnEmbed> = lib
                .get(b"cactus_embed\0")
                .map_err(|e| MimirError::Whistle(format!("missing cactus_embed: {e}")))?;

            CactusFfi {
                cactus_load: *load,
                cactus_last_error: *last_error,
                cactus_set_threads: *set_threads,
                cactus_transcribe: *transcribe,
                cactus_embed: *embed,
            }
        };

        let weights = std::fs::read(weights_path)?;
        let rc = unsafe { (ffi.cactus_load)(weights.as_ptr().cast::<c_char>(), weights.len() as u64) };
        if rc < 0 {
            return Err(MimirError::Whistle(unsafe { last_error_string(&ffi) }));
        }

        Ok(Self {
            _lib: lib,
            inner: Mutex::new(ffi),
        })
    }

    /// Configures CPU thread count (`0` = automatic).
    pub fn set_threads(&self, threads: i32) -> Result<()> {
        let ffi = self
            .inner
            .lock()
            .map_err(|_| MimirError::Whistle("engine lock poisoned".into()))?;
        unsafe { (ffi.cactus_set_threads)(threads as c_int) };
        Ok(())
    }

    /// Transcribes 16 kHz mono `f32` samples in `[-1.0, 1.0]`.
    pub fn transcribe(&self, samples: &[f32], opts: &TranscribeOptions) -> Result<TranscribeResult> {
        if samples.is_empty() {
            return Ok(TranscribeResult {
                text: String::new(),
                language: opts.language.clone().unwrap_or_default(),
                ttft_ms: 0.0,
                decode_tps: 0.0,
                words: Vec::new(),
            });
        }

        let lang_cstr = opts
            .language
            .as_deref()
            .map(CString::new)
            .transpose()
            .map_err(|e| MimirError::Whistle(format!("invalid language string: {e}")))?;
        let kw_joined = format_keywords(&opts.keywords);
        let kw_cstr = kw_joined
            .as_deref()
            .map(CString::new)
            .transpose()
            .map_err(|e| MimirError::Whistle(format!("invalid keyword string: {e}")))?;

        let seconds = (samples.len() / WHISTLE_SAMPLE_RATE as usize).max(1);
        let buf_len = (seconds * OUTPUT_BYTES_PER_SECOND).max(MIN_OUTPUT_BUFFER_BYTES);
        let mut buffer = vec![0u8; buf_len];

        let ffi = self
            .inner
            .lock()
            .map_err(|_| MimirError::Whistle("engine lock poisoned".into()))?;

        let rc = unsafe {
            (ffi.cactus_transcribe)(
                samples.as_ptr(),
                samples.len() as c_int,
                lang_cstr.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
                kw_cstr.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
                i32::from(opts.word_timestamps),
                buffer.as_mut_ptr().cast::<c_char>(),
                buffer.len() as c_int,
            )
        };
        if rc < 0 {
            return Err(MimirError::Whistle(unsafe { last_error_string(&ffi) }));
        }

        let json_str = unsafe { CStr::from_ptr(buffer.as_ptr().cast::<c_char>()) }
            .to_str()
            .map_err(|e| MimirError::Whistle(format!("non-utf8 transcribe output: {e}")))?;

        serde_json::from_str(json_str)
            .map_err(|e| MimirError::Whistle(format!("failed to parse transcribe JSON: {e}")))
    }

    /// Extracts Whistle's flattened 80 ms encoder embeddings for speaker diarization.
    pub fn embed(&self, samples: &[f32]) -> Result<Vec<f32>> {
        if samples.is_empty() {
            return Ok(Vec::new());
        }

        let ffi = self
            .inner
            .lock()
            .map_err(|_| MimirError::Whistle("engine lock poisoned".into()))?;

        let needed = unsafe {
            (ffi.cactus_embed)(ptr::null(), samples.as_ptr(), samples.len() as c_int, ptr::null_mut(), 0)
        };
        if needed < 0 {
            return Err(MimirError::Whistle(unsafe { last_error_string(&ffi) }));
        }

        let mut output = vec![0.0f32; needed as usize];
        let written = unsafe {
            (ffi.cactus_embed)(
                ptr::null(),
                samples.as_ptr(),
                samples.len() as c_int,
                output.as_mut_ptr(),
                needed,
            )
        };
        if written != needed {
            return Err(MimirError::Whistle(unsafe { last_error_string(&ffi) }));
        }
        Ok(output)
    }
}

unsafe fn last_error_string(ffi: &CactusFfi) -> String {
    let ptr = unsafe { (ffi.cactus_last_error)() };
    if ptr.is_null() {
        return "unknown cactus error".to_string();
    }
    unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
}

fn format_keywords(keywords: &[String]) -> Option<String> {
    let cleaned: Vec<&str> = keywords
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_whistle_json_and_converts_word_timestamps() {
        let raw = r#"{
            "text": "hello mimir",
            "language": "en",
            "ttft_ms": 11.4,
            "decode_tps": 95.2,
            "words": [
                {"word": "hello", "start": 0.12, "end": 0.45, "probability": 0.98},
                {"word": "mimir", "start": 0.50, "end": 0.91, "probability": 0.91}
            ]
        }"#;

        let parsed: TranscribeResult = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.text, "hello mimir");
        assert_eq!(parsed.language, "en");
        let tokens = parsed.into_word_tokens(10_000);
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].start_ms, 10_120);
        assert_eq!(tokens[0].end_ms, 10_450);
        assert_eq!(tokens[1].word, "mimir");
    }

    #[test]
    fn formats_keywords_cleanly() {
        assert_eq!(format_keywords(&[]), None);
        assert_eq!(
            format_keywords(&["  Mimir ".into(), "".into(), "Whistle".into()]),
            Some("Mimir,Whistle".into())
        );
    }
}
