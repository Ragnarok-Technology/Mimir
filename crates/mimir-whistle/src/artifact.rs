//! Local artifact discovery and explicit on-demand fetching for `whistle.cact` and `libcactus`.

use mimir_core::{MimirConfig, MimirError, Result, restrict_file_permissions};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const WHISTLE_WEIGHTS_FILENAME: &str = "whistle.cact";
pub const WHISTLE_WEIGHTS_URL: &str =
    "https://huggingface.co/Cactus-Compute/whistle/resolve/741118687e1f008f06d1351a402a731274789790/whistle.cact";
pub const WHISTLE_WEIGHTS_SHA256: &str =
    "b6e02f048568ac5d01a2042556c658061e699acbc0aa2a1439f52f3d461dffeb";

pub const QWEN_WEIGHTS_FILENAME: &str = "qwen2.5-0.5b-instruct-q4_k_m.gguf";
pub const QWEN_WEIGHTS_URL: &str =
    "https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/main/qwen2.5-0.5b-instruct-q4_k_m.gguf";
pub const QWEN_WEIGHTS_SHA256: &str =
    "74a4da8c9fdbcd15bd1f6d01d621410d31c6fc00986f5eb687824e7b93d7a9db";

pub const QWEN_TOKENIZER_FILENAME: &str = "qwen2.5-tokenizer.json";
pub const QWEN_TOKENIZER_URL: &str =
    "https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct/resolve/main/tokenizer.json";
pub const QWEN_TOKENIZER_SHA256: &str =
    "c0382117ea329cdf097041132f6d735924b697924d6f6fc3945713e96ce87539";

pub const SILERO_VAD_FILENAME: &str = "silero_vad.onnx";
pub const SILERO_VAD_URL: &str =
    "https://raw.githubusercontent.com/snakers4/silero-vad/v5.1.2/src/silero_vad/data/silero_vad.onnx";
pub const SILERO_VAD_SHA256: &str =
    "2623a2953f6ff3d2c1e61740c6cdb7168133479b267dfef114a4a3cc5bdd788f";

pub const SPEAKER_EMBED_FILENAME: &str = "speaker_embed.onnx";
pub const SPEAKER_EMBED_URL: &str =
    "https://huggingface.co/csukuangfj/speaker-embedding-models/resolve/70503b6968275c19ba5bdcba8da82023ee73d9d8/wespeaker_en_voxceleb_CAM++.onnx";
pub const SPEAKER_EMBED_SHA256: &str =
    "c46fad10b5f81e1aa4a60c162714208577093655076c5450f8c469e522ec54ef";

/// Status snapshot for a locally managed Mimir model artifact.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelArtifactStatus {
    pub id: String,
    pub name: String,
    pub installed: bool,
    pub size_bytes: u64,
    pub expected_size_label: String,
    pub path: String,
}

/// Returns the platform-specific Cactus shared library filename.
pub fn platform_lib_filename() -> &'static str {
    if cfg!(target_os = "macos") {
        "libcactus.dylib"
    } else if cfg!(target_os = "windows") {
        "libcactus.dll"
    } else {
        "libcactus.so"
    }
}

/// Resolves the `whistle.cact` path from env override, config, or `data_dir/models/whistle.cact`.
pub fn resolve_weights_path(cfg: &MimirConfig) -> PathBuf {
    if let Ok(env_path) = std::env::var("CACTUS_WHISTLE_WEIGHTS")
        && !env_path.is_empty()
    {
        return PathBuf::from(env_path);
    }
    if let Some(p) = &cfg.whistle_weights {
        return p.clone();
    }
    cfg.data_dir.join("models").join(WHISTLE_WEIGHTS_FILENAME)
}

/// Resolves the `libcactus` shared library path from env override, config, or `data_dir/lib/`.
pub fn resolve_lib_path(cfg: &MimirConfig) -> PathBuf {
    if let Ok(env_path) = std::env::var("CACTUS_LIB_PATH")
        && !env_path.is_empty()
    {
        return PathBuf::from(env_path);
    }
    if let Some(p) = &cfg.cactus_lib_path {
        return p.clone();
    }
    cfg.data_dir.join("lib").join(platform_lib_filename())
}

/// Explicitly downloads `whistle.cact` (16.9 MB) and verifies its SHA-256 digest.
/// Only called when the user explicitly runs `mimir fetch`.
pub fn fetch_whistle_weights(dest: &Path) -> Result<PathBuf> {
    fetch_verified_file(dest, WHISTLE_WEIGHTS_URL, WHISTLE_WEIGHTS_SHA256, "whistle.cact")
}

/// Resolves the local paths for Qwen 2.5 0.5B Instruct GGUF weights and tokenizer.
pub fn resolve_qwen_paths(cfg: &MimirConfig) -> (PathBuf, PathBuf) {
    let dir = cfg.data_dir.join("models");
    (dir.join(QWEN_WEIGHTS_FILENAME), dir.join(QWEN_TOKENIZER_FILENAME))
}

/// Downloads Qwen 2.5 0.5B Instruct (`qwen2.5-0.5b-instruct-q4_k_m.gguf` + tokenizer)
/// and verifies both SHA-256 digests.
pub fn fetch_qwen_model(cfg: &MimirConfig) -> Result<(PathBuf, PathBuf)> {
    let (weights, tokenizer) = resolve_qwen_paths(cfg);
    fetch_verified_file(
        &weights,
        QWEN_WEIGHTS_URL,
        QWEN_WEIGHTS_SHA256,
        QWEN_WEIGHTS_FILENAME,
    )?;
    fetch_verified_file(
        &tokenizer,
        QWEN_TOKENIZER_URL,
        QWEN_TOKENIZER_SHA256,
        QWEN_TOKENIZER_FILENAME,
    )?;
    Ok((weights, tokenizer))
}

/// Deletes the locally cached Qwen cleanup model and tokenizer.
pub fn delete_qwen_model(cfg: &MimirConfig) -> Result<()> {
    let (weights, tokenizer) = resolve_qwen_paths(cfg);
    for path in [&weights, &tokenizer] {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

/// Resolves the local path for `silero_vad.onnx`.
pub fn resolve_silero_vad_path(cfg: &MimirConfig) -> PathBuf {
    cfg.data_dir.join("models").join(SILERO_VAD_FILENAME)
}

/// Downloads `silero_vad.onnx` and verifies its SHA-256 digest.
pub fn fetch_silero_vad(cfg: &MimirConfig) -> Result<PathBuf> {
    let dest = resolve_silero_vad_path(cfg);
    fetch_verified_file(&dest, SILERO_VAD_URL, SILERO_VAD_SHA256, SILERO_VAD_FILENAME)
}

/// Deletes the locally cached `silero_vad.onnx`.
pub fn delete_silero_vad(cfg: &MimirConfig) -> Result<()> {
    let path = resolve_silero_vad_path(cfg);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Resolves the local path for `speaker_embed.onnx`.
pub fn resolve_speaker_embed_path(cfg: &MimirConfig) -> PathBuf {
    cfg.data_dir.join("models").join(SPEAKER_EMBED_FILENAME)
}

/// Downloads `speaker_embed.onnx` and verifies its SHA-256 digest.
pub fn fetch_speaker_embed(cfg: &MimirConfig) -> Result<PathBuf> {
    let dest = resolve_speaker_embed_path(cfg);
    fetch_verified_file(
        &dest,
        SPEAKER_EMBED_URL,
        SPEAKER_EMBED_SHA256,
        SPEAKER_EMBED_FILENAME,
    )
}

/// Deletes the locally cached `speaker_embed.onnx`.
pub fn delete_speaker_embed(cfg: &MimirConfig) -> Result<()> {
    let path = resolve_speaker_embed_path(cfg);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Returns installation status for all locally managed Mimir models.
pub fn model_statuses(cfg: &MimirConfig) -> Vec<ModelArtifactStatus> {
    let whistle = resolve_weights_path(cfg);
    let (qwen_weights, qwen_tok) = resolve_qwen_paths(cfg);
    let silero = resolve_silero_vad_path(cfg);
    let speaker = resolve_speaker_embed_path(cfg);

    let file_size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let qwen_installed = qwen_weights.is_file() && qwen_tok.is_file();

    vec![
        ModelArtifactStatus {
            id: "whistle".to_string(),
            name: "Whistle ASR (whistle.cact)".to_string(),
            installed: whistle.is_file(),
            size_bytes: file_size(&whistle),
            expected_size_label: "16.9 MB".to_string(),
            path: whistle.display().to_string(),
        },
        ModelArtifactStatus {
            id: "qwen".to_string(),
            name: "Qwen 2.5 0.5B Instruct (Transcript Cleanup)".to_string(),
            installed: qwen_installed,
            size_bytes: file_size(&qwen_weights) + file_size(&qwen_tok),
            expected_size_label: "475 MB".to_string(),
            path: qwen_weights.display().to_string(),
        },
        ModelArtifactStatus {
            id: "silero_vad".to_string(),
            name: "Silero VAD v5 (ONNX Speech Detector)".to_string(),
            installed: silero.is_file(),
            size_bytes: file_size(&silero),
            expected_size_label: "2.2 MB".to_string(),
            path: silero.display().to_string(),
        },
        ModelArtifactStatus {
            id: "speaker_embed".to_string(),
            name: "WeSpeaker CAM++ (ONNX Speaker Embeddings)".to_string(),
            installed: speaker.is_file(),
            size_bytes: file_size(&speaker),
            expected_size_label: "28 MB".to_string(),
            path: speaker.display().to_string(),
        },
    ]
}

/// Downloads `url` to `dest` atomically and verifies its SHA-256 digest.
pub fn fetch_verified_file(
    dest: &Path,
    url: &str,
    expected_sha256: &str,
    label: &str,
) -> Result<PathBuf> {
    if dest.exists() && verify_sha256(dest, expected_sha256)? {
        return Ok(dest.to_path_buf());
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let response = ureq::get(url)
        .call()
        .map_err(|e| MimirError::Whistle(format!("failed to download {label}: {e}")))?;

    let tmp = dest.with_extension("download.tmp");
    let mut reader = response.into_body().into_reader();
    let mut file = std::fs::File::create(&tmp)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 32 * 1024];

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
    }
    file.flush()?;

    let digest = hex::encode(hasher.finalize());
    if digest != expected_sha256 {
        let _ = std::fs::remove_file(&tmp);
        return Err(MimirError::Whistle(format!(
            "SHA-256 mismatch for {label}: expected {expected_sha256}, got {digest}"
        )));
    }

    std::fs::rename(&tmp, dest)?;
    let _ = restrict_file_permissions(dest);
    Ok(dest.to_path_buf())
}

pub fn verify_sha256(path: &Path, expected_hex: &str) -> Result<bool> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 32 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()) == expected_hex)
}
