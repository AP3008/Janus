use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct CompressionEvent {
    pub tokens_before: usize,
    pub tokens_after: usize,
    pub stage_name: String,
    pub reason: String,
}

impl CompressionEvent {
    pub fn tokens_saved(&self) -> usize {
        self.tokens_before.saturating_sub(self.tokens_after)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallInfo {
    pub tool_name: String,
    pub input_summary: String,
    pub tool_use_id: String,
    pub status: ToolCallStatus,
    pub tokens_saved: usize,
}

#[derive(Debug, Clone, Serialize)]
pub enum ToolCallStatus {
    Kept,
    Deduped,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CacheStatus {
    Hit { similarity: f64 },
    Miss,
    Skipped,
}

#[derive(Debug, Clone, Default)]
pub struct SessionStats {
    pub total_requests: u64,
    pub total_tokens_original: u64,
    pub total_tokens_compressed: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub cache_tokens_saved: u64,
}

impl SessionStats {
    pub fn tokens_saved(&self) -> u64 {
        self.total_tokens_original.saturating_sub(self.total_tokens_compressed)
    }

    pub fn compression_ratio(&self) -> f64 {
        if self.total_tokens_original == 0 {
            return 0.0;
        }
        self.tokens_saved() as f64 / self.total_tokens_original as f64
    }

    pub fn cache_hit_ratio(&self) -> f64 {
        let total = self.cache_hits + self.cache_misses;
        if total == 0 {
            return 0.0;
        }
        self.cache_hits as f64 / total as f64
    }
}
