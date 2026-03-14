use dashmap::DashMap;
use xxhash_rust::xxh3::xxh3_64;

/// Entry stored for each unique tool result
#[derive(Debug, Clone)]
pub struct ToolResultEntry {
    pub tool_use_id: String,
    pub original_token_count: usize,
}

/// Global data tracking tool result hashes for deduplication
pub struct SessionData {
    pub tool_hashes: DashMap<u64, ToolResultEntry>,
}

impl SessionData {
    pub fn new() -> Self {
        Self {
            tool_hashes: DashMap::new(),
        }
    }
}

/// Derive a stable instance ID from the request body.
///
/// Extracts only the stable environment fields from the system prompt
/// (working directory, platform, shell, IDE context) and hashes those.
/// Dynamic fields like current date, git status, and opened files are ignored
/// so the ID stays consistent across requests from the same Claude Code process.
pub fn derive_instance_id(body: &serde_json::Value) -> String {
    if let Some(system) = body.get("system") {
        let system_text = extract_system_text(system);
        if !system_text.is_empty() {
            let fingerprint = extract_env_fingerprint(&system_text);
            if !fingerprint.is_empty() {
                return format!("inst_{:016x}", xxh3_64(fingerprint.as_bytes()));
            }
        }
    }

    // Fallback for non-Claude-Code clients: hash sorted tool names only
    if let Some(tools) = body.get("tools") {
        let tool_names = extract_tool_names(tools);
        if !tool_names.is_empty() {
            return format!("inst_{:016x}", xxh3_64(tool_names.as_bytes()));
        }
    }

    "inst_default".to_string()
}

/// Extract text from the `system` field, which can be a plain string
/// or an array of content blocks (each with `type: "text"` and `text`).
fn extract_system_text(system: &serde_json::Value) -> String {
    if let Some(s) = system.as_str() {
        return s.to_string();
    }
    if let Some(arr) = system.as_array() {
        let mut parts = Vec::new();
        for block in arr {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    parts.push(text);
                }
            }
        }
        return parts.join("\n");
    }
    String::new()
}

/// Extract only stable environment lines from the system prompt.
/// These fields are set once when Claude Code launches and don't change:
/// - Primary working directory
/// - Platform
/// - Shell
/// - VSCode Extension Context (presence/absence)
fn extract_env_fingerprint(text: &str) -> String {
    let mut parts = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim().trim_start_matches("- ");
        if trimmed.starts_with("Primary working directory:")
            || trimmed.starts_with("working directory:")
        {
            parts.push(trimmed.to_string());
        } else if trimmed.starts_with("Platform:") {
            parts.push(trimmed.to_string());
        } else if trimmed.starts_with("Shell:") {
            parts.push(trimmed.to_string());
        }
    }

    // IDE indicator: VSCode vs terminal
    if text.contains("VSCode Extension Context") {
        parts.push("ide:vscode".to_string());
    } else {
        parts.push("ide:terminal".to_string());
    }

    parts.join("|")
}

/// Extract sorted tool names from the tools array (stable across requests,
/// unlike full tool JSON which may include dynamic descriptions).
fn extract_tool_names(tools: &serde_json::Value) -> String {
    if let Some(arr) = tools.as_array() {
        let mut names: Vec<&str> = arr
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        names.sort();
        return names.join(",");
    }
    String::new()
}
