pub mod redis_cache;

use async_trait::async_trait;

/// A cached response entry
#[derive(Debug, Clone)]
pub struct CachedResponse {
    pub response_body: Vec<u8>,
    pub model: String,
    pub tokens_saved: usize,
    pub similarity: f64,
}

/// Semantic cache trait for storing and retrieving cached LLM responses
#[async_trait]
pub trait SemanticCache: Send + Sync {
    /// Look up a cached response by embedding similarity
    async fn get(
        &self,
        embedding: &[f32],
        threshold: f64,
        model: &str,
    ) -> anyhow::Result<Option<CachedResponse>>;

    /// Store a response with its embedding
    async fn put(
        &self,
        embedding: &[f32],
        response_body: &[u8],
        model: &str,
        tokens_saved: usize,
        ttl_seconds: u64,
    ) -> anyhow::Result<()>;

    /// Flush all cached entries
    async fn flush(&self) -> anyhow::Result<u64>;

    /// Get cache statistics
    async fn stats(&self) -> anyhow::Result<CacheStats>;
}

#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    pub total_entries: u64,
    pub total_hits: u64,
    pub total_misses: u64,
}
