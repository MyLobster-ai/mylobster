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
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tower_http::cors::{Any, CorsLayer};
use tracing::{debug, error, warn};

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

/// Plugin route auth enforcement middleware.
///
/// Protects `/api/channels` and `/api/plugins` endpoints by requiring
/// a valid Authorization header with a bearer token. Rejects
/// broken-path variants that attempt to bypass authentication.
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
            // Check if gateway has token auth configured
            if state.auth.token.is_some() {
                warn!("Blocked unauthenticated access to protected path: {}", path);
                return StatusCode::UNAUTHORIZED.into_response();
            }
        }
    }

    next.run(request).await
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
        // Config
        .route("/api/config/schema", get(config_schema_handler))
        // Agents
        .route("/api/agents", get(agents_list_handler))
        .route("/api/agents/{id}", get(agent_get_handler))
        // Cron
        .route("/api/cron/jobs", get(cron_jobs_handler))
        .route("/api/cron/status", get(cron_status_handler))
        // Usage
        .route("/api/usage", get(usage_handler))
        // Status
        .route("/api/status", get(status_handler))
        // OpenAI-compatible endpoints
        .route("/v1/chat/completions", post(chat_completions_handler))
        .route("/v1/responses", post(responses_handler))
        // Security: path canonicalization + plugin route auth
        .layer(middleware::from_fn(security_path_middleware))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            plugin_route_auth_middleware,
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

async fn cron_status_handler(State(state): State<GatewayState>) -> Json<serde_json::Value> {
    let job_count = state.rpc.cron_jobs.read().len();
    Json(serde_json::json!({
        "running": true,
        "jobCount": job_count,
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
