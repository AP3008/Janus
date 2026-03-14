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

    // Parse body for token counting
    let body_json: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
        tracing::error!(error = %e, "Failed to parse request body as JSON");
        StatusCode::BAD_REQUEST
    })?;

    let tokens_before = state.tokenizer.count_message_tokens(&body_json);

    tracing::info!(
        tokens = tokens_before,
        upstream = %upstream_url,
        "Proxying request"
    );

    // Build upstream request with forwarded headers
    let mut req = state.client.post(&upstream_url);
    for &header_name in FORWARD_HEADERS {
        if let Some(value) = headers.get(header_name) {
            if let Ok(name) = HeaderName::from_bytes(header_name.as_bytes()) {
                req = req.header(name, value.clone());
            }
        }
    }

    // Forward the body (using original bytes for now, compression comes later)
    let upstream_start = Instant::now();
    let upstream_response = req
        .body(body.to_vec())
        .send()
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to forward request to upstream");
            StatusCode::BAD_GATEWAY
        })?;
    let upstream_duration = upstream_start.elapsed();

    // Build response with upstream status and headers
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
