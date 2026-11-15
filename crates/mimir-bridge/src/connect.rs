//! Zero-config assistant auto-registration (`mimir connect`).

use mimir_core::{MimirError, Result};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct AssistantTarget {
    pub name: &'static str,
    pub config_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ConnectOutcome {
    pub name: &'static str,
    pub path: PathBuf,
    pub status: &'static str,
}

/// Discovers known AI assistant MCP config locations on the user's machine.
pub fn discover_assistants(home_dir: &Path) -> Vec<AssistantTarget> {
    let mut targets = vec![
        AssistantTarget {
            name: "Claude Code",
            config_path: home_dir.join(".claude.json"),
        },
        AssistantTarget {
            name: "Cursor",
            config_path: home_dir.join(".cursor/mcp.json"),
        },
        AssistantTarget {
            name: "Gemini CLI",
            config_path: home_dir.join(".gemini/settings.json"),
        },
        AssistantTarget {
            name: "Antigravity",
            config_path: home_dir.join(".gemini/antigravity/mcp_config.json"),
        },
        AssistantTarget {
            name: "Windsurf",
            config_path: home_dir.join(".codeium/windsurf/mcp_config.json"),
        },
    ];

    #[cfg(target_os = "macos")]
    targets.push(AssistantTarget {
        name: "Claude Desktop",
        config_path: home_dir.join("Library/Application Support/Claude/claude_desktop_config.json"),
    });

    #[cfg(target_os = "windows")]
    if let Some(app_data) = dirs::config_dir() {
        targets.push(AssistantTarget {
            name: "Claude Desktop",
            config_path: app_data.join("Claude/claude_desktop_config.json"),
        });
    }

    #[cfg(target_os = "linux")]
    targets.push(AssistantTarget {
        name: "Claude Desktop",
        config_path: home_dir.join(".config/Claude/claude_desktop_config.json"),
    });

    targets
}

/// Registers Mimir's `mcp` stdio server in any detected assistant config files.
pub fn connect_detected_assistants(
    home_dir: &Path,
    mimir_bin: &Path,
    dry_run: bool,
) -> Result<Vec<ConnectOutcome>> {
    let mut outcomes = Vec::new();
    let targets = discover_assistants(home_dir);

    for target in targets {
        let is_home_root_file = target.config_path.parent() == Some(home_dir);
        let parent_exists = !is_home_root_file && target.config_path.parent().is_some_and(|p| p.exists());
        if !target.config_path.exists() && !parent_exists {
            continue;
        }

        if !dry_run {
            upsert_mcp_entry(&target.config_path, mimir_bin)?;
        }

        outcomes.push(ConnectOutcome {
            name: target.name,
            path: target.config_path,
            status: if dry_run { "would_register" } else { "registered" },
        });
    }

    Ok(outcomes)
}

/// Safely inserts `mcpServers.mimir` into a JSON config file without clobbering existing entries.
pub fn upsert_mcp_entry(config_path: &Path, mimir_bin: &Path) -> Result<()> {
    let mut root: Value = if config_path.exists() {
        let raw = std::fs::read_to_string(config_path)?;
        if raw.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&raw)
                .map_err(|e| MimirError::Config(format!("invalid JSON in {}: {e}", config_path.display())))?
        }
    } else {
        json!({})
    };

    let obj = root
        .as_object_mut()
        .ok_or_else(|| MimirError::Config("MCP config root is not a JSON object".into()))?;

    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| MimirError::Config("mcpServers is not a JSON object".into()))?;

    servers.insert(
        "mimir".to_string(),
        json!({
            "command": mimir_bin.to_string_lossy(),
            "args": ["mcp"]
        }),
    );

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = config_path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&root).unwrap())?;
    std::fs::rename(&tmp, config_path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn upsert_preserves_existing_mcp_servers() {
        let dir = TempDir::new().unwrap();
        let cursor_dir = dir.path().join(".cursor");
        std::fs::create_dir_all(&cursor_dir).unwrap();
        let cfg = cursor_dir.join("mcp.json");
        std::fs::write(
            &cfg,
            r#"{"mcpServers":{"existing":{"command":"foo","args":[]}}}"#,
        )
        .unwrap();

        let outcomes =
            connect_detected_assistants(dir.path(), Path::new("/usr/local/bin/mimir"), false)
                .unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].name, "Cursor");

        let updated: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(updated["mcpServers"]["existing"].is_object());
        assert_eq!(updated["mcpServers"]["mimir"]["command"], "/usr/local/bin/mimir");
        assert_eq!(updated["mcpServers"]["mimir"]["args"][0], "mcp");
    }
}
