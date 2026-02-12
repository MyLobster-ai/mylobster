use crate::gateway::protocol::*;
use crate::gateway::server::GatewayState;
use crate::gateway::websocket;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, Json, Query, State,
    },
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use tower_http::cors::{Any, CorsLayer};
use tracing::{debug, error, info};

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
        // OpenAI-compatible endpoints
        .route("/v1/chat/completions", post(chat_completions_handler))
        .route("/v1/responses", post(responses_handler))
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

async fn sessions_list_handler(
    State(state): State<GatewayState>,
) -> Json<Vec<SessionInfo>> {
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

async fn channels_status_handler(
    State(state): State<GatewayState>,
) -> Json<serde_json::Value> {
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
