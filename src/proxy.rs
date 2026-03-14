use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderName, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use reqwest::Client;
use std::sync::Arc;
use std::time::Instant;

use crate::config::JanusConfig;
use crate::tokenizer::Tokenizer;

pub struct AppState {
    pub config: JanusConfig,
    pub client: Client,
    pub start_time: Instant,
    pub tokenizer: Tokenizer,
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

    // Run compression pipeline
    let pipeline_start = Instant::now();
    let events = crate::pipeline::process(
        &mut body_json,
        &state.tokenizer,
        &state.config.pipeline,
    );
    let pipeline_duration = pipeline_start.elapsed();

    let tokens_after = state.tokenizer.count_message_tokens(&body_json);
    let tokens_saved = tokens_before.saturating_sub(tokens_after);

    tracing::info!(
        tokens_before = tokens_before,
        tokens_after = tokens_after,
        tokens_saved = tokens_saved,
        pipeline_ms = pipeline_duration.as_millis() as u64,
        stages = events.len(),
        "Compression complete"
    );

    for event in &events {
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

    let response_bytes = upstream_response.bytes().await.map_err(|e| {
        tracing::error!(error = %e, "Failed to read upstream response");
        StatusCode::BAD_GATEWAY
    })?;

    tracing::info!(
        tokens_in = tokens_before,
        tokens_out = tokens_after,
        upstream_ms = upstream_duration.as_millis() as u64,
        response_status = status.as_u16(),
        "Request completed"
    );

    let mut response = Response::new(Body::from(response_bytes));
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;

    Ok(response)
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
