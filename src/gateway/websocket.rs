use crate::gateway::auth::{authorize_gateway_connect, is_local_request, ConnectAuth};
use crate::gateway::protocol::*;
use crate::gateway::server::GatewayState;

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Handle a WebSocket connection.
pub async fn handle_websocket(
    socket: WebSocket,
    state: GatewayState,
    addr: SocketAddr,
    query_token: Option<String>,
) {
    let client_id = Uuid::new_v4().to_string();
    info!("WebSocket client connected: {} from {}", client_id, addr);

    let (mut ws_tx, mut ws_rx) = socket.split();

    // Send hello frame
    let hello = HelloFrame {
        protocol: PROTOCOL_VERSION,
        server: "mylobster".to_string(),
        version: state.version.clone(),
        capabilities: Some(vec![
            "chat".to_string(),
            "sessions".to_string(),
            "tools".to_string(),
            "channels".to_string(),
            "memory".to_string(),
        ]),
        challenge: None,
    };

    let hello_json = serde_json::to_string(&Frame::Hello(hello)).unwrap();
    if ws_tx.send(Message::Text(hello_json.into())).await.is_err() {
        error!("Failed to send hello frame to {}", client_id);
        return;
    }

    // Authentication: check query token or wait for connect message
    let is_local = is_local_request(&addr);
    let auth_result = if let Some(ref token) = query_token {
        authorize_gateway_connect(
            &state.auth,
            Some(&ConnectAuth {
                token: Some(token.clone()),
                password: None,
            }),
            is_local,
        )
    } else {
        // For local connections without token, check if auth is configured
        authorize_gateway_connect(&state.auth, None, is_local)
    };

    if !auth_result.ok {
        warn!(
            "WebSocket auth failed for {}: {:?}",
            client_id, auth_result.reason
        );
        let error_frame = ResponseFrame {
            id: "auth".to_string(),
            result: None,
            error: Some(ProtocolError {
                code: 1008,
                message: auth_result
                    .reason
                    .unwrap_or_else(|| "Authentication failed".to_string()),
                data: None,
            }),
            seq: Some(0),
        };
        let error_json = serde_json::to_string(&Frame::Response(error_frame)).unwrap();
        let _ = ws_tx.send(Message::Text(error_json.into())).await;
        let _ = ws_tx.close().await;
        return;
    }

    info!(
        "WebSocket client authenticated: {} via {:?}",
        client_id, auth_result.method
    );

    // Channel for sending messages to this client
    let (tx, mut rx) = mpsc::channel::<String>(256);
    let seq = Arc::new(AtomicU64::new(1));

    // Spawn writer task
    let writer_seq = seq.clone();
    let writer_client_id = client_id.clone();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_tx.send(Message::Text(msg.into())).await.is_err() {
                debug!("WebSocket write failed for {}", writer_client_id);
                break;
            }
        }
    });

    // Process incoming messages
    while let Some(msg) = ws_rx.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                let text_str: &str = &text;
                match serde_json::from_str::<RequestFrame>(text_str) {
                    Ok(request) => {
                        handle_request(&state, &client_id, &tx, &seq, request).await;
                    }
                    Err(e) => {
                        debug!("Failed to parse WebSocket message from {}: {}", client_id, e);
                        let error_response = ResponseFrame {
                            id: "parse-error".to_string(),
                            result: None,
                            error: Some(ProtocolError {
                                code: -32700,
                                message: format!("Parse error: {}", e),
                                data: None,
                            }),
                            seq: Some(seq.fetch_add(1, Ordering::SeqCst)),
                        };
                        let json = serde_json::to_string(&error_response).unwrap();
                        let _ = tx.send(json).await;
                    }
                }
            }
            Ok(Message::Ping(_data)) => {
                // Pong is automatically handled by the axum WebSocket layer.
            }
            Ok(Message::Close(_)) => {
                info!("WebSocket client {} disconnected", client_id);
                break;
            }
            Err(e) => {
                error!("WebSocket error for {}: {}", client_id, e);
                break;
            }
            _ => {}
        }
    }

    info!("WebSocket client {} connection closed", client_id);
}

/// Handle a parsed WebSocket request.
async fn handle_request(
    state: &GatewayState,
    client_id: &str,
    tx: &mpsc::Sender<String>,
    seq: &Arc<AtomicU64>,
    request: RequestFrame,
) {
    let response_seq = seq.fetch_add(1, Ordering::SeqCst);

    debug!(
        "Handling request '{}' (id={}) from {}",
        request.method, request.id, client_id
    );

    let result = match request.method.as_str() {
        "chat.send" => handle_chat_send(state, client_id, tx, seq, &request).await,
        "sessions.list" => handle_sessions_list(state, &request).await,
        "sessions.get" => handle_sessions_get(state, &request).await,
        "sessions.patch" => handle_sessions_patch(state, &request).await,
        "sessions.delete" => handle_sessions_delete(state, &request).await,
        "tools.list" => handle_tools_list(state).await,
        "channels.status" => handle_channels_status(state).await,
        "memory.search" => handle_memory_search(state, &request).await,
        "gateway.info" => handle_gateway_info(state).await,
        "config.reload" => handle_config_reload(state).await,
        "presence.set" => handle_presence_set(state, &request).await,
        "cron.list" => handle_cron_list(state).await,
        _ => Err(ProtocolError {
            code: -32601,
            message: format!("Method not found: {}", request.method),
            data: None,
        }),
    };

    let response = ResponseFrame {
        id: request.id,
        result: result.as_ref().ok().cloned(),
        error: result.err(),
        seq: Some(response_seq),
    };

    let json = serde_json::to_string(&response).unwrap();
    let _ = tx.send(json).await;
}

// ============================================================================
// Method Handlers
// ============================================================================

async fn handle_chat_send(
    state: &GatewayState,
    client_id: &str,
    tx: &mpsc::Sender<String>,
    seq: &Arc<AtomicU64>,
    request: &RequestFrame,
) -> Result<serde_json::Value, ProtocolError> {
    let params: ChatSendParams = serde_json::from_value(
        request.params.clone().unwrap_or(serde_json::Value::Null),
    )
    .map_err(|e| ProtocolError {
        code: -32602,
        message: format!("Invalid params: {}", e),
        data: None,
    })?;

    let config = state.config.read().await;
    let run_id = Uuid::new_v4().to_string();

    // Stream chat events back to client
    let tx = tx.clone();
    let seq = seq.clone();
    let session_key = params.session_key.clone();
    let run_id_clone = run_id.clone();

    tokio::spawn(async move {
        // Send initial delta
        let event = ChatEvent {
            run_id: run_id_clone.clone(),
            session_key: session_key.clone(),
            seq: seq.fetch_add(1, Ordering::SeqCst),
            state: ChatEventState::Delta,
            message: Some(serde_json::json!({
                "role": "assistant",
                "content": ""
            })),
            error_message: None,
            usage: None,
            stop_reason: None,
        };
        let event_frame = EventFrame {
            event: "chat.event".to_string(),
            data: Some(serde_json::to_value(&event).unwrap()),
            seq: Some(seq.fetch_add(1, Ordering::SeqCst)),
        };
        let json = serde_json::to_string(&Frame::Event(event_frame)).unwrap();
        let _ = tx.send(json).await;

        // TODO: Actually call the AI provider and stream response
        // For now, send a placeholder final event
        let final_event = ChatEvent {
            run_id: run_id_clone,
            session_key,
            seq: seq.fetch_add(1, Ordering::SeqCst),
            state: ChatEventState::Final,
            message: Some(serde_json::json!({
                "role": "assistant",
                "content": "Hello! I'm MyLobster, your AI assistant. How can I help you today?"
            })),
            error_message: None,
            usage: Some(TokenUsage {
                input_tokens: Some(0),
                output_tokens: Some(0),
                cache_read_tokens: None,
                cache_write_tokens: None,
            }),
            stop_reason: Some("end_turn".to_string()),
        };
        let event_frame = EventFrame {
            event: "chat.event".to_string(),
            data: Some(serde_json::to_value(&final_event).unwrap()),
            seq: Some(seq.fetch_add(1, Ordering::SeqCst)),
        };
        let json = serde_json::to_string(&Frame::Event(event_frame)).unwrap();
        let _ = tx.send(json).await;
    });

    Ok(serde_json::json!({ "runId": run_id }))
}

async fn handle_sessions_list(
    state: &GatewayState,
    _request: &RequestFrame,
) -> Result<serde_json::Value, ProtocolError> {
    let sessions = state.sessions.list_sessions();
    Ok(serde_json::to_value(sessions).unwrap())
}

async fn handle_sessions_get(
    state: &GatewayState,
    request: &RequestFrame,
) -> Result<serde_json::Value, ProtocolError> {
    let params = request.params.as_ref().ok_or(ProtocolError {
        code: -32602,
        message: "Missing params".to_string(),
        data: None,
    })?;

    let session_key = params
        .get("sessionKey")
        .and_then(|v| v.as_str())
        .ok_or(ProtocolError {
            code: -32602,
            message: "Missing sessionKey".to_string(),
            data: None,
        })?;

    state
        .sessions
        .get_session(session_key)
        .map(|s| serde_json::to_value(s).unwrap())
        .ok_or(ProtocolError {
            code: -32600,
            message: "Session not found".to_string(),
            data: None,
        })
}

async fn handle_sessions_patch(
    state: &GatewayState,
    request: &RequestFrame,
) -> Result<serde_json::Value, ProtocolError> {
    let params: SessionPatchParams = serde_json::from_value(
        request.params.clone().unwrap_or(serde_json::Value::Null),
    )
    .map_err(|e| ProtocolError {
        code: -32602,
        message: format!("Invalid params: {}", e),
        data: None,
    })?;

    state.sessions.patch_session(&params);
    Ok(serde_json::json!({ "ok": true }))
}

async fn handle_sessions_delete(
    state: &GatewayState,
    request: &RequestFrame,
) -> Result<serde_json::Value, ProtocolError> {
    let params = request.params.as_ref().ok_or(ProtocolError {
        code: -32602,
        message: "Missing params".to_string(),
        data: None,
    })?;

    let session_key = params
        .get("sessionKey")
        .and_then(|v| v.as_str())
        .ok_or(ProtocolError {
            code: -32602,
            message: "Missing sessionKey".to_string(),
            data: None,
        })?;

    state.sessions.delete_session(session_key);
    Ok(serde_json::json!({ "ok": true }))
}

async fn handle_tools_list(
    state: &GatewayState,
) -> Result<serde_json::Value, ProtocolError> {
    let config = state.config.read().await;
    let tools = crate::agents::tools::list_available_tools(&config);
    Ok(serde_json::to_value(tools).unwrap())
}

async fn handle_channels_status(
    state: &GatewayState,
) -> Result<serde_json::Value, ProtocolError> {
    let status = state.channels.get_status().await;
    Ok(status)
}

async fn handle_memory_search(
    state: &GatewayState,
    request: &RequestFrame,
) -> Result<serde_json::Value, ProtocolError> {
    let params = request.params.as_ref().ok_or(ProtocolError {
        code: -32602,
        message: "Missing params".to_string(),
        data: None,
    })?;

    let query = params
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or(ProtocolError {
            code: -32602,
            message: "Missing query".to_string(),
            data: None,
        })?;

    let max_results = params
        .get("maxResults")
        .and_then(|v| v.as_u64())
        .unwrap_or(10) as u32;

    let config = state.config.read().await;
    match crate::memory::search(&config, query, max_results, 0.0, None).await {
        Ok(results) => Ok(serde_json::to_value(results).unwrap()),
        Err(e) => Err(ProtocolError {
            code: -32603,
            message: format!("Memory search failed: {}", e),
            data: None,
        }),
    }
}

async fn handle_gateway_info(
    state: &GatewayState,
) -> Result<serde_json::Value, ProtocolError> {
    let uptime = state.start_time.elapsed().as_secs();
    Ok(serde_json::json!({
        "version": state.version,
        "protocol": PROTOCOL_VERSION,
        "uptimeSeconds": uptime,
        "sessionsActive": state.sessions.active_count(),
    }))
}

async fn handle_config_reload(
    state: &GatewayState,
) -> Result<serde_json::Value, ProtocolError> {
    info!("Config reload requested");
    // TODO: Implement hot-reload
    Ok(serde_json::json!({ "ok": true }))
}

async fn handle_presence_set(
    state: &GatewayState,
    _request: &RequestFrame,
) -> Result<serde_json::Value, ProtocolError> {
    // TODO: Implement presence
    Ok(serde_json::json!({ "ok": true }))
}

async fn handle_cron_list(
    state: &GatewayState,
) -> Result<serde_json::Value, ProtocolError> {
    let jobs = crate::cron::list_jobs();
    Ok(serde_json::to_value(jobs).unwrap())
}
