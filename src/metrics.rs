use serde::Serialize;
use std::time::Instant;

#[derive(Debug, Clone, Serialize)]
pub struct CompressionEvent {
    pub tokens_before: usize,
    pub tokens_after: usize,
    pub stage_name: String,
    pub reason: String,
    #[serde(skip)]
    pub timestamp: Instant,
}

impl CompressionEvent {
    pub fn tokens_saved(&self) -> usize {
        self.tokens_before.saturating_sub(self.tokens_after)
    }

    pub fn compression_ratio(&self) -> f64 {
        if self.tokens_before == 0 {
            return 0.0;
        }
        1.0 - (self.tokens_after as f64 / self.tokens_before as f64)
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

#[derive(Debug, Clone)]
pub struct RequestMetrics {
    pub events: Vec<CompressionEvent>,
    pub tool_calls: Vec<ToolCallInfo>,
    pub tokens_original: usize,
    pub tokens_compressed: usize,
    pub cache_status: CacheStatus,
    pub pipeline_duration: std::time::Duration,
    pub upstream_duration: Option<std::time::Duration>,
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

    pub fn update(&mut self, metrics: &RequestMetrics) {
        self.total_requests += 1;
        self.total_tokens_original += metrics.tokens_original as u64;
        self.total_tokens_compressed += metrics.tokens_compressed as u64;
        match &metrics.cache_status {
            CacheStatus::Hit { .. } => self.cache_hits += 1,
            CacheStatus::Miss => self.cache_misses += 1,
            CacheStatus::Skipped => {}
        }
    }
}
