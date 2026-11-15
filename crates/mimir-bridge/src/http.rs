//! Localhost-only HTTP REST + SSE API with mandatory bearer token and Host validation.

use crate::mcp::wrap_untrusted_transcript;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use axum::Router;
use mimir_core::{MimirConfig, MimirError, Result, TranscriptFormat, Utterance, restrict_file_permissions};
use mimir_store::{SessionStore, export::render_session};
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast;
use uuid::Uuid;

#[derive(Clone)]
pub struct HttpState {
    pub data_dir: PathBuf,
    pub bearer_token: String,
    pub live_tx: broadcast::Sender<Utterance>,
}

#[derive(Debug, Deserialize)]
pub struct TranscriptQuery {
    #[serde(default = "default_format")]
    pub format: String,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_format() -> String {
    "markdown".to_string()
}

fn default_limit() -> usize {
    20
}

/// Loads or creates a `0600` per-user random bearer token in `cfg.token_path()`.
pub fn ensure_bearer_token(cfg: &MimirConfig) -> Result<String> {
    std::fs::create_dir_all(&cfg.data_dir)?;
    let path = cfg.token_path();
    if path.exists() {
        let tok = std::fs::read_to_string(&path)?.trim().to_string();
        if !tok.is_empty() {
            return Ok(tok);
        }
    }
    let token = format!("mimir_{}_{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    std::fs::write(&path, &token)?;
    restrict_file_permissions(&path)?;
    Ok(token)
}

/// Builds the Axum router with strict loopback `Host` + bearer token enforcement.
pub fn build_router(state: Arc<HttpState>) -> Router {
    Router::new()
        .route("/health", get(health_handler))
        .route("/openapi.json", get(openapi_handler))
        .route("/v1/sessions", get(list_sessions_handler))
        .route("/v1/sessions/{id}", get(get_session_handler))
        .route("/v1/search", get(search_handler))
        .route("/v1/live/stream", get(live_stream_handler))
        .with_state(state)
}

/// Starts the local HTTP server bound strictly to `bind_addr` (must be loopback).
pub async fn serve_http(cfg: &MimirConfig, live_tx: broadcast::Sender<Utterance>) -> Result<()> {
    let token = ensure_bearer_token(cfg)?;
    let state = Arc::new(HttpState {
        data_dir: cfg.data_dir.clone(),
        bearer_token: token,
        live_tx,
    });

    let listener = tokio::net::TcpListener::bind(&cfg.http_bind_addr)
        .await
        .map_err(|e| MimirError::Config(format!("failed to bind {}: {e}", cfg.http_bind_addr)))?;

    axum::serve(listener, build_router(state))
        .await
        .map_err(|e| MimirError::Config(format!("http server error: {e}")))?;
    Ok(())
}

fn authorize(headers: &HeaderMap, expected_token: &str) -> std::result::Result<(), StatusCode> {
    // DNS-rebinding defense: only allow loopback Host headers.
    if let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        let host_only = host.split(':').next().unwrap_or("");
        if host_only != "127.0.0.1" && host_only != "localhost" && host_only != "[::1]" {
            return Err(StatusCode::FORBIDDEN);
        }
    }

    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let provided = auth.strip_prefix("Bearer ").ok_or(StatusCode::UNAUTHORIZED)?;
    if provided != expected_token {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(())
}

async fn health_handler() -> impl IntoResponse {
    Json(json!({ "status": "ok", "service": "mimir" }))
}

async fn openapi_handler(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
) -> std::result::Result<impl IntoResponse, StatusCode> {
    authorize(&headers, &state.bearer_token)?;
    Ok(Json(json!({
        "openapi": "3.1.0",
        "info": { "title": "Mimir Local Transcript API", "version": env!("CARGO_PKG_VERSION") },
        "paths": {
            "/v1/sessions": { "get": { "summary": "List recent transcription sessions" } },
            "/v1/sessions/{id}": { "get": { "summary": "Get transcript by UUID or 'latest'" } },
            "/v1/search": { "get": { "summary": "Full-text search across transcripts" } },
            "/v1/live/stream": { "get": { "summary": "SSE stream of live utterances" } }
        }
    })))
}

async fn list_sessions_handler(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
) -> std::result::Result<impl IntoResponse, StatusCode> {
    authorize(&headers, &state.bearer_token)?;
    let store = SessionStore::open(&state.data_dir).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let list = store.list_sessions(50).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(list))
}

async fn get_session_handler(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<TranscriptQuery>,
) -> std::result::Result<impl IntoResponse, StatusCode> {
    authorize(&headers, &state.bearer_token)?;
    let store = SessionStore::open(&state.data_dir).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let session = if id == "latest" {
        store.latest_session().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    } else {
        let uuid = Uuid::parse_str(&id).map_err(|_| StatusCode::BAD_REQUEST)?;
        store.get_session(uuid).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    };

    let Some(session) = session else {
        return Err(StatusCode::NOT_FOUND);
    };

    let fmt = match q.format.as_str() {
        "json" => TranscriptFormat::Json,
        "jsonl" => TranscriptFormat::Jsonl,
        "srt" => TranscriptFormat::Srt,
        _ => TranscriptFormat::Markdown,
    };

    let rendered = render_session(&session, fmt).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({
        "session_id": session.id,
        "format": q.format,
        "transcript": wrap_untrusted_transcript(&rendered),
    })))
}

async fn search_handler(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    Query(q): Query<SearchQuery>,
) -> std::result::Result<impl IntoResponse, StatusCode> {
    authorize(&headers, &state.bearer_token)?;
    let store = SessionStore::open(&state.data_dir).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let hits = store
        .search(&q.q, q.limit.min(50))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(hits))
}

async fn live_stream_handler(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
) -> std::result::Result<Sse<impl tokio_stream::Stream<Item = std::result::Result<Event, Infallible>>>, StatusCode> {
    authorize(&headers, &state.bearer_token)?;
    let rx = state.live_tx.subscribe();
    let stream = tokio_stream::StreamExt::filter_map(
        tokio_stream::wrappers::BroadcastStream::new(rx),
        |item| match item {
            Ok(utterance) => {
                let data = serde_json::to_string(&utterance).unwrap_or_default();
                Some(Ok(Event::default().data(data)))
            }
            Err(_) => None,
        },
    );
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use tempfile::TempDir;

    #[test]
    fn authorize_rejects_missing_token_and_non_loopback_host() {
        let mut headers = HeaderMap::new();
        assert_eq!(authorize(&headers, "secret"), Err(StatusCode::UNAUTHORIZED));

        headers.insert(header::HOST, HeaderValue::from_static("evil.example.com"));
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
        assert_eq!(authorize(&headers, "secret"), Err(StatusCode::FORBIDDEN));

        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:7331"));
        assert_eq!(authorize(&headers, "secret"), Ok(()));
    }

    #[test]
    fn ensure_bearer_token_persists_and_reuses() {
        let dir = TempDir::new().unwrap();
        let cfg = MimirConfig {
            data_dir: dir.path().to_path_buf(),
            ..Default::default()
        };
        let t1 = ensure_bearer_token(&cfg).unwrap();
        let t2 = ensure_bearer_token(&cfg).unwrap();
        assert_eq!(t1, t2);
        assert!(t1.starts_with("mimir_"));
    }
}
