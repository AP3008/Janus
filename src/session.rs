use dashmap::DashMap;
use std::sync::Arc;
use xxhash_rust::xxh3::xxh3_64;

/// Entry stored for each unique tool result
#[derive(Debug, Clone)]
pub struct ToolResultEntry {
    pub tool_use_id: String,
    pub original_token_count: usize,
}

/// Per-session data tracking tool result hashes
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

/// Concurrent session store keyed by session ID
pub struct SessionStore {
    sessions: DashMap<String, Arc<SessionData>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
        }
    }

    /// Get or create a session for the given ID
    pub fn get_or_create(&self, session_id: &str) -> Arc<SessionData> {
        self.sessions
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(SessionData::new()))
            .clone()
    }

    /// Derive a stable session ID from the first user message in the conversation
    pub fn derive_session_id(messages: &[serde_json::Value]) -> String {
        for msg in messages {
            if msg.get("role").and_then(|r| r.as_str()) == Some("user") {
                let content = if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
                    s.to_string()
                } else if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
                    // Find first text block
                    arr.iter()
                        .find_map(|block| {
                            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                                block.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default()
                } else {
                    continue;
                };

                if !content.is_empty() {
                    return format!("{:016x}", xxh3_64(content.as_bytes()));
                }
            }
        }
        "default".to_string()
    }
}
