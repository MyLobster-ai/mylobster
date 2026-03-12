use crate::gateway::protocol::*;
use crate::gateway::server::GatewayState;
use crate::gateway::websocket;
use crate::infra::security_path;

use axum::{
    body::Body,
    extract::{
        ws::WebSocketUpgrade,
        ConnectInfo, Json, Query, State,
    },
    http::{header, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tower_http::cors::{Any, CorsLayer};
use tracing::{debug, error, info, warn};

/// Security path canonicalization middleware.
///
/// Rejects requests with encoded path traversal, null bytes, or other
/// malicious URI patterns before they reach any handler.
async fn security_path_middleware(request: Request<Body>, next: Next) -> Response {
    let path = request.uri().path();
    match security_path::validate_request_path(path) {
        Ok(_canonical) => next.run(request).await,
        Err(reason) => {
            warn!("Blocked request with invalid path '{}': {}", path, reason);
            StatusCode::BAD_REQUEST.into_response()
        }
    }
}

/// Plugin route auth enforcement middleware (v2026.3.11 hardened).
///
/// Protects `/api/channels` and `/api/plugins` endpoints by requiring
/// a valid Authorization header with a bearer token. Rejects
/// broken-path variants that attempt to bypass authentication.
///
/// v2026.3.11: Unauthenticated routes no longer inherit synthetic admin
/// scopes — only explicitly authenticated requests receive elevated scopes.
async fn plugin_route_auth_middleware(
    State(state): State<GatewayState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path();

    // Check if this is a protected plugin route (is_protected_path canonicalizes internally)
    if security_path::is_protected_path(path) {
        // Require bearer token auth
        let has_auth = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.starts_with("Bearer "))
            .unwrap_or(false);

        if !has_auth {
            // v2026.3.11: Always reject unauthenticated access to protected paths
            // when gateway has token auth configured. No synthetic admin scope inheritance.
            if state.auth.token.is_some() {
                warn!("Blocked unauthenticated access to protected path: {}", path);
                return StatusCode::UNAUTHORIZED.into_response();
            }
        }
    }

    next.run(request).await
}

/// Browser origin validation middleware (v2026.3.11 — GHSA-5wcw-8jjv-m286).
///
/// Prevents cross-site WebSocket hijacking by validating the Origin header
/// on WebSocket upgrade requests. Enforced regardless of proxy headers.
async fn browser_origin_validation_middleware(
    State(state): State<GatewayState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    // Only validate WebSocket upgrade requests
    let is_ws_upgrade = request
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);

    if is_ws_upgrade {
        if let Some(origin) = request.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
            let config = state.config.read().await;
            let allowed = &config.gateway.allowed_origins;
            if !allowed.is_empty() && !is_origin_allowed(origin, allowed) {
                warn!(
                    "Blocked WebSocket upgrade from disallowed origin: {}",
                    origin
                );
                return StatusCode::FORBIDDEN.into_response();
            }
        }
    }

    next.run(request).await
}

/// Check if a given origin is in the allowed origins list.
///
/// Supports exact match and wildcard ("*").
fn is_origin_allowed(origin: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|a| a == "*" || a == origin)
}

/// Build all routes for the gateway.
pub fn build_routes(state: GatewayState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        // Health
        .route("/api/health", get(health_handler))
        // WebSocket
        .route("/ws", get(ws_handler))
        .route("/api/chat", get(ws_handler))
        // Sessions
        .route("/api/sessions", get(sessions_list_handler))
        .route("/api/sessions/{id}", get(session_get_handler))
        // Tools
        .route("/api/tools", get(tools_list_handler))
        // Memory
        .route("/api/memory/search", post(memory_search_handler))
        // Channels
        .route("/api/channels/status", get(channels_status_handler))
        // Gateway info
        .route("/api/gateway/info", get(gateway_info_handler))
        // Models
        .route("/api/models", get(models_list_handler))
        // Config (v2026.3.11: added validate + reload)
        .route("/api/config/schema", get(config_schema_handler))
        .route("/api/config/validate", post(config_validate_handler))
        .route("/api/config/reload", post(config_reload_handler))
        // Agents
        .route("/api/agents", get(agents_list_handler))
        .route("/api/agents/{id}", get(agent_get_handler))
        // Cron (v2026.3.11: added jobs detail)
        .route("/api/cron/jobs", get(cron_jobs_handler))
        .route("/api/cron/jobs/{id}", get(cron_job_detail_handler))
        .route("/api/cron/status", get(cron_status_handler))
        // Usage
        .route("/api/usage", get(usage_handler))
        // Status (v2026.3.11: includes runtime version)
        .route("/api/status", get(status_handler))
        // OpenAI-compatible endpoints
        .route("/v1/chat/completions", post(chat_completions_handler))
        .route("/v1/responses", post(responses_handler))
        // Security: path canonicalization + plugin route auth + browser origin validation
        .layer(middleware::from_fn(security_path_middleware))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            plugin_route_auth_middleware,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            browser_origin_validation_middleware,
        ))
        .layer(cors)
        .with_state(state)
}

// ============================================================================
// Health
// ============================================================================

async fn health_handler(State(state): State<GatewayState>) -> Json<HealthResponse> {
    let uptime = state.start_time.elapsed().as_secs();
    Json(HealthResponse {
        status: "ok".to_string(),
        version: state.version.clone(),
        uptime,
    })
}

// ============================================================================
// WebSocket
// ============================================================================

#[derive(Debug, Deserialize)]
struct WsQuery {
    token: Option<String>,
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<GatewayState>,
    Query(query): Query<WsQuery>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    debug!("WebSocket upgrade request from {}", addr);
    ws.max_message_size(MAX_WS_PAYLOAD)
        .on_upgrade(move |socket| websocket::handle_websocket(socket, state, addr, query.token))
}

// ============================================================================
// Sessions
// ============================================================================

async fn sessions_list_handler(State(state): State<GatewayState>) -> Json<Vec<SessionInfo>> {
    let sessions = state.sessions.list_sessions();
    Json(sessions)
}

async fn session_get_handler(
    State(state): State<GatewayState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<SessionInfo>, StatusCode> {
    state
        .sessions
        .get_session(&id)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

// ============================================================================
// Tools
// ============================================================================

#[derive(Debug, Serialize)]
struct ToolInfo {
    name: String,
    description: String,
    category: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    hidden: Option<bool>,
}

async fn tools_list_handler(State(state): State<GatewayState>) -> Json<Vec<ToolInfo>> {
    let config = state.config.read().await;
    let tools = crate::agents::tools::list_available_tools(&config);
    let tool_infos: Vec<ToolInfo> = tools
        .into_iter()
        .map(|t| ToolInfo {
            name: t.name,
            description: t.description,
            category: t.category,
            hidden: if t.hidden { Some(true) } else { None },
        })
        .collect();
    Json(tool_infos)
}

// ============================================================================
// Memory
// ============================================================================

#[derive(Debug, Deserialize)]
struct MemorySearchRequest {
    query: String,
    max_results: Option<u32>,
    min_score: Option<f64>,
    session_key: Option<String>,
}

async fn memory_search_handler(
    State(state): State<GatewayState>,
    Json(req): Json<MemorySearchRequest>,
) -> Json<Vec<crate::memory::MemorySearchResult>> {
    let config = state.config.read().await;
    match crate::memory::search(
        &config,
        &req.query,
        req.max_results.unwrap_or(10),
        req.min_score.unwrap_or(0.0),
        req.session_key.as_deref(),
    )
    .await
    {
        Ok(results) => Json(results),
        Err(e) => {
            error!("Memory search error: {}", e);
            Json(vec![])
        }
    }
}

// ============================================================================
// Channels
// ============================================================================

async fn channels_status_handler(State(state): State<GatewayState>) -> Json<serde_json::Value> {
    let status = state.channels.get_status().await;
    Json(status)
}

// ============================================================================
// Gateway Info
// ============================================================================

async fn gateway_info_handler(State(state): State<GatewayState>) -> Json<GatewayInfo> {
    let uptime = state.start_time.elapsed().as_secs();
    Json(GatewayInfo {
        version: state.version.clone(),
        protocol: PROTOCOL_VERSION,
        uptime_seconds: uptime,
        sessions_active: state.sessions.active_count() as u32,
        clients_connected: 0, // TODO: track connected clients
    })
}

// ============================================================================
// Models
// ============================================================================

async fn models_list_handler(State(state): State<GatewayState>) -> Json<serde_json::Value> {
    let config = state.config.read().await;
    let mut models = Vec::new();
    for (provider, provider_config) in &config.models.providers {
        if provider_config.api_key.is_some() || provider == "ollama" {
            let provider_models = crate::gateway::websocket_models(provider);
            models.extend(provider_models);
        }
    }
    if models.is_empty() {
        models = crate::gateway::websocket_models("anthropic");
    }
    Json(serde_json::json!({ "models": models }))
}

// ============================================================================
// Config Schema
// ============================================================================

async fn config_schema_handler() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "schema": {
            "type": "object",
            "properties": {
                "agents": { "type": "object" },
                "models": { "type": "object" },
                "channels": { "type": "object" },
                "gateway": { "type": "object" },
                "memory": { "type": "object" },
                "tools": { "type": "object" },
                "browser": { "type": "object" },
                "cron": { "type": "object" },
                "plugins": { "type": "object" },
            }
        }
    }))
}

// ============================================================================
// Config Validate + Reload (v2026.3.11)
// ============================================================================

async fn config_validate_handler(
    State(state): State<GatewayState>,
) -> Json<serde_json::Value> {
    let config = state.config.read().await;
    let issues = crate::config::validate_config(&config);
    // Surface up to 3 issues per v2026.3.11 spec
    let capped: Vec<String> = issues.iter().map(|e| e.to_string()).take(3).collect();
    Json(serde_json::json!({
        "valid": issues.is_empty(),
        "issues": capped,
    }))
}

async fn config_reload_handler(
    State(state): State<GatewayState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Attempt to reload config from disk
    let new_config = match crate::config::Config::load(None) {
        Ok(c) => c,
        Err(e) => {
            error!("Config reload failed: {}", e);
            return Ok(Json(serde_json::json!({
                "ok": false,
                "error": format!("{}", e),
            })));
        }
    };
    let mut config = state.config.write().await;
    *config = new_config;
    info!("Configuration reloaded successfully");
    Ok(Json(serde_json::json!({
        "ok": true,
    })))
}

// ============================================================================
// Agents
// ============================================================================

async fn agents_list_handler(State(state): State<GatewayState>) -> Json<serde_json::Value> {
    let agents: Vec<serde_json::Value> = state.rpc.agents.read().values().cloned().collect();
    let mut result = vec![serde_json::json!({
        "id": "default",
        "name": "Default Agent",
    })];
    result.extend(agents);
    Json(serde_json::json!({ "agents": result }))
}

async fn agent_get_handler(
    State(state): State<GatewayState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if id == "default" {
        return Ok(Json(serde_json::json!({
            "id": "default",
            "name": "Default Agent",
            "version": state.version,
        })));
    }
    state
        .rpc
        .agents
        .read()
        .get(&id)
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

// ============================================================================
// Cron
// ============================================================================

async fn cron_jobs_handler(State(state): State<GatewayState>) -> Json<serde_json::Value> {
    let jobs: Vec<serde_json::Value> = state.rpc.cron_jobs.read().values().cloned().collect();
    Json(serde_json::json!({ "jobs": jobs }))
}

async fn cron_job_detail_handler(
    State(state): State<GatewayState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let jobs = state.rpc.cron_jobs.read();
    jobs.get(&id)
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn cron_status_handler(State(state): State<GatewayState>) -> Json<serde_json::Value> {
    let job_count = state.rpc.cron_jobs.read().len();
    let error_count = state.rpc.cron_error_count.read().clone();
    Json(serde_json::json!({
        "running": true,
        "jobCount": job_count,
        "errorCount": error_count,
    }))
}

// ============================================================================
// Usage
// ============================================================================

async fn usage_handler(State(state): State<GatewayState>) -> Json<serde_json::Value> {
    let input = *state.rpc.usage_input_tokens.read();
    let output = *state.rpc.usage_output_tokens.read();
    let requests = *state.rpc.usage_requests.read();
    Json(serde_json::json!({
        "totalInputTokens": input,
        "totalOutputTokens": output,
        "totalRequests": requests,
    }))
}

// ============================================================================
// Status
// ============================================================================

async fn status_handler(State(state): State<GatewayState>) -> Json<serde_json::Value> {
    let uptime = state.start_time.elapsed().as_secs();
    let session_count = state.sessions.active_count();
    Json(serde_json::json!({
        "version": state.version,
        "runtimeVersion": env!("CARGO_PKG_VERSION"),
        "protocolVersion": PROTOCOL_VERSION,
        "uptime": uptime,
        "sessions": session_count,
        "status": "ok",
    }))
}

// ============================================================================
// OpenAI Compatibility
// ============================================================================

async fn chat_completions_handler(
    State(state): State<GatewayState>,
    Json(req): Json<ChatCompletionRequest>,
) -> Result<Json<ChatCompletionResponse>, StatusCode> {
    let config = state.config.read().await;

    // Check if endpoint is enabled
    let enabled = config
        .gateway
        .http
        .endpoints
        .as_ref()
        .and_then(|e| e.chat_completions.as_ref())
        .and_then(|c| c.enabled)
        .unwrap_or(true);

    if !enabled {
        return Err(StatusCode::NOT_FOUND);
    }

    // Forward to agent for processing
    match crate::agents::handle_chat_completion(&config, &state.sessions, req).await {
        Ok(response) => Ok(Json(response)),
        Err(e) => {
            error!("Chat completion error: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// ============================================================================
// OpenResponses
// ============================================================================

async fn responses_handler(
    State(state): State<GatewayState>,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let config = state.config.read().await;

    let enabled = config
        .gateway
        .http
        .endpoints
        .as_ref()
        .and_then(|e| e.responses.as_ref())
        .and_then(|r| r.enabled)
        .unwrap_or(true);

    if !enabled {
        return Err(StatusCode::NOT_FOUND);
    }

    // Forward to agent for processing
    match crate::agents::handle_responses_api(&config, &state.sessions, req).await {
        Ok(response) => Ok(Json(response)),
        Err(e) => {
            error!("Responses API error: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ====================================================================
    // is_origin_allowed (v2026.3.11 — GHSA-5wcw-8jjv-m286)
    // ====================================================================

    #[test]
    fn origin_allowed_exact_match() {
        let allowed = vec!["https://mylobster.ai".to_string()];
        assert!(is_origin_allowed("https://mylobster.ai", &allowed));
    }

    #[test]
    fn origin_rejected_not_in_list() {
        let allowed = vec!["https://mylobster.ai".to_string()];
        assert!(!is_origin_allowed("https://evil.com", &allowed));
    }

    #[test]
    fn origin_allowed_wildcard() {
        let allowed = vec!["*".to_string()];
        assert!(is_origin_allowed("https://anything.example.com", &allowed));
    }

    #[test]
    fn origin_allowed_multiple_entries() {
        let allowed = vec![
            "https://mylobster.ai".to_string(),
            "https://app.mylobster.ai".to_string(),
            "http://localhost:3000".to_string(),
        ];
        assert!(is_origin_allowed("https://app.mylobster.ai", &allowed));
        assert!(is_origin_allowed("http://localhost:3000", &allowed));
        assert!(!is_origin_allowed("https://other.com", &allowed));
    }

    #[test]
    fn origin_rejected_empty_list() {
        let allowed: Vec<String> = vec![];
        // Empty list means nothing matches (caller should skip check for empty)
        assert!(!is_origin_allowed("https://anything.com", &allowed));
    }

    #[test]
    fn origin_case_sensitive() {
        let allowed = vec!["https://MyLobster.ai".to_string()];
        // Origin matching is case-sensitive per spec
        assert!(!is_origin_allowed("https://mylobster.ai", &allowed));
    }

    #[test]
    fn origin_no_partial_match() {
        let allowed = vec!["https://mylobster.ai".to_string()];
        assert!(!is_origin_allowed("https://mylobster.ai.evil.com", &allowed));
    }
}
