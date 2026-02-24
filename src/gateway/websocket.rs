use crate::gateway::auth::{
    authorize_connect_auth, is_local_request, verify_device_identity,
};
use crate::gateway::chat;
use crate::gateway::protocol::*;
use crate::gateway::server::GatewayState;

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Handle a WebSocket connection using the OC v2026.2.22 protocol.
///
/// Flow:
/// 1. Send `connect.challenge` event with a random nonce
/// 2. Wait for `connect` request with auth + device params
/// 3. Validate token auth and device identity
/// 4. Send success/failure response
/// 5. Process subsequent requests (chat.send, sessions.*, config.*, etc.)
pub async fn handle_websocket(
    socket: WebSocket,
    state: GatewayState,
    addr: SocketAddr,
    _query_token: Option<String>,
) {
    let client_id = Uuid::new_v4().to_string();
    info!("WebSocket client connected: {} from {}", client_id, addr);

    let (mut ws_tx, mut ws_rx) = socket.split();

    // Generate challenge nonce
    let challenge_nonce = Uuid::new_v4().to_string();
    let challenge_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    // Step 1: Send connect.challenge event
    let challenge_event = OcEventFrame::new(
        "connect.challenge",
        serde_json::json!({
            "nonce": challenge_nonce,
            "ts": challenge_ts,
        }),
    );
    let challenge_json = serde_json::to_string(&challenge_event).unwrap();
    if ws_tx
        .send(Message::Text(challenge_json.into()))
        .await
        .is_err()
    {
        error!("Failed to send connect.challenge to {}", client_id);
        return;
    }

    // Step 2: Wait for `connect` request (with timeout)
    let is_local = is_local_request(&addr);
    let mut conn_state = ConnectionState {
        client_id: client_id.clone(),
        handshake_complete: false,
        challenge_nonce: challenge_nonce.clone(),
        scopes: Vec::new(),
        user_id: None,
        session_id: Uuid::new_v4().to_string(),
    };

    let handshake_timeout = tokio::time::Duration::from_secs(10);
    let connect_result = tokio::time::timeout(handshake_timeout, async {
        while let Some(msg) = ws_rx.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    let text_str: &str = &text;
                    match serde_json::from_str::<IncomingFrame>(text_str) {
                        Ok(frame) => {
                            let request = frame.into_request();
                            if request.method == "connect" {
                                return Some(request);
                            }
                            // Non-connect messages before handshake — ignore
                            debug!(
                                "Ignoring pre-handshake message '{}' from {}",
                                request.method, client_id
                            );
                        }
                        Err(e) => {
                            debug!("Failed to parse handshake message from {}: {}", client_id, e);
                        }
                    }
                }
                Ok(Message::Close(_)) => return None,
                Err(e) => {
                    error!("WebSocket error during handshake for {}: {}", client_id, e);
                    return None;
                }
                _ => {}
            }
        }
        None
    })
    .await;

    let connect_request = match connect_result {
        Ok(Some(req)) => req,
        Ok(None) => {
            warn!("Client {} disconnected during handshake", client_id);
            return;
        }
        Err(_) => {
            warn!("Handshake timeout for client {}", client_id);
            let timeout_err = OcResponseFrame::error(
                "handshake-timeout".to_string(),
                "Handshake timeout".to_string(),
                Some(1008),
            );
            let json = serde_json::to_string(&timeout_err).unwrap();
            let _ = ws_tx.send(Message::Text(json.into())).await;
            let _ = ws_tx.close().await;
            return;
        }
    };

    // Step 3: Validate connect params
    let connect_params: ConnectParams = match &connect_request.params {
        Some(params) => match serde_json::from_value(params.clone()) {
            Ok(p) => p,
            Err(e) => {
                let err = OcResponseFrame::error(
                    connect_request.id.clone(),
                    format!("Invalid connect params: {}", e),
                    Some(-32602),
                );
                let json = serde_json::to_string(&err).unwrap();
                let _ = ws_tx.send(Message::Text(json.into())).await;
                let _ = ws_tx.close().await;
                return;
            }
        },
        None => {
            let err = OcResponseFrame::error(
                connect_request.id.clone(),
                "Missing connect params".to_string(),
                Some(-32602),
            );
            let json = serde_json::to_string(&err).unwrap();
            let _ = ws_tx.send(Message::Text(json.into())).await;
            let _ = ws_tx.close().await;
            return;
        }
    };

    // Step 3a: Validate token auth
    let auth_result = authorize_connect_auth(
        &state.auth,
        connect_params.auth.as_ref(),
        is_local,
    );

    if !auth_result.ok {
        warn!(
            "WebSocket auth failed for {}: {:?}",
            client_id, auth_result.reason
        );
        let err = OcResponseFrame::error(
            connect_request.id.clone(),
            auth_result
                .reason
                .unwrap_or_else(|| "Authentication failed".to_string()),
            Some(1008),
        );
        let json = serde_json::to_string(&err).unwrap();
        let _ = ws_tx.send(Message::Text(json.into())).await;
        let _ = ws_tx.close().await;
        return;
    }

    // Step 3b: Resolve requested scopes
    let requested_scopes = connect_params
        .scopes
        .as_ref()
        .cloned()
        .unwrap_or_default();
    let resolved_scopes = GatewayScope::resolve_scopes(&requested_scopes);

    // Step 3c: Validate device identity (if provided)
    let client_info_id = connect_params
        .client
        .as_ref()
        .and_then(|c| c.id.as_deref())
        .unwrap_or("unknown");
    let client_mode = connect_params
        .client
        .as_ref()
        .and_then(|c| c.mode.as_deref())
        .unwrap_or("backend");
    let role = connect_params.role.as_deref().unwrap_or("operator");
    let auth_token = connect_params
        .auth
        .as_ref()
        .and_then(|a| a.token.as_deref());

    if let Some(ref device) = connect_params.device {
        let verify_result = verify_device_identity(
            device,
            client_info_id,
            client_mode,
            role,
            &requested_scopes,
            auth_token,
            &challenge_nonce,
        );

        if verify_result.valid {
            info!(
                "Device identity verified for {} (device={})",
                client_id,
                verify_result.device_id.as_deref().unwrap_or("?")
            );
            conn_state.scopes = resolved_scopes;
        } else {
            warn!(
                "Device identity verification failed for {}: {:?}",
                client_id, verify_result.reason
            );
            // Clear scopes — connection without valid device identity gets no permissions
            conn_state.scopes = Vec::new();
        }
    } else {
        // No device params — if local, allow scopes; otherwise clear
        if is_local {
            conn_state.scopes = resolved_scopes;
        } else {
            warn!(
                "No device identity provided by non-local client {}",
                client_id
            );
            conn_state.scopes = Vec::new();
        }
    }

    conn_state.handshake_complete = true;

    // Step 4: Send success response
    let hello_ok = OcResponseFrame::success(
        connect_request.id.clone(),
        serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "server": "mylobster",
            "version": state.version,
            "sessionId": conn_state.session_id,
        }),
    );
    let hello_json = serde_json::to_string(&hello_ok).unwrap();
    if ws_tx
        .send(Message::Text(hello_json.into()))
        .await
        .is_err()
    {
        error!("Failed to send connect response to {}", client_id);
        return;
    }

    info!(
        "WebSocket client handshake complete: {} (scopes={:?})",
        client_id, conn_state.scopes
    );

    // Step 5: Set up writer channel and process messages
    let (tx, mut rx) = mpsc::channel::<String>(256);
    let seq = Arc::new(AtomicU64::new(1));

    // Track active chat runs for cancellation
    let active_runs: Arc<RwLock<HashMap<String, CancellationToken>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Config hash for CAS operations
    let config_hash: Arc<RwLock<String>> = Arc::new(RwLock::new(
        Uuid::new_v4().to_string(),
    ));

    // Spawn writer task
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
                match serde_json::from_str::<IncomingFrame>(text_str) {
                    Ok(frame) => {
                        let request = frame.into_request();
                        handle_request(
                            &state,
                            &conn_state,
                            &tx,
                            &seq,
                            &active_runs,
                            &config_hash,
                            request,
                        )
                        .await;
                    }
                    Err(e) => {
                        debug!(
                            "Failed to parse WebSocket message from {}: {}",
                            client_id, e
                        );
                        let err = OcResponseFrame::error(
                            "parse-error".to_string(),
                            format!("Parse error: {}", e),
                            Some(-32700),
                        );
                        let json = serde_json::to_string(&err).unwrap();
                        let _ = tx.send(json).await;
                    }
                }
            }
            Ok(Message::Ping(_)) => {
                // Pong is automatically handled by axum
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

    // Cancel any active runs on disconnect
    {
        let runs = active_runs.read().await;
        for (run_id, token) in runs.iter() {
            debug!("Cancelling run {} on disconnect for {}", run_id, client_id);
            token.cancel();
        }
    }

    info!("WebSocket client {} connection closed", client_id);
}

/// Handle a parsed WebSocket request (post-handshake).
/// All responses use OC format: `{type:"res", id, ok, payload/error}`
async fn handle_request(
    state: &GatewayState,
    conn: &ConnectionState,
    tx: &mpsc::Sender<String>,
    seq: &Arc<AtomicU64>,
    active_runs: &Arc<RwLock<HashMap<String, CancellationToken>>>,
    config_hash: &Arc<RwLock<String>>,
    request: RequestFrame,
) {
    debug!(
        "Handling request '{}' (id={}) from {}",
        request.method, request.id, conn.client_id
    );

    let request_id = request.id.clone();

    match request.method.as_str() {
        "chat.send" => {
            handle_chat_send(state, conn, tx, seq, active_runs, &request).await;
            // chat.send sends its own response — don't send another
            return;
        }
        "chat.cancel" => {
            let response = handle_chat_cancel(active_runs, &request).await;
            send_oc_response(tx, response).await;
        }
        "config.get" => {
            let response = handle_config_get(state, config_hash, &request).await;
            send_oc_response(tx, response).await;
        }
        "config.patch" => {
            let response = handle_config_patch(state, config_hash, &request).await;
            send_oc_response(tx, response).await;
        }
        "skills.update" => {
            let response = handle_skills_update(state, &request).await;
            send_oc_response(tx, response).await;
        }
        "sessions.list" => {
            let response = handle_sessions_list(state, &request);
            send_oc_response(tx, response).await;
        }
        "sessions.get" => {
            let response = handle_sessions_get(state, &request);
            send_oc_response(tx, response).await;
        }
        "sessions.patch" => {
            let response = handle_sessions_patch(state, &request);
            send_oc_response(tx, response).await;
        }
        "sessions.delete" => {
            let response = handle_sessions_delete(state, &request);
            send_oc_response(tx, response).await;
        }
        "tools.list" => {
            let response = handle_tools_list(state, &request).await;
            send_oc_response(tx, response).await;
        }
        "channels.status" => {
            let response = handle_channels_status(state, &request).await;
            send_oc_response(tx, response).await;
        }
        "memory.search" => {
            let response = handle_memory_search(state, &request).await;
            send_oc_response(tx, response).await;
        }
        "gateway.info" => {
            let response = handle_gateway_info(state, &request);
            send_oc_response(tx, response).await;
        }
        "config.reload" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "ok": true })),
            )
            .await;
        }
        "presence.set" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "ok": true })),
            )
            .await;
        }
        "cron.list" => {
            let response = handle_cron_list(&request);
            send_oc_response(tx, response).await;
        }
        _ => {
            send_oc_response(
                tx,
                OcResponseFrame::error(
                    request_id,
                    format!("Method not found: {}", request.method),
                    Some(-32601),
                ),
            )
            .await;
        }
    }
}

/// Send an OC-format response over the writer channel.
async fn send_oc_response(tx: &mpsc::Sender<String>, response: OcResponseFrame) {
    let json = serde_json::to_string(&response).unwrap();
    let _ = tx.send(json).await;
}

/// Send an OC-format event over the writer channel.
#[allow(dead_code)]
async fn send_oc_event(tx: &mpsc::Sender<String>, event: OcEventFrame) {
    let json = serde_json::to_string(&event).unwrap();
    let _ = tx.send(json).await;
}

// ============================================================================
// Chat Methods
// ============================================================================

async fn handle_chat_send(
    state: &GatewayState,
    conn: &ConnectionState,
    tx: &mpsc::Sender<String>,
    _seq: &Arc<AtomicU64>,
    active_runs: &Arc<RwLock<HashMap<String, CancellationToken>>>,
    request: &RequestFrame,
) {
    // Check scope
    if !conn.scopes.contains(&GatewayScope::OperatorWrite) {
        send_oc_response(
            tx,
            OcResponseFrame::error(
                request.id.clone(),
                "missing scope: operator.write".to_string(),
                Some(-32600),
            ),
        )
        .await;
        return;
    }

    let params: ChatSendParams = match serde_json::from_value(
        request.params.clone().unwrap_or(serde_json::Value::Null),
    ) {
        Ok(p) => p,
        Err(e) => {
            send_oc_response(
                tx,
                OcResponseFrame::error(
                    request.id.clone(),
                    format!("Invalid params: {}", e),
                    Some(-32602),
                ),
            )
            .await;
            return;
        }
    };

    let run_id = params
        .idempotency_key
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    // Send immediate ack with runId
    send_oc_response(
        tx,
        OcResponseFrame::success(
            request.id.clone(),
            serde_json::json!({ "runId": run_id }),
        ),
    )
    .await;

    // Set up cancellation
    let cancel_token = CancellationToken::new();
    {
        let mut runs = active_runs.write().await;
        runs.insert(run_id.clone(), cancel_token.clone());
    }

    // Clone what we need for the spawned task
    let tx = tx.clone();
    let config = state.config.clone();
    let sessions = state.sessions.clone();
    let active_runs = active_runs.clone();
    let run_id_clone = run_id.clone();

    tokio::spawn(async move {
        let config_guard = config.read().await;

        // Create event channel for chat processing
        let (event_tx, mut event_rx) = mpsc::channel::<ChatEvent>(64);

        // Spawn chat processing
        let chat_config = config_guard.clone();
        let chat_sessions = sessions.clone();
        let chat_cancel = cancel_token.clone();
        let chat_handle = tokio::spawn(async move {
            chat::process_chat(&chat_config, &chat_sessions, &params, event_tx, chat_cancel).await
        });

        // Forward chat events as OC events
        while let Some(event) = event_rx.recv().await {
            let oc_event = OcEventFrame::new(
                "chat",
                serde_json::to_value(&event).unwrap(),
            );
            let json = serde_json::to_string(&oc_event).unwrap();
            if tx.send(json).await.is_err() {
                break;
            }
        }

        // Wait for chat task to complete
        let _ = chat_handle.await;

        // Remove from active runs
        let mut runs = active_runs.write().await;
        runs.remove(&run_id_clone);
    });
}

async fn handle_chat_cancel(
    active_runs: &Arc<RwLock<HashMap<String, CancellationToken>>>,
    request: &RequestFrame,
) -> OcResponseFrame {
    let run_id = request
        .params
        .as_ref()
        .and_then(|p| p.get("runId"))
        .and_then(|v| v.as_str());

    match run_id {
        Some(run_id) => {
            let runs = active_runs.read().await;
            if let Some(token) = runs.get(run_id) {
                token.cancel();
                info!("Cancelled run {}", run_id);
                OcResponseFrame::success(request.id.clone(), serde_json::json!({ "ok": true }))
            } else {
                OcResponseFrame::error(
                    request.id.clone(),
                    format!("Run not found: {}", run_id),
                    Some(-32600),
                )
            }
        }
        None => OcResponseFrame::error(
            request.id.clone(),
            "Missing runId param".to_string(),
            Some(-32602),
        ),
    }
}

// ============================================================================
// Config Methods (P1: Bridge Feature Compatibility)
// ============================================================================

async fn handle_config_get(
    _state: &GatewayState,
    config_hash: &Arc<RwLock<String>>,
    request: &RequestFrame,
) -> OcResponseFrame {
    let hash = config_hash.read().await.clone();
    OcResponseFrame::success(
        request.id.clone(),
        serde_json::json!({
            "hash": hash,
            "exists": true,
        }),
    )
}

async fn handle_config_patch(
    state: &GatewayState,
    config_hash: &Arc<RwLock<String>>,
    request: &RequestFrame,
) -> OcResponseFrame {
    let params = match request.params.as_ref() {
        Some(p) => p,
        None => {
            return OcResponseFrame::error(
                request.id.clone(),
                "Missing params".to_string(),
                Some(-32602),
            )
        }
    };

    // Parse raw JSON patch
    let raw = params.get("raw").and_then(|v| v.as_str());
    let _base_hash = params.get("baseHash").and_then(|v| v.as_str());

    if let Some(raw_str) = raw {
        // Parse the JSON patch
        if let Ok(patch) = serde_json::from_str::<serde_json::Value>(raw_str) {
            // Apply patch to config (deep merge)
            let mut config = state.config.write().await;
            // For now we apply known fields from the patch
            apply_config_patch(&mut config, &patch);

            // Update hash
            let new_hash = Uuid::new_v4().to_string();
            *config_hash.write().await = new_hash;
        }
    }

    OcResponseFrame::success(request.id.clone(), serde_json::json!({ "ok": true }))
}

/// Apply a JSON config patch to the running config.
/// This handles the settings the bridge's openclaw-config-client syncs.
fn apply_config_patch(config: &mut crate::config::Config, patch: &serde_json::Value) {
    // agents.defaults.model.primary
    if let Some(model) = patch
        .pointer("/agents/defaults/model/primary")
        .and_then(|v| v.as_str())
    {
        config.agent.model = crate::config::AgentModelConfig::Simple(model.to_string());
    }

    // models.providers.anthropic.apiKey
    if let Some(key) = patch
        .pointer("/models/providers/anthropic/apiKey")
        .and_then(|v| v.as_str())
    {
        config.models.apply_anthropic_key(key);
    }

    // models.providers.openai.apiKey
    if let Some(key) = patch
        .pointer("/models/providers/openai/apiKey")
        .and_then(|v| v.as_str())
    {
        config.models.apply_openai_key(key);
    }

    // models.providers.google.apiKey — use apply method if available, otherwise direct
    if let Some(key) = patch
        .pointer("/models/providers/google/apiKey")
        .and_then(|v| v.as_str())
    {
        config
            .models
            .providers
            .entry("google".to_string())
            .and_modify(|p| p.api_key = Some(key.to_string()));
    }

    // channels.telegram.botToken
    if let Some(token) = patch
        .pointer("/channels/telegram/botToken")
        .and_then(|v| v.as_str())
    {
        config.channels.telegram.apply_token(token);
    }

    // channels.discord.token
    if let Some(token) = patch
        .pointer("/channels/discord/token")
        .and_then(|v| v.as_str())
    {
        config.channels.discord.apply_token(token);
    }

    // channels.slack.botToken
    if let Some(token) = patch
        .pointer("/channels/slack/botToken")
        .and_then(|v| v.as_str())
    {
        config.channels.slack.apply_bot_token(token);
    }

    info!("Config patch applied");
}

async fn handle_skills_update(
    _state: &GatewayState,
    request: &RequestFrame,
) -> OcResponseFrame {
    // Accept the update but don't do anything complex for now
    info!("Skills update received");
    OcResponseFrame::success(request.id.clone(), serde_json::json!({ "ok": true }))
}

// ============================================================================
// Session Methods
// ============================================================================

fn handle_sessions_list(
    state: &GatewayState,
    request: &RequestFrame,
) -> OcResponseFrame {
    let sessions = state.sessions.list_sessions();
    OcResponseFrame::success(
        request.id.clone(),
        serde_json::to_value(sessions).unwrap(),
    )
}

fn handle_sessions_get(
    state: &GatewayState,
    request: &RequestFrame,
) -> OcResponseFrame {
    let session_key = request
        .params
        .as_ref()
        .and_then(|p| p.get("sessionKey"))
        .and_then(|v| v.as_str());

    match session_key {
        Some(key) => match state.sessions.get_session(key) {
            Some(s) => {
                OcResponseFrame::success(request.id.clone(), serde_json::to_value(s).unwrap())
            }
            None => OcResponseFrame::error(
                request.id.clone(),
                "Session not found".to_string(),
                Some(-32600),
            ),
        },
        None => OcResponseFrame::error(
            request.id.clone(),
            "Missing sessionKey".to_string(),
            Some(-32602),
        ),
    }
}

fn handle_sessions_patch(
    state: &GatewayState,
    request: &RequestFrame,
) -> OcResponseFrame {
    match serde_json::from_value::<SessionPatchParams>(
        request.params.clone().unwrap_or(serde_json::Value::Null),
    ) {
        Ok(params) => {
            state.sessions.patch_session(&params);
            OcResponseFrame::success(request.id.clone(), serde_json::json!({ "ok": true }))
        }
        Err(e) => OcResponseFrame::error(
            request.id.clone(),
            format!("Invalid params: {}", e),
            Some(-32602),
        ),
    }
}

fn handle_sessions_delete(
    state: &GatewayState,
    request: &RequestFrame,
) -> OcResponseFrame {
    let session_key = request
        .params
        .as_ref()
        .and_then(|p| p.get("sessionKey"))
        .and_then(|v| v.as_str());

    match session_key {
        Some(key) => {
            state.sessions.delete_session(key);
            OcResponseFrame::success(request.id.clone(), serde_json::json!({ "ok": true }))
        }
        None => OcResponseFrame::error(
            request.id.clone(),
            "Missing sessionKey".to_string(),
            Some(-32602),
        ),
    }
}

// ============================================================================
// Other Methods
// ============================================================================

async fn handle_tools_list(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let config = state.config.read().await;
    let tools = crate::agents::tools::list_available_tools(&config);
    OcResponseFrame::success(request.id.clone(), serde_json::to_value(tools).unwrap())
}

async fn handle_channels_status(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let status = state.channels.get_status().await;
    OcResponseFrame::success(request.id.clone(), status)
}

async fn handle_memory_search(
    state: &GatewayState,
    request: &RequestFrame,
) -> OcResponseFrame {
    let params = match request.params.as_ref() {
        Some(p) => p,
        None => {
            return OcResponseFrame::error(
                request.id.clone(),
                "Missing params".to_string(),
                Some(-32602),
            )
        }
    };

    let query = match params.get("query").and_then(|v| v.as_str()) {
        Some(q) => q,
        None => {
            return OcResponseFrame::error(
                request.id.clone(),
                "Missing query".to_string(),
                Some(-32602),
            )
        }
    };

    let max_results = params
        .get("maxResults")
        .and_then(|v| v.as_u64())
        .unwrap_or(10) as u32;

    let config = state.config.read().await;
    match crate::memory::search(&config, query, max_results, 0.0, None).await {
        Ok(results) => OcResponseFrame::success(
            request.id.clone(),
            serde_json::to_value(results).unwrap(),
        ),
        Err(e) => OcResponseFrame::error(
            request.id.clone(),
            format!("Memory search failed: {}", e),
            Some(-32603),
        ),
    }
}

fn handle_gateway_info(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let uptime = state.start_time.elapsed().as_secs();
    OcResponseFrame::success(
        request.id.clone(),
        serde_json::json!({
            "version": state.version,
            "protocol": PROTOCOL_VERSION,
            "uptimeSeconds": uptime,
            "sessionsActive": state.sessions.active_count(),
        }),
    )
}

fn handle_cron_list(request: &RequestFrame) -> OcResponseFrame {
    let jobs = crate::cron::list_jobs();
    OcResponseFrame::success(request.id.clone(), serde_json::to_value(jobs).unwrap())
}
