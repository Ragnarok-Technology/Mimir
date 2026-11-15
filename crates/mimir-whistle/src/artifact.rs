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
    if dest.exists() && verify_sha256(dest, WHISTLE_WEIGHTS_SHA256)? {
        return Ok(dest.to_path_buf());
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let response = ureq::get(WHISTLE_WEIGHTS_URL)
        .call()
        .map_err(|e| MimirError::Whistle(format!("failed to download whistle.cact: {e}")))?;

    let tmp = dest.with_extension("cact.tmp");
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
    if digest != WHISTLE_WEIGHTS_SHA256 {
        let _ = std::fs::remove_file(&tmp);
        return Err(MimirError::Whistle(format!(
            "SHA-256 mismatch for whistle.cact: expected {WHISTLE_WEIGHTS_SHA256}, got {digest}"
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
