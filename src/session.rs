use dashmap::DashMap;

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
