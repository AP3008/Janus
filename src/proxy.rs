use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderName, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use reqwest::Client;
use std::sync::Arc;
use std::time::Instant;

use crate::cache::SemanticCache;
use crate::config::JanusConfig;
use crate::embed::Embedder;
use crate::metrics::CacheStatus;
use crate::session::{SessionStore, self};
use crate::stream_reassemble::{self, StreamTee};
use crate::tokenizer::Tokenizer;
use crate::tui::ProxyUpdate;
use tokio::sync::mpsc;

pub struct AppState {
    pub config: JanusConfig,
    pub client: Client,
    pub start_time: Instant,
    pub tokenizer: Tokenizer,
    pub tui_tx: mpsc::UnboundedSender<ProxyUpdate>,
    pub session_store: SessionStore,
    pub cache: Option<Box<dyn SemanticCache>>,
    pub embedder: Option<Embedder>,
}

pub fn create_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/messages", post(proxy_handler))
        .route("/health", get(health_handler))
        .with_state(state)
}

/// Headers to forward from client to upstream
const FORWARD_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "anthropic-version",
    "content-type",
    "anthropic-beta",
    "accept",
];

async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, StatusCode> {
    let upstream_url = format!("{}/v1/messages", state.config.server.upstream_url);

    // Parse body for token counting and compression
    let mut body_json: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
        tracing::error!(error = %e, "Failed to parse request body as JSON");
        StatusCode::BAD_REQUEST
    })?;

    let tokens_before = state.tokenizer.count_message_tokens(&body_json);

    // Check if this is a streaming request (needed for cache hit response format)
    let is_streaming = body_json
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    // Extract last user message text BEFORE pipeline compression for consistent cache embeddings
    let user_text_for_cache = if state.config.cache.enabled {
        extract_user_text(&body_json)
    } else {
        String::new()
    };

    // Try semantic cache lookup
    let model_id = body_json
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown")
        .to_string();

    if state.config.cache.enabled {
        if let (Some(cache), Some(embedder)) = (&state.cache, &state.embedder) {
            if !user_text_for_cache.is_empty() {
                match embedder.embed_one(&user_text_for_cache).await {
                    Ok(embedding) => {
                        match cache
                            .get(&embedding, state.config.cache.similarity_cutoff, &model_id)
                            .await
                        {
                            Ok(Some(cached)) => {
                                tracing::info!(
                                    similarity = cached.similarity,
                                    tokens_saved = cached.tokens_saved,
                                    "Cache HIT"
                                );

                                let _ = state.tui_tx.send(ProxyUpdate {
                                    tokens_original: tokens_before,
                                    tokens_compressed: 0,
                                    events: Vec::new(),
                                    tool_calls: Vec::new(),
                                    cache_status: CacheStatus::Hit {
                                        similarity: cached.similarity,
                                    },
                                    pipeline_duration: std::time::Duration::ZERO,
                                    upstream_duration: None,
                                });

                                let (body_bytes, content_type) = if is_streaming {
                                    // Convert cached JSON to SSE for streaming clients
                                    match stream_reassemble::json_to_sse(
                                        &cached.response_body,
                                    ) {
                                        Some(sse) => (sse, "text/event-stream"),
                                        None => {
                                            // Fallback to JSON if conversion fails
                                            (cached.response_body, "application/json")
                                        }
                                    }
                                } else {
                                    (cached.response_body, "application/json")
                                };

                                let mut response = Response::new(Body::from(body_bytes));
                                *response.status_mut() = StatusCode::OK;
                                response.headers_mut().insert(
                                    "content-type",
                                    content_type.parse().unwrap(),
                                );
                                response.headers_mut().insert(
                                    "x-janus-cache",
                                    "HIT".parse().unwrap(),
                                );
                                return Ok(response);
                            }
                            Ok(None) => {
                                tracing::debug!("Cache MISS");
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "Cache lookup failed, continuing without cache");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Embedding failed, skipping cache");
                    }
                }
            }
        }
    }

    // Derive session ID for dedup
    let session_id = if let Some(messages) = body_json.get("messages").and_then(|m| m.as_array()) {
        session::SessionStore::derive_session_id(messages)
    } else {
        "default".to_string()
    };
    let session_data = state.session_store.get_or_create(&session_id);

    // Run compression pipeline (with panic recovery to avoid crashing the proxy)
    let pipeline_start = Instant::now();
    let pipeline_result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::pipeline::process(
            &mut body_json,
            &state.tokenizer,
            &state.config.pipeline,
            Some(&session_data),
        )
    })) {
        Ok(result) => result,
        Err(e) => {
            tracing::error!("Pipeline panicked: {:?}", e);
            crate::pipeline::PipelineResult::default()
        }
    };
    let pipeline_duration = pipeline_start.elapsed();

    let tokens_after = state.tokenizer.count_message_tokens(&body_json);
    let tokens_saved = tokens_before.saturating_sub(tokens_after);

    tracing::info!(
        tokens_before = tokens_before,
        tokens_after = tokens_after,
        tokens_saved = tokens_saved,
        pipeline_ms = pipeline_duration.as_millis() as u64,
        stages = pipeline_result.events.len(),
        session = %session_id,
        "Compression complete"
    );

    for event in &pipeline_result.events {
        tracing::debug!(
            stage = %event.stage_name,
            saved = event.tokens_saved(),
            reason = %event.reason,
            "Stage result"
        );
    }

    // Serialize compressed body
    let compressed_body = serde_json::to_vec(&body_json).map_err(|e| {
        tracing::error!(error = %e, "Failed to serialize compressed body");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Build upstream request with forwarded headers
    let mut req = state.client.post(&upstream_url);
    for &header_name in FORWARD_HEADERS {
        if let Some(value) = headers.get(header_name) {
            if let Ok(name) = HeaderName::from_bytes(header_name.as_bytes()) {
                req = req.header(name, value.clone());
            }
        }
    }

    // Forward the compressed body
    let upstream_start = Instant::now();
    let upstream_response = req
        .body(compressed_body)
        .send()
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to forward request to upstream");
            StatusCode::BAD_GATEWAY
        })?;
    let upstream_duration = upstream_start.elapsed();

    // Build response
    let status = StatusCode::from_u16(upstream_response.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    let mut response_headers = HeaderMap::new();
    for (name, value) in upstream_response.headers() {
        response_headers.insert(name.clone(), value.clone());
    }

    // For streaming requests, pipe the response body through directly
    if is_streaming {
        let should_cache = status == StatusCode::OK
            && state.config.cache.enabled
            && state.cache.is_some()
            && state.embedder.is_some();

        tracing::info!(
            tokens_in = tokens_before,
            tokens_out = tokens_after,
            upstream_ms = upstream_duration.as_millis() as u64,
            response_status = status.as_u16(),
            "Streaming request forwarded"
        );

        // Send TUI update immediately (we won't wait for stream to finish)
        let _ = state.tui_tx.send(ProxyUpdate {
            tokens_original: tokens_before,
            tokens_compressed: tokens_after,
            events: pipeline_result.events,
            tool_calls: pipeline_result.tool_calls,
            cache_status: if should_cache {
                CacheStatus::Miss
            } else {
                CacheStatus::Skipped
            },
            pipeline_duration,
            upstream_duration: Some(upstream_duration),
        });

        if should_cache {
            // Tee the stream: forward chunks to client while accumulating for cache
            let (tx, rx) = tokio::sync::oneshot::channel::<Vec<u8>>();
            let tee_stream = StreamTee::new(upstream_response.bytes_stream(), tx);

            let stream = tee_stream.map(|chunk: Result<bytes::Bytes, reqwest::Error>| {
                chunk
                    .map(|bytes| axum::body::Bytes::from(bytes))
                    .map_err(|e| {
                        std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
                    })
            });

            // Spawn background task to cache the response after streaming completes
            let state_bg = state.clone();
            let user_text_bg = user_text_for_cache.clone();
            let model_id_bg = model_id.clone();
            tokio::spawn(async move {
                match rx.await {
                    Ok(sse_bytes) => {
                        let sse_str = String::from_utf8_lossy(&sse_bytes);
                        if let Some(response_bytes) =
                            stream_reassemble::reconstruct_response(&sse_str)
                        {
                            if let (Some(cache), Some(embedder)) =
                                (&state_bg.cache, &state_bg.embedder)
                            {
                                if !user_text_bg.is_empty() {
                                    match embedder.embed_one(&user_text_bg).await {
                                        Ok(embedding) => {
                                            if let Err(e) = cache
                                                .put(
                                                    &embedding,
                                                    &response_bytes,
                                                    &model_id_bg,
                                                    tokens_saved,
                                                    state_bg.config.cache.ttl_seconds,
                                                )
                                                .await
                                            {
                                                tracing::warn!(
                                                    error = %e,
                                                    "Failed to cache streaming response"
                                                );
                                            } else {
                                                tracing::info!(
                                                    "Cached reconstructed streaming response"
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                error = %e,
                                                "Embedding failed for streaming cache store"
                                            );
                                        }
                                    }
                                }
                            }
                        } else {
                            tracing::debug!(
                                "Stream incomplete or unparseable, skipping cache"
                            );
                        }
                    }
                    Err(_) => {
                        tracing::debug!(
                            "Stream tee channel dropped (client disconnect?), skipping cache"
                        );
                    }
                }
            });

            let body = Body::from_stream(stream);
            let mut response = Response::new(body);
            *response.status_mut() = status;
            *response.headers_mut() = response_headers;

            return Ok(response);
        } else {
            // Non-cacheable streaming: simple passthrough
            let stream = upstream_response.bytes_stream().map(|chunk| {
                chunk
                    .map(|bytes| axum::body::Bytes::from(bytes))
                    .map_err(|e| {
                        std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
                    })
            });

            let body = Body::from_stream(stream);
            let mut response = Response::new(body);
            *response.status_mut() = status;
            *response.headers_mut() = response_headers;

            return Ok(response);
        }
    }

    // Non-streaming: buffer the full response
    let response_bytes = upstream_response.bytes().await.map_err(|e| {
        tracing::error!(error = %e, "Failed to read upstream response");
        StatusCode::BAD_GATEWAY
    })?;

    // Store in cache if successful
    let cache_status = if status == StatusCode::OK && state.config.cache.enabled {
        if let (Some(cache), Some(embedder)) = (&state.cache, &state.embedder) {
            if !user_text_for_cache.is_empty() {
                match embedder.embed_one(&user_text_for_cache).await {
                    Ok(embedding) => {
                        if let Err(e) = cache
                            .put(
                                &embedding,
                                &response_bytes,
                                &model_id,
                                tokens_saved,
                                state.config.cache.ttl_seconds,
                            )
                            .await
                        {
                            tracing::warn!(error = %e, "Failed to store in cache");
                        }
                        CacheStatus::Miss
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Embedding failed for cache store");
                        CacheStatus::Skipped
                    }
                }
            } else {
                CacheStatus::Skipped
            }
        } else {
            CacheStatus::Skipped
        }
    } else {
        CacheStatus::Skipped
    };

    tracing::info!(
        tokens_in = tokens_before,
        tokens_out = tokens_after,
        upstream_ms = upstream_duration.as_millis() as u64,
        response_status = status.as_u16(),
        "Request completed"
    );

    // Send update to TUI
    let _ = state.tui_tx.send(ProxyUpdate {
        tokens_original: tokens_before,
        tokens_compressed: tokens_after,
        events: pipeline_result.events,
        tool_calls: pipeline_result.tool_calls,
        cache_status,
        pipeline_duration,
        upstream_duration: Some(upstream_duration),
    });

    let mut response = Response::new(Body::from(response_bytes));
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;

    Ok(response)
}

/// Extract the LAST user message text for embedding (most recent question)
fn extract_user_text(body: &serde_json::Value) -> String {
    if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
        for msg in messages.iter().rev() {
            if msg.get("role").and_then(|r| r.as_str()) == Some("user") {
                if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
                    return s.to_string();
                }
                if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
                    for block in arr {
                        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                return text.to_string();
                            }
                        }
                    }
                }
            }
        }
    }
    String::new()
}

async fn health_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let uptime = state.start_time.elapsed().as_secs();
    axum::Json(serde_json::json!({
        "status": "healthy",
        "uptime": uptime,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}
