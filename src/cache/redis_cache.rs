use async_trait::async_trait;
use redis::AsyncCommands;
use uuid::Uuid;

use super::{CacheStats, CachedResponse, SemanticCache};

/// Redis-backed semantic cache using RediSearch HNSW vector index
pub struct RedisSemanticCache {
    client: redis::Client,
    dims: usize,
}

impl RedisSemanticCache {
    /// Connect to Redis and ensure the vector index exists
    pub async fn new(redis_url: &str) -> anyhow::Result<Self> {
        let client = redis::Client::open(redis_url)?;

        // Test connection
        let mut conn = client.get_multiplexed_async_connection().await?;
        let _: String = redis::cmd("PING")
            .query_async(&mut conn)
            .await
            .map_err(|e| anyhow::anyhow!("Redis connection failed: {}", e))?;

        // Check that RediSearch module is available (required for vector search)
        let has_search = match redis::cmd("MODULE")
            .arg("LIST")
            .query_async::<redis::Value>(&mut conn)
            .await
        {
            Ok(redis::Value::Array(ref mods)) => {
                format!("{:?}", mods).to_lowercase().contains("search")
            }
            _ => {
                // MODULE LIST unavailable; probe with FT._LIST
                redis::cmd("FT._LIST")
                    .query_async::<redis::Value>(&mut conn)
                    .await
                    .is_ok()
            }
        };

        if !has_search {
            return Err(anyhow::anyhow!(
                "Redis at {} does not have the RediSearch module. \
                 Janus requires redis-stack (not vanilla Redis). \
                 Install via: brew tap redis-stack/redis-stack && brew install redis-stack-server, \
                 or use docker: docker compose up -d redis"
            , redis_url));
        }

        let cache = Self {
            client,
            dims: 384,
        };

        Ok(cache)
    }

    /// Ensure the vector search index exists for a given model
    async fn ensure_index(&self, model: &str) -> anyhow::Result<()> {
        let index_name = format!("janus_cache_{}", model.replace(['/', '-', '.'], "_"));
        let mut conn = self.client.get_multiplexed_async_connection().await?;

        // Check if index exists
        let exists: Result<redis::Value, _> = redis::cmd("FT.INFO")
            .arg(&index_name)
            .query_async(&mut conn)
            .await;

        if exists.is_ok() {
            return Ok(());
        }

        // Create index with HNSW vector field
        let result: Result<String, redis::RedisError> = redis::cmd("FT.CREATE")
            .arg(&index_name)
            .arg("ON")
            .arg("HASH")
            .arg("PREFIX")
            .arg("1")
            .arg(format!("janus:cache:{}:", model.replace(['/', '-', '.'], "_")))
            .arg("SCHEMA")
            .arg("embedding")
            .arg("VECTOR")
            .arg("HNSW")
            .arg("6")
            .arg("TYPE")
            .arg("FLOAT32")
            .arg("DIM")
            .arg(self.dims.to_string())
            .arg("DISTANCE_METRIC")
            .arg("COSINE")
            .arg("model")
            .arg("TAG")
            .arg("tokens_saved")
            .arg("NUMERIC")
            .query_async(&mut conn)
            .await;

        match result {
            Ok(_) => {
                tracing::info!(index = %index_name, "Created Redis vector index");
                Ok(())
            }
            Err(e) => {
                // Index might already exist from concurrent creation
                let msg = e.to_string();
                if msg.contains("already exists") {
                    Ok(())
                } else if msg.contains("unknown command") || msg.contains("ERR unknown") {
                    Err(anyhow::anyhow!(
                        "RediSearch module not available. Install redis-stack-server \
                         (not vanilla Redis): brew tap redis-stack/redis-stack && \
                         brew install redis-stack-server, \
                         or use docker: docker compose up -d redis. Error: {}", e
                    ))
                } else {
                    Err(anyhow::anyhow!("Failed to create vector index '{}': {}", index_name, e))
                }
            }
        }
    }

    fn model_key_prefix(model: &str) -> String {
        format!("janus:cache:{}:", model.replace(['/', '-', '.'], "_"))
    }

    fn index_name(model: &str) -> String {
        format!("janus_cache_{}", model.replace(['/', '-', '.'], "_"))
    }

    fn embedding_to_bytes(embedding: &[f32]) -> Vec<u8> {
        embedding
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect()
    }
}

#[async_trait]
impl SemanticCache for RedisSemanticCache {
    async fn get(
        &self,
        embedding: &[f32],
        threshold: f64,
        model: &str,
    ) -> anyhow::Result<Option<CachedResponse>> {
        self.ensure_index(model).await?;

        let mut conn = self.client.get_multiplexed_async_connection().await?;
        let index = Self::index_name(model);
        let blob = Self::embedding_to_bytes(embedding);

        // KNN search: find nearest neighbor
        let result: Result<redis::Value, _> = redis::cmd("FT.SEARCH")
            .arg(&index)
            .arg("*=>[KNN 1 @embedding $vec AS score]")
            .arg("PARAMS")
            .arg("2")
            .arg("vec")
            .arg(&blob)
            .arg("SORTBY")
            .arg("score")
            .arg("DIALECT")
            .arg("2")
            .query_async(&mut conn)
            .await;

        match result {
            Ok(redis::Value::Array(ref items)) if items.len() >= 3 => {
                // Parse FT.SEARCH response: [count, key, [field, value, ...], ...]
                if let Some(redis::Value::Array(ref fields)) = items.get(2) {
                    let mut score: Option<f64> = None;
                    let mut response_body: Option<Vec<u8>> = None;
                    let mut cached_model = String::new();
                    let mut tokens_saved: usize = 0;

                    let mut i = 0;
                    while i + 1 < fields.len() {
                        let field_name = match &fields[i] {
                            redis::Value::BulkString(b) => {
                                String::from_utf8_lossy(b).to_string()
                            }
                            _ => {
                                i += 2;
                                continue;
                            }
                        };

                        match field_name.as_str() {
                            "score" => {
                                if let redis::Value::BulkString(b) = &fields[i + 1] {
                                    let s = String::from_utf8_lossy(b);
                                    score = s.parse().ok();
                                }
                            }
                            "response" => {
                                if let redis::Value::BulkString(b) = &fields[i + 1] {
                                    response_body = Some(b.clone());
                                }
                            }
                            "model" => {
                                if let redis::Value::BulkString(b) = &fields[i + 1] {
                                    cached_model = String::from_utf8_lossy(b).to_string();
                                }
                            }
                            "tokens_saved" => {
                                if let redis::Value::BulkString(b) = &fields[i + 1] {
                                    let s = String::from_utf8_lossy(b);
                                    tokens_saved = s.parse().unwrap_or(0);
                                }
                            }
                            _ => {}
                        }
                        i += 2;
                    }

                    // Cosine distance: 0 = identical, 2 = opposite
                    // Convert to similarity: 1 - distance
                    if let (Some(distance), Some(body)) = (score, response_body) {
                        let similarity = 1.0 - distance;
                        if similarity >= threshold {
                            return Ok(Some(CachedResponse {
                                response_body: body,
                                model: cached_model,
                                tokens_saved,
                                similarity,
                            }));
                        }
                    }
                }
                Ok(None)
            }
            Ok(_) => Ok(None),
            Err(e) => {
                tracing::warn!(error = %e, "Redis cache lookup failed");
                Ok(None)
            }
        }
    }

    async fn put(
        &self,
        embedding: &[f32],
        response_body: &[u8],
        model: &str,
        tokens_saved: usize,
        ttl_seconds: u64,
    ) -> anyhow::Result<()> {
        self.ensure_index(model).await?;

        // Skip if a near-duplicate entry already exists (prevents HNSW index pollution)
        if let Ok(Some(_)) = self.get(embedding, 0.95, model).await {
            tracing::debug!("Skipping cache put: near-duplicate already exists");
            return Ok(());
        }

        let mut conn = self.client.get_multiplexed_async_connection().await?;
        let key = format!("{}{}", Self::model_key_prefix(model), Uuid::new_v4());
        let blob = Self::embedding_to_bytes(embedding);

        // Store as hash with embedding, response, model, tokens_saved
        redis::pipe()
            .hset_multiple(
                &key,
                &[
                    ("embedding", blob.as_slice()),
                    ("response", response_body),
                    ("model", model.as_bytes()),
                    ("tokens_saved", tokens_saved.to_string().as_bytes()),
                ],
            )
            .expire(&key, ttl_seconds as i64)
            .query_async::<()>(&mut conn)
            .await?;

        Ok(())
    }

    async fn flush(&self) -> anyhow::Result<u64> {
        let mut conn = self.client.get_multiplexed_async_connection().await?;

        // Find and delete all janus:cache:* keys
        let keys: Vec<String> = redis::cmd("KEYS")
            .arg("janus:cache:*")
            .query_async(&mut conn)
            .await
            .unwrap_or_default();

        let count = keys.len() as u64;
        for key in &keys {
            let _: () = conn.del(key).await?;
        }

        Ok(count)
    }

    async fn stats(&self) -> anyhow::Result<CacheStats> {
        let mut conn = self.client.get_multiplexed_async_connection().await?;

        let keys: Vec<String> = redis::cmd("KEYS")
            .arg("janus:cache:*")
            .query_async(&mut conn)
            .await
            .unwrap_or_default();

        Ok(CacheStats {
            total_entries: keys.len() as u64,
            total_hits: 0,
            total_misses: 0,
        })
    }
}
