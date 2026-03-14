use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
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

/// Per-instance data: holds a session store and tracks liveness
pub struct InstanceData {
    pub session_store: SessionStore,
    pub last_seen: std::sync::Mutex<Instant>,
}

impl InstanceData {
    pub fn new() -> Self {
        Self {
            session_store: SessionStore::new(),
            last_seen: std::sync::Mutex::new(Instant::now()),
        }
    }

    pub fn touch(&self) {
        if let Ok(mut last) = self.last_seen.lock() {
            *last = Instant::now();
        }
    }

    pub fn idle_duration(&self) -> Duration {
        self.last_seen
            .lock()
            .map(|last| last.elapsed())
            .unwrap_or(Duration::ZERO)
    }
}

/// Concurrent store of all connected Claude Code instances
pub struct InstanceStore {
    instances: DashMap<String, Arc<InstanceData>>,
}

impl InstanceStore {
    pub fn new() -> Self {
        Self {
            instances: DashMap::new(),
        }
    }

    /// Get or create instance data, updating last_seen timestamp
    pub fn get_or_create(&self, instance_id: &str) -> Arc<InstanceData> {
        let instance = self
            .instances
            .entry(instance_id.to_string())
            .or_insert_with(|| Arc::new(InstanceData::new()))
            .clone();
        instance.touch();
        instance
    }

    /// Remove instances that have been idle longer than max_idle
    pub fn cleanup_stale(&self, max_idle: Duration) {
        self.instances
            .retain(|_, data| data.idle_duration() < max_idle);
    }

    /// Number of tracked instances
    pub fn len(&self) -> usize {
        self.instances.len()
    }
}

/// Derive a stable instance ID from the request body.
/// Hashes the system prompt (which contains environment-specific context like
/// working directory, IDE type, OS info) to distinguish different Claude Code processes.
pub fn derive_instance_id(body: &serde_json::Value) -> String {
    // Try system prompt first (most reliable differentiator)
    if let Some(system) = body.get("system") {
        let system_str = if let Some(s) = system.as_str() {
            s.to_string()
        } else {
            // system can be an array of content blocks
            system.to_string()
        };
        if !system_str.is_empty() {
            return format!("inst_{:016x}", xxh3_64(system_str.as_bytes()));
        }
    }

    // Fallback: hash the tools list
    if let Some(tools) = body.get("tools") {
        let tools_str = tools.to_string();
        if !tools_str.is_empty() {
            return format!("inst_{:016x}", xxh3_64(tools_str.as_bytes()));
        }
    }

    "inst_default".to_string()
}
