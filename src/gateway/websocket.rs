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
            let response = handle_config_reload(state, config_hash, &request).await;
            send_oc_response(tx, response).await;
        }
        "presence.set" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "ok": true })),
            )
            .await;
        }
        "cron.list" => {
            let response = handle_cron_list(state, &request);
            send_oc_response(tx, response).await;
        }

        // ================================================================
        // Chat extensions
        // ================================================================
        "chat.history" => {
            let response = handle_chat_history(state, &request);
            send_oc_response(tx, response).await;
        }
        "chat.abort" => {
            let response = handle_chat_cancel(active_runs, &request).await;
            send_oc_response(tx, response).await;
        }

        // ================================================================
        // Config extensions
        // ================================================================
        "config.set" => {
            let response = handle_config_set(state, config_hash, &request).await;
            send_oc_response(tx, response).await;
        }
        "config.apply" => {
            let response = handle_config_apply(state, config_hash, &request).await;
            send_oc_response(tx, response).await;
        }
        "config.schema" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({
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
                    }),
                ),
            )
            .await;
        }

        // ================================================================
        // Session extensions
        // ================================================================
        "sessions.preview" => {
            let response = handle_sessions_preview(state, &request);
            send_oc_response(tx, response).await;
        }
        "sessions.reset" => {
            let response = handle_sessions_reset(state, &request);
            send_oc_response(tx, response).await;
        }
        "sessions.compact" => {
            let response = handle_sessions_compact(state, &request);
            send_oc_response(tx, response).await;
        }
        "sessions.usage" => {
            let response = handle_sessions_usage(state, &request);
            send_oc_response(tx, response).await;
        }
        "sessions.resolve" => {
            let response = handle_sessions_resolve(state, &request);
            send_oc_response(tx, response).await;
        }

        // ================================================================
        // Agent methods
        // ================================================================
        "agent" => {
            let response = handle_agent_run(state, &request).await;
            send_oc_response(tx, response).await;
        }
        "agent.wait" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({ "ok": true, "status": "completed" }),
                ),
            )
            .await;
        }
        "agent.identity.get" => {
            let response = handle_agent_identity_get(state, &request);
            send_oc_response(tx, response).await;
        }
        "agents.list" => {
            let response = handle_agents_list(state, &request);
            send_oc_response(tx, response).await;
        }
        "agents.create" => {
            let response = handle_agents_create(state, &request);
            send_oc_response(tx, response).await;
        }
        "agents.update" => {
            let response = handle_agents_update(state, &request);
            send_oc_response(tx, response).await;
        }
        "agents.delete" => {
            let response = handle_agents_delete(state, &request);
            send_oc_response(tx, response).await;
        }
        "agents.files.list" => {
            let response = handle_agents_files_list(state, &request);
            send_oc_response(tx, response).await;
        }
        "agents.files.get" => {
            let response = handle_agents_files_get(state, &request);
            send_oc_response(tx, response).await;
        }
        "agents.files.set" => {
            let response = handle_agents_files_set(state, &request);
            send_oc_response(tx, response).await;
        }

        // ================================================================
        // Channel extensions
        // ================================================================
        "channels.logout" => {
            let response = handle_channels_logout(state, &request).await;
            send_oc_response(tx, response).await;
        }

        // ================================================================
        // Skills extensions
        // ================================================================
        "skills.status" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({ "installed": [], "available": [] }),
                ),
            )
            .await;
        }
        "skills.bins" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "bins": [] })),
            )
            .await;
        }
        "skills.install" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "ok": true })),
            )
            .await;
        }

        // ================================================================
        // Tools extensions
        // ================================================================
        "tools.catalog" => {
            let response = handle_tools_list(state, &request).await;
            send_oc_response(tx, response).await;
        }

        // ================================================================
        // Models
        // ================================================================
        "models.list" => {
            let response = handle_models_list(state, &request).await;
            send_oc_response(tx, response).await;
        }

        // ================================================================
        // Cron extensions
        // ================================================================
        "cron.status" => {
            let response = handle_cron_status(state, &request);
            send_oc_response(tx, response).await;
        }
        "cron.add" => {
            let response = handle_cron_add(state, &request);
            send_oc_response(tx, response).await;
        }
        "cron.update" => {
            let response = handle_cron_update(state, &request);
            send_oc_response(tx, response).await;
        }
        "cron.remove" => {
            let response = handle_cron_remove(state, &request);
            send_oc_response(tx, response).await;
        }
        "cron.run" => {
            let response = handle_cron_run(state, &request);
            send_oc_response(tx, response).await;
        }
        "cron.runs" => {
            let response = handle_cron_runs(state, &request);
            send_oc_response(tx, response).await;
        }

        // ================================================================
        // Device pairing
        // ================================================================
        "device.pair.list" => {
            // Account-scoped: filter pairs visible to requesting account
            let account_filter = request
                .params
                .as_ref()
                .and_then(|p| p.get("accountId"))
                .and_then(|v| v.as_str());
            let all_pairs = state.rpc.device_pairs.read().clone();
            let pairs: Vec<serde_json::Value> = match account_filter {
                Some(acct) => all_pairs
                    .into_iter()
                    .filter(|p| {
                        p.get("accountId")
                            .and_then(|v| v.as_str())
                            .map(|a| a == acct)
                            .unwrap_or(true) // include pairs without accountId
                    })
                    .collect(),
                None => all_pairs,
            };
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "devices": pairs })),
            )
            .await;
        }
        "device.pair.approve" => {
            let response = handle_device_pair_action(state, &request, "approved");
            send_oc_response(tx, response).await;
        }
        "device.pair.reject" => {
            let response = handle_device_pair_action(state, &request, "rejected");
            send_oc_response(tx, response).await;
        }
        "device.pair.remove" => {
            let response = handle_device_pair_remove(state, &request);
            send_oc_response(tx, response).await;
        }
        "device.token.rotate" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({ "ok": true, "token": Uuid::new_v4().to_string() }),
                ),
            )
            .await;
        }
        "device.token.revoke" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "ok": true })),
            )
            .await;
        }

        // ================================================================
        // Node pairing
        // ================================================================
        "node.pair.request" => {
            let response = handle_node_pair_request(state, &request);
            send_oc_response(tx, response).await;
        }
        "node.pair.list" => {
            let pairs = state.rpc.node_pairs.read().clone();
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "requests": pairs })),
            )
            .await;
        }
        "node.pair.approve" => {
            let response = handle_node_pair_action(state, &request, "approved");
            send_oc_response(tx, response).await;
        }
        "node.pair.reject" => {
            let response = handle_node_pair_action(state, &request, "rejected");
            send_oc_response(tx, response).await;
        }
        "node.pair.verify" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({ "ok": true, "verified": true }),
                ),
            )
            .await;
        }

        // ================================================================
        // Node operations
        // ================================================================
        "node.list" => {
            let nodes: Vec<serde_json::Value> =
                state.rpc.nodes.read().values().cloned().collect();
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "nodes": nodes })),
            )
            .await;
        }
        "node.describe" => {
            let response = handle_node_describe(state, &request);
            send_oc_response(tx, response).await;
        }
        "node.rename" => {
            let response = handle_node_rename(state, &request);
            send_oc_response(tx, response).await;
        }
        "node.invoke" => {
            let response = handle_node_invoke(state, &request);
            send_oc_response(tx, response).await;
        }
        "node.invoke.result" => {
            let response = handle_node_invoke_result(state, &request);
            send_oc_response(tx, response).await;
        }
        "node.event" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "ok": true })),
            )
            .await;
        }

        // ================================================================
        // System events
        // ================================================================
        "system-event" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "ok": true })),
            )
            .await;
        }
        "send" => {
            let response = handle_send(state, &request).await;
            send_oc_response(tx, response).await;
        }

        // ================================================================
        // Health & Status
        // ================================================================
        "health" => {
            let uptime = state.start_time.elapsed().as_secs();
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({
                        "status": "ok",
                        "version": state.version,
                        "uptime": uptime,
                    }),
                ),
            )
            .await;
        }
        "status" => {
            let response = handle_status(state, &request).await;
            send_oc_response(tx, response).await;
        }
        "doctor.memory.status" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({
                        "status": "ok",
                        "backend": "sqlite-fts5",
                        "indexedDocuments": 0,
                        "embeddingProvider": null,
                    }),
                ),
            )
            .await;
        }
        "logs.tail" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({ "ok": true, "logs": [] }),
                ),
            )
            .await;
        }
        "last-heartbeat" => {
            let ts = state.rpc.last_heartbeat_ms.read().unwrap_or(0);
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "ts": ts })),
            )
            .await;
        }
        "set-heartbeats" => {
            if let Some(mode) = request
                .params
                .as_ref()
                .and_then(|p| p.get("mode"))
                .and_then(|v| v.as_str())
            {
                *state.rpc.heartbeat_mode.write() = mode.to_string();
            }
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "ok": true })),
            )
            .await;
        }
        "system-presence" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({ "presence": "active" }),
                ),
            )
            .await;
        }

        // ================================================================
        // TTS
        // ================================================================
        "tts.status" => {
            let enabled = *state.rpc.tts_enabled.read();
            let provider = state.rpc.tts_provider.read().clone();
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({
                        "enabled": enabled,
                        "provider": provider,
                        "available": true,
                    }),
                ),
            )
            .await;
        }
        "tts.providers" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({
                        "providers": [
                            { "id": "openai", "name": "OpenAI TTS", "available": true },
                            { "id": "elevenlabs", "name": "ElevenLabs", "available": false },
                            { "id": "system", "name": "System TTS", "available": true },
                        ]
                    }),
                ),
            )
            .await;
        }
        "tts.enable" => {
            *state.rpc.tts_enabled.write() = true;
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "ok": true })),
            )
            .await;
        }
        "tts.disable" => {
            *state.rpc.tts_enabled.write() = false;
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "ok": true })),
            )
            .await;
        }
        "tts.convert" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({ "ok": true, "audio": null }),
                ),
            )
            .await;
        }
        "tts.setProvider" => {
            if let Some(provider) = request
                .params
                .as_ref()
                .and_then(|p| p.get("provider"))
                .and_then(|v| v.as_str())
            {
                *state.rpc.tts_provider.write() = Some(provider.to_string());
            }
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "ok": true })),
            )
            .await;
        }

        // ================================================================
        // Voice wake
        // ================================================================
        "voicewake.get" => {
            let triggers = state.rpc.voice_wake_triggers.read().clone();
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({ "triggers": triggers }),
                ),
            )
            .await;
        }
        "voicewake.set" => {
            if let Some(triggers) = request
                .params
                .as_ref()
                .and_then(|p| p.get("triggers"))
                .and_then(|v| v.as_array())
            {
                let new_triggers: Vec<String> = triggers
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
                *state.rpc.voice_wake_triggers.write() = new_triggers;
            }
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "ok": true })),
            )
            .await;
        }

        // ================================================================
        // Exec approvals
        // ================================================================
        "exec.approvals.get" => {
            let policies = state.rpc.exec_policies.read().clone();
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({ "policies": policies }),
                ),
            )
            .await;
        }
        "exec.approvals.set" => {
            if let Some(policies) = request
                .params
                .as_ref()
                .and_then(|p| p.get("policies"))
                .and_then(|v| v.as_array())
            {
                *state.rpc.exec_policies.write() = policies.clone();
            }
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "ok": true })),
            )
            .await;
        }
        "exec.approvals.node.get" => {
            let policies = state.rpc.exec_node_policies.read().clone();
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({ "policies": policies }),
                ),
            )
            .await;
        }
        "exec.approvals.node.set" => {
            if let Some(policies) = request
                .params
                .as_ref()
                .and_then(|p| p.get("policies"))
                .and_then(|v| v.as_array())
            {
                *state.rpc.exec_node_policies.write() = policies.clone();
            }
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "ok": true })),
            )
            .await;
        }
        "exec.approval.request" => {
            let response = handle_exec_approval_request(state, &request);
            send_oc_response(tx, response).await;
        }
        "exec.approval.resolve" => {
            let response = handle_exec_approval_resolve(state, &request);
            send_oc_response(tx, response).await;
        }
        "exec.approval.waitDecision" => {
            // In a real implementation this would block until a decision is made.
            // For now, auto-approve.
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({ "decision": "approved" }),
                ),
            )
            .await;
        }

        // ================================================================
        // Wizard
        // ================================================================
        "wizard.start" => {
            *state.rpc.wizard_active.write() = true;
            *state.rpc.wizard_step.write() = 1;
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({
                        "ok": true,
                        "step": 1,
                        "totalSteps": 5,
                    }),
                ),
            )
            .await;
        }
        "wizard.next" => {
            let (current, done) = {
                let mut step = state.rpc.wizard_step.write();
                *step = step.saturating_add(1);
                let c = *step;
                let d = c > 5;
                if d {
                    *state.rpc.wizard_active.write() = false;
                }
                (c, d)
            };
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({
                        "ok": true,
                        "step": current,
                        "totalSteps": 5,
                        "done": done,
                    }),
                ),
            )
            .await;
        }
        "wizard.cancel" => {
            {
                *state.rpc.wizard_active.write() = false;
                *state.rpc.wizard_step.write() = 0;
            }
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "ok": true })),
            )
            .await;
        }
        "wizard.status" => {
            let active = *state.rpc.wizard_active.read();
            let step = *state.rpc.wizard_step.read();
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({
                        "active": active,
                        "currentStep": step,
                        "totalSteps": 5,
                    }),
                ),
            )
            .await;
        }

        // ================================================================
        // Web login
        // ================================================================
        "web.login.start" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({ "url": null, "token": null }),
                ),
            )
            .await;
        }
        "web.login.wait" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({ "ok": false, "reason": "not_implemented" }),
                ),
            )
            .await;
        }

        // ================================================================
        // Updates & System
        // ================================================================
        "update.run" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({ "ok": true, "version": state.version }),
                ),
            )
            .await;
        }
        "wake" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "ok": true })),
            )
            .await;
        }
        "browser.request" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({ "ok": true, "result": null }),
                ),
            )
            .await;
        }

        // ================================================================
        // Usage
        // ================================================================
        "usage.status" => {
            let input = *state.rpc.usage_input_tokens.read();
            let output = *state.rpc.usage_output_tokens.read();
            let requests = *state.rpc.usage_requests.read();
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({
                        "totalInputTokens": input,
                        "totalOutputTokens": output,
                        "totalRequests": requests,
                    }),
                ),
            )
            .await;
        }
        "usage.cost" => {
            let input = *state.rpc.usage_input_tokens.read();
            let output = *state.rpc.usage_output_tokens.read();
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({
                        "totalInputTokens": input,
                        "totalOutputTokens": output,
                        "estimatedCostUsd": 0.0,
                    }),
                ),
            )
            .await;
        }

        // ================================================================
        // Talk
        // ================================================================
        "talk.config" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({ "mode": "text", "provider": null }),
                ),
            )
            .await;
        }
        "talk.mode" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(request_id, serde_json::json!({ "ok": true })),
            )
            .await;
        }

        // ================================================================
        // Push
        // ================================================================
        "push.test" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({ "ok": true, "delivered": false }),
                ),
            )
            .await;
        }

        // ================================================================
        // Secrets management (v2026.2.26)
        // ================================================================
        "secrets.reload" => {
            let response = handle_secrets_reload(state, &request).await;
            send_oc_response(tx, response).await;
        }

        // ================================================================
        // ACP agents (v2026.2.26)
        // ================================================================
        "acp.spawn" => {
            let response = handle_acp_spawn(state, &request).await;
            send_oc_response(tx, response).await;
        }
        "acp.send" => {
            let response = handle_acp_send(state, &request).await;
            send_oc_response(tx, response).await;
        }
        "acp.stop" => {
            let response = handle_acp_stop(state, &request).await;
            send_oc_response(tx, response).await;
        }
        "acp.list" => {
            let response = handle_acp_list(state, &request).await;
            send_oc_response(tx, response).await;
        }

        // ================================================================
        // Agent routing bindings (v2026.2.26)
        // ================================================================
        "agents.bindings" => {
            let response = handle_agents_bindings(state, &request).await;
            send_oc_response(tx, response).await;
        }
        "agents.bind" => {
            let response = handle_agents_bind(state, &request).await;
            send_oc_response(tx, response).await;
        }
        "agents.unbind" => {
            let response = handle_agents_unbind(state, &request).await;
            send_oc_response(tx, response).await;
        }

        // ================================================================
        // Device status & info (v2026.2.26)
        // ================================================================
        "device.status" => {
            let uptime = state.start_time.elapsed().as_secs();
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({
                        "ok": true,
                        "uptime": uptime,
                        "status": "online",
                        "version": state.version,
                    }),
                ),
            )
            .await;
        }
        "device.info" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({
                        "platform": std::env::consts::OS,
                        "arch": std::env::consts::ARCH,
                        "version": state.version,
                        "runtime": "rust",
                    }),
                ),
            )
            .await;
        }

        // ================================================================
        // Notifications (v2026.2.26)
        // ================================================================
        "notifications.list" => {
            send_oc_response(
                tx,
                OcResponseFrame::success(
                    request_id,
                    serde_json::json!({ "notifications": [] }),
                ),
            )
            .await;
        }

        // ================================================================
        // Unknown method
        // ================================================================
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

fn handle_cron_list(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let jobs: Vec<serde_json::Value> = state.rpc.cron_jobs.read().values().cloned().collect();
    OcResponseFrame::success(request.id.clone(), serde_json::json!({ "jobs": jobs }))
}

// ============================================================================
// Chat Extensions
// ============================================================================

fn handle_chat_history(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let session_key = request
        .params
        .as_ref()
        .and_then(|p| p.get("sessionKey"))
        .and_then(|v| v.as_str());

    match session_key {
        Some(key) => {
            if let Some(handle) = state.sessions.get_session_handle(key) {
                let history = handle.get_history();
                let messages: Vec<serde_json::Value> = history
                    .iter()
                    .map(|m| serde_json::to_value(m).unwrap_or(serde_json::json!({})))
                    .collect();
                OcResponseFrame::success(
                    request.id.clone(),
                    serde_json::json!({ "messages": messages }),
                )
            } else {
                OcResponseFrame::success(
                    request.id.clone(),
                    serde_json::json!({ "messages": [] }),
                )
            }
        }
        None => OcResponseFrame::error(
            request.id.clone(),
            "Missing sessionKey".to_string(),
            Some(-32602),
        ),
    }
}

// ============================================================================
// Config Extensions
// ============================================================================

async fn handle_config_set(
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

    let key = params.get("key").and_then(|v| v.as_str());
    let value = params.get("value");

    if let (Some(key), Some(value)) = (key, value) {
        // Apply as a patch using JSON pointer
        let patch = build_patch_from_key(key, value.clone());
        let mut config = state.config.write().await;
        apply_config_patch(&mut config, &patch);
        *config_hash.write().await = Uuid::new_v4().to_string();
    }

    OcResponseFrame::success(request.id.clone(), serde_json::json!({ "ok": true }))
}

async fn handle_config_apply(
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

    if let Some(config_data) = params.get("config") {
        let mut config = state.config.write().await;
        apply_config_patch(&mut config, config_data);
        *config_hash.write().await = Uuid::new_v4().to_string();
    }

    OcResponseFrame::success(request.id.clone(), serde_json::json!({ "ok": true }))
}

/// Build a JSON patch value from a dot-separated key path.
fn build_patch_from_key(key: &str, value: serde_json::Value) -> serde_json::Value {
    let parts: Vec<&str> = key.split('.').collect();
    let mut result = value;
    for part in parts.iter().rev() {
        let mut obj = serde_json::Map::new();
        obj.insert((*part).to_string(), result);
        result = serde_json::Value::Object(obj);
    }
    result
}

// ============================================================================
// Config Reload with Secrets Sync (v2026.2.26)
// ============================================================================

/// Three-phase config reload: prepare → activate → apply.
///
/// 1. Prepare: read config from disk, validate, resolve secrets
/// 2. Activate: swap the in-memory config with the new one
/// 3. Apply: update config hash, report results
///
/// If secrets resolution is enabled and required secrets fail to resolve,
/// the reload is rolled back and the old config remains active.
async fn handle_config_reload(
    state: &GatewayState,
    config_hash: &Arc<RwLock<String>>,
    request: &RequestFrame,
) -> OcResponseFrame {
    let resolve_secrets = request
        .params
        .as_ref()
        .and_then(|p| p.get("resolveSecrets"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let fail_on_missing = request
        .params
        .as_ref()
        .and_then(|p| p.get("failOnMissing"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let base_dir = request
        .params
        .as_ref()
        .and_then(|p| p.get("baseDir"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let config_path = request
        .params
        .as_ref()
        .and_then(|p| p.get("configPath"))
        .and_then(|v| v.as_str())
        .map(String::from);

    // Phase 1: Prepare — read config from disk or use current
    let new_config_value = if let Some(ref path) = config_path {
        let p = std::path::Path::new(path);
        match crate::config::read_config_file_snapshot(p) {
            Ok(v) => v,
            Err(e) => {
                return OcResponseFrame::error(
                    request.id.clone(),
                    format!("Failed to read config: {}", e),
                    Some(-32603),
                );
            }
        }
    } else {
        let config = state.config.read().await;
        serde_json::to_value(&*config).unwrap_or_default()
    };

    // Optionally resolve secrets
    let final_config_value = if resolve_secrets {
        let required_paths: Vec<&str> = request
            .params
            .as_ref()
            .and_then(|p| p.get("requiredPaths"))
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        let (resolved, result) = crate::infra::secrets::reload::reload_secrets(
            &new_config_value,
            base_dir,
            &required_paths,
            fail_on_missing,
        )
        .await;

        if !result.ok {
            return OcResponseFrame::error(
                request.id.clone(),
                format!(
                    "Config reload failed: secrets resolution error — {} unresolved",
                    result.failed_count
                ),
                Some(-32603),
            );
        }

        resolved.unwrap_or(new_config_value)
    } else {
        new_config_value
    };

    // Phase 2: Activate — apply the new config
    {
        let mut config = state.config.write().await;
        apply_config_patch(&mut config, &final_config_value);
    }

    // Phase 3: Apply — update hash
    let new_hash = Uuid::new_v4().to_string();
    *config_hash.write().await = new_hash.clone();

    OcResponseFrame::success(
        request.id.clone(),
        serde_json::json!({
            "ok": true,
            "hash": new_hash,
            "secretsResolved": resolve_secrets,
        }),
    )
}

// ============================================================================
// Session Extensions
// ============================================================================

fn handle_sessions_preview(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let previews = state.sessions.preview_sessions();
    OcResponseFrame::success(request.id.clone(), serde_json::json!({ "sessions": previews }))
}

fn handle_sessions_reset(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let session_key = request
        .params
        .as_ref()
        .and_then(|p| p.get("sessionKey"))
        .and_then(|v| v.as_str());

    match session_key {
        Some(key) => {
            let ok = state.sessions.reset_session(key);
            if ok {
                OcResponseFrame::success(request.id.clone(), serde_json::json!({ "ok": true }))
            } else {
                OcResponseFrame::error(
                    request.id.clone(),
                    "Session not found".to_string(),
                    Some(-32600),
                )
            }
        }
        None => OcResponseFrame::error(
            request.id.clone(),
            "Missing sessionKey".to_string(),
            Some(-32602),
        ),
    }
}

fn handle_sessions_compact(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let session_key = request
        .params
        .as_ref()
        .and_then(|p| p.get("sessionKey"))
        .and_then(|v| v.as_str());

    match session_key {
        Some(key) => {
            let ok = state.sessions.compact_session(key);
            OcResponseFrame::success(
                request.id.clone(),
                serde_json::json!({ "ok": ok, "compacted": ok }),
            )
        }
        None => OcResponseFrame::error(
            request.id.clone(),
            "Missing sessionKey".to_string(),
            Some(-32602),
        ),
    }
}

fn handle_sessions_usage(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let session_key = request
        .params
        .as_ref()
        .and_then(|p| p.get("sessionKey"))
        .and_then(|v| v.as_str());

    match session_key {
        Some(key) => match state.sessions.get_session_usage(key) {
            Some(usage) => OcResponseFrame::success(request.id.clone(), usage),
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

fn handle_sessions_resolve(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let reference = request
        .params
        .as_ref()
        .and_then(|p| p.get("ref"))
        .or_else(|| request.params.as_ref().and_then(|p| p.get("sessionKey")))
        .and_then(|v| v.as_str());

    match reference {
        Some(r) => match state.sessions.resolve_session(r) {
            Some(key) => OcResponseFrame::success(
                request.id.clone(),
                serde_json::json!({ "sessionKey": key }),
            ),
            None => OcResponseFrame::error(
                request.id.clone(),
                "Session not found".to_string(),
                Some(-32600),
            ),
        },
        None => OcResponseFrame::error(
            request.id.clone(),
            "Missing ref param".to_string(),
            Some(-32602),
        ),
    }
}

// ============================================================================
// Agent Methods
// ============================================================================

async fn handle_agent_run(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let params = request.params.as_ref();
    let message = params
        .and_then(|p| p.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let config = state.config.read().await;
    match crate::agents::run_single_message(&config, message, None).await {
        Ok(()) => {
            OcResponseFrame::success(request.id.clone(), serde_json::json!({ "ok": true }))
        }
        Err(e) => OcResponseFrame::error(
            request.id.clone(),
            format!("Agent error: {}", e),
            Some(-32603),
        ),
    }
}

fn handle_agent_identity_get(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    OcResponseFrame::success(
        request.id.clone(),
        serde_json::json!({
            "id": "default",
            "name": "MyLobster",
            "version": state.version,
            "server": "mylobster",
        }),
    )
}

fn handle_agents_list(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let agents: Vec<serde_json::Value> = state.rpc.agents.read().values().cloned().collect();
    // Always include the default agent
    let mut result = vec![serde_json::json!({
        "id": "default",
        "name": "Default Agent",
        "model": null,
        "createdAt": chrono::Utc::now().to_rfc3339(),
    })];
    result.extend(agents);
    OcResponseFrame::success(request.id.clone(), serde_json::json!({ "agents": result }))
}

fn handle_agents_create(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
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

    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("Unnamed Agent");

    let agent = serde_json::json!({
        "id": id,
        "name": name,
        "model": params.get("model"),
        "description": params.get("description"),
        "systemPrompt": params.get("systemPrompt"),
        "createdAt": now,
        "updatedAt": now,
    });

    state.rpc.agents.write().insert(id.clone(), agent.clone());
    OcResponseFrame::success(request.id.clone(), agent)
}

fn handle_agents_update(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
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

    let agent_id = match params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => {
            return OcResponseFrame::error(
                request.id.clone(),
                "Missing agent id".to_string(),
                Some(-32602),
            )
        }
    };

    let mut agents = state.rpc.agents.write();
    if let Some(agent) = agents.get_mut(agent_id) {
        if let Some(obj) = agent.as_object_mut() {
            if let Some(name) = params.get("name") {
                obj.insert("name".to_string(), name.clone());
            }
            if let Some(model) = params.get("model") {
                obj.insert("model".to_string(), model.clone());
            }
            if let Some(desc) = params.get("description") {
                obj.insert("description".to_string(), desc.clone());
            }
            if let Some(prompt) = params.get("systemPrompt") {
                obj.insert("systemPrompt".to_string(), prompt.clone());
            }
            obj.insert(
                "updatedAt".to_string(),
                serde_json::json!(chrono::Utc::now().to_rfc3339()),
            );
        }
        OcResponseFrame::success(request.id.clone(), agent.clone())
    } else {
        OcResponseFrame::error(
            request.id.clone(),
            format!("Agent not found: {}", agent_id),
            Some(-32600),
        )
    }
}

fn handle_agents_delete(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let agent_id = request
        .params
        .as_ref()
        .and_then(|p| p.get("id"))
        .and_then(|v| v.as_str());

    match agent_id {
        Some(id) => {
            let removed = state.rpc.agents.write().remove(id).is_some();
            if removed {
                OcResponseFrame::success(request.id.clone(), serde_json::json!({ "ok": true }))
            } else {
                OcResponseFrame::error(
                    request.id.clone(),
                    format!("Agent not found: {}", id),
                    Some(-32600),
                )
            }
        }
        None => OcResponseFrame::error(
            request.id.clone(),
            "Missing agent id".to_string(),
            Some(-32602),
        ),
    }
}

fn handle_agents_files_list(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let agent_id = request
        .params
        .as_ref()
        .and_then(|p| p.get("agentId"))
        .and_then(|v| v.as_str())
        .unwrap_or("default");

    let files = state.rpc.agent_files.read();
    let agent_files = files.get(agent_id);
    let file_list: Vec<String> = agent_files
        .map(|f| f.keys().cloned().collect())
        .unwrap_or_default();

    OcResponseFrame::success(request.id.clone(), serde_json::json!({ "files": file_list }))
}

fn handle_agents_files_get(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
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

    let agent_id = params
        .get("agentId")
        .and_then(|v| v.as_str())
        .unwrap_or("default");
    let filename = match params.get("filename").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return OcResponseFrame::error(
                request.id.clone(),
                "Missing filename".to_string(),
                Some(-32602),
            )
        }
    };

    let files = state.rpc.agent_files.read();
    if let Some(agent_files) = files.get(agent_id) {
        if let Some(content) = agent_files.get(filename) {
            return OcResponseFrame::success(
                request.id.clone(),
                serde_json::json!({ "filename": filename, "content": content }),
            );
        }
    }

    OcResponseFrame::error(
        request.id.clone(),
        format!("File not found: {}", filename),
        Some(-32600),
    )
}

fn handle_agents_files_set(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
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

    let agent_id = params
        .get("agentId")
        .and_then(|v| v.as_str())
        .unwrap_or("default");
    let filename = match params.get("filename").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return OcResponseFrame::error(
                request.id.clone(),
                "Missing filename".to_string(),
                Some(-32602),
            )
        }
    };
    let content = params
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    state
        .rpc
        .agent_files
        .write()
        .entry(agent_id.to_string())
        .or_default()
        .insert(filename.to_string(), content.to_string());

    OcResponseFrame::success(request.id.clone(), serde_json::json!({ "ok": true }))
}

// ============================================================================
// Channel Extensions
// ============================================================================

async fn handle_channels_logout(
    state: &GatewayState,
    request: &RequestFrame,
) -> OcResponseFrame {
    let channel = request
        .params
        .as_ref()
        .and_then(|p| p.get("channel"))
        .and_then(|v| v.as_str());

    match channel {
        Some(ch) => {
            if let Some(plugin) = state.channels.get_plugin(ch).await {
                let _ = plugin.stop_account().await;
                info!("Channel '{}' logged out", ch);
            }
            OcResponseFrame::success(request.id.clone(), serde_json::json!({ "ok": true }))
        }
        None => OcResponseFrame::error(
            request.id.clone(),
            "Missing channel param".to_string(),
            Some(-32602),
        ),
    }
}

// ============================================================================
// Models
// ============================================================================

async fn handle_models_list(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let config = state.config.read().await;
    let mut models = Vec::new();

    // List models from configured providers
    for (provider, provider_config) in &config.models.providers {
        if provider_config.api_key.is_some() || provider == "ollama" {
            let provider_models = get_provider_models(provider);
            models.extend(provider_models);
        }
    }

    // If no providers configured, include defaults
    if models.is_empty() {
        models = get_provider_models("anthropic");
    }

    OcResponseFrame::success(request.id.clone(), serde_json::json!({ "models": models }))
}

pub fn get_provider_models(provider: &str) -> Vec<serde_json::Value> {
    match provider {
        "anthropic" => vec![
            serde_json::json!({ "id": "claude-opus-4-6", "provider": "anthropic", "name": "Claude Opus 4.6" }),
            serde_json::json!({ "id": "claude-sonnet-4-6", "provider": "anthropic", "name": "Claude Sonnet 4.6" }),
            serde_json::json!({ "id": "claude-haiku-4-5-20251001", "provider": "anthropic", "name": "Claude Haiku 4.5" }),
        ],
        "openai" => vec![
            serde_json::json!({ "id": "gpt-4o", "provider": "openai", "name": "GPT-4o" }),
            serde_json::json!({ "id": "gpt-4o-mini", "provider": "openai", "name": "GPT-4o mini" }),
            serde_json::json!({ "id": "o1", "provider": "openai", "name": "o1" }),
            serde_json::json!({ "id": "o3-mini", "provider": "openai", "name": "o3-mini" }),
        ],
        "google" => vec![
            serde_json::json!({ "id": "gemini-2.0-flash", "provider": "google", "name": "Gemini 2.0 Flash" }),
            serde_json::json!({ "id": "gemini-2.0-pro", "provider": "google", "name": "Gemini 2.0 Pro" }),
        ],
        "groq" => vec![
            serde_json::json!({ "id": "llama-3.3-70b-versatile", "provider": "groq", "name": "Llama 3.3 70B" }),
            serde_json::json!({ "id": "mixtral-8x7b-32768", "provider": "groq", "name": "Mixtral 8x7B" }),
        ],
        "ollama" => vec![
            serde_json::json!({ "id": "llama3.3:latest", "provider": "ollama", "name": "Llama 3.3 (local)" }),
        ],
        "mistral" => vec![
            serde_json::json!({ "id": "mistral-large-latest", "provider": "mistral", "name": "Mistral Large" }),
            serde_json::json!({ "id": "codestral-latest", "provider": "mistral", "name": "Codestral" }),
        ],
        _ => vec![],
    }
}

// ============================================================================
// Cron Extensions
// ============================================================================

fn handle_cron_status(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let job_count = state.rpc.cron_jobs.read().len();
    OcResponseFrame::success(
        request.id.clone(),
        serde_json::json!({
            "running": true,
            "jobCount": job_count,
        }),
    )
}

fn handle_cron_add(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
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

    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let job = serde_json::json!({
        "id": id,
        "name": params.get("name").and_then(|v| v.as_str()).unwrap_or("Unnamed"),
        "schedule": params.get("schedule").and_then(|v| v.as_str()).unwrap_or("0 * * * *"),
        "prompt": params.get("prompt").and_then(|v| v.as_str()).unwrap_or(""),
        "enabled": params.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
        "sessionKey": params.get("sessionKey"),
        "lastRunAt": null,
        "nextRunAt": null,
        "createdAt": now,
        "updatedAt": now,
    });

    state.rpc.cron_jobs.write().insert(id.clone(), job.clone());
    OcResponseFrame::success(request.id.clone(), job)
}

fn handle_cron_update(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
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

    let job_id = match params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => {
            return OcResponseFrame::error(
                request.id.clone(),
                "Missing job id".to_string(),
                Some(-32602),
            )
        }
    };

    let mut jobs = state.rpc.cron_jobs.write();
    if let Some(job) = jobs.get_mut(job_id) {
        if let Some(obj) = job.as_object_mut() {
            for (key, value) in params.as_object().into_iter().flat_map(|m| m.iter()) {
                if key != "id" {
                    obj.insert(key.clone(), value.clone());
                }
            }
            obj.insert(
                "updatedAt".to_string(),
                serde_json::json!(chrono::Utc::now().to_rfc3339()),
            );
        }
        OcResponseFrame::success(request.id.clone(), job.clone())
    } else {
        OcResponseFrame::error(
            request.id.clone(),
            format!("Job not found: {}", job_id),
            Some(-32600),
        )
    }
}

fn handle_cron_remove(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let job_id = request
        .params
        .as_ref()
        .and_then(|p| p.get("id"))
        .and_then(|v| v.as_str());

    match job_id {
        Some(id) => {
            let removed = state.rpc.cron_jobs.write().remove(id).is_some();
            if removed {
                OcResponseFrame::success(request.id.clone(), serde_json::json!({ "ok": true }))
            } else {
                OcResponseFrame::error(
                    request.id.clone(),
                    format!("Job not found: {}", id),
                    Some(-32600),
                )
            }
        }
        None => OcResponseFrame::error(
            request.id.clone(),
            "Missing job id".to_string(),
            Some(-32602),
        ),
    }
}

fn handle_cron_run(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let job_id = request
        .params
        .as_ref()
        .and_then(|p| p.get("id"))
        .and_then(|v| v.as_str());

    match job_id {
        Some(id) => {
            let jobs = state.rpc.cron_jobs.read();
            if jobs.contains_key(id) {
                let run_id = Uuid::new_v4().to_string();
                let now = chrono::Utc::now().to_rfc3339();
                let run = serde_json::json!({
                    "id": run_id,
                    "jobId": id,
                    "startedAt": now,
                    "finishedAt": null,
                    "status": "triggered",
                });
                drop(jobs);
                state.rpc.cron_runs.write().push(run.clone());
                OcResponseFrame::success(request.id.clone(), run)
            } else {
                OcResponseFrame::error(
                    request.id.clone(),
                    format!("Job not found: {}", id),
                    Some(-32600),
                )
            }
        }
        None => OcResponseFrame::error(
            request.id.clone(),
            "Missing job id".to_string(),
            Some(-32602),
        ),
    }
}

fn handle_cron_runs(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let job_id = request
        .params
        .as_ref()
        .and_then(|p| p.get("id"))
        .and_then(|v| v.as_str());

    let runs = state.rpc.cron_runs.read();
    let filtered: Vec<&serde_json::Value> = match job_id {
        Some(id) => runs
            .iter()
            .filter(|r| r.get("jobId").and_then(|v| v.as_str()) == Some(id))
            .collect(),
        None => runs.iter().collect(),
    };

    OcResponseFrame::success(request.id.clone(), serde_json::json!({ "runs": filtered }))
}

// ============================================================================
// Device Pairing
// ============================================================================

fn handle_device_pair_action(
    state: &GatewayState,
    request: &RequestFrame,
    new_status: &str,
) -> OcResponseFrame {
    let params = request.params.as_ref();
    let device_id = params.and_then(|p| p.get("deviceId")).and_then(|v| v.as_str());
    let account_id = params
        .and_then(|p| p.get("accountId"))
        .and_then(|v| v.as_str());

    match device_id {
        Some(did) => {
            let mut pairs = state.rpc.device_pairs.write();
            let mut found = false;
            for pair in pairs.iter_mut() {
                if pair.get("deviceId").and_then(|v| v.as_str()) != Some(did) {
                    continue;
                }
                // Account-scoped: only modify pairs owned by this account
                if let Some(acct) = account_id {
                    let pair_acct = pair.get("accountId").and_then(|v| v.as_str());
                    if pair_acct.is_some() && pair_acct != Some(acct) {
                        continue;
                    }
                }
                if let Some(obj) = pair.as_object_mut() {
                    obj.insert("status".to_string(), serde_json::json!(new_status));
                    // Stamp accountId if not already present
                    if account_id.is_some() && !obj.contains_key("accountId") {
                        obj.insert(
                            "accountId".to_string(),
                            serde_json::json!(account_id),
                        );
                    }
                }
                found = true;
                break;
            }
            if found {
                OcResponseFrame::success(request.id.clone(), serde_json::json!({ "ok": true }))
            } else {
                OcResponseFrame::error(
                    request.id.clone(),
                    format!("Device not found: {}", did),
                    Some(-32600),
                )
            }
        }
        None => OcResponseFrame::error(
            request.id.clone(),
            "Missing deviceId".to_string(),
            Some(-32602),
        ),
    }
}

fn handle_device_pair_remove(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let params = request.params.as_ref();
    let device_id = params.and_then(|p| p.get("deviceId")).and_then(|v| v.as_str());
    let account_id = params
        .and_then(|p| p.get("accountId"))
        .and_then(|v| v.as_str());

    match device_id {
        Some(did) => {
            let mut pairs = state.rpc.device_pairs.write();
            let before = pairs.len();
            pairs.retain(|p| {
                if p.get("deviceId").and_then(|v| v.as_str()) != Some(did) {
                    return true; // keep — different device
                }
                // Account-scoped: only remove pairs owned by this account
                if let Some(acct) = account_id {
                    let pair_acct = p.get("accountId").and_then(|v| v.as_str());
                    if pair_acct.is_some() && pair_acct != Some(acct) {
                        return true; // keep — different account
                    }
                }
                false // remove
            });
            let removed = pairs.len() < before;
            OcResponseFrame::success(
                request.id.clone(),
                serde_json::json!({ "ok": true, "removed": removed }),
            )
        }
        None => OcResponseFrame::error(
            request.id.clone(),
            "Missing deviceId".to_string(),
            Some(-32602),
        ),
    }
}

// ============================================================================
// Node Pairing & Operations
// ============================================================================

fn handle_node_pair_request(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let params = request.params.as_ref();
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let pair_request = serde_json::json!({
        "id": id,
        "nodeId": params.and_then(|p| p.get("nodeId")).and_then(|v| v.as_str()).unwrap_or("unknown"),
        "name": params.and_then(|p| p.get("name")).and_then(|v| v.as_str()).unwrap_or("Unknown Node"),
        "status": "pending",
        "requestedAt": now,
    });

    state
        .rpc
        .node_pairs
        .write()
        .push(pair_request.clone());

    OcResponseFrame::success(request.id.clone(), pair_request)
}

fn handle_node_pair_action(
    state: &GatewayState,
    request: &RequestFrame,
    new_status: &str,
) -> OcResponseFrame {
    let request_id_param = request
        .params
        .as_ref()
        .and_then(|p| p.get("id"))
        .and_then(|v| v.as_str());

    match request_id_param {
        Some(rid) => {
            let mut pairs = state.rpc.node_pairs.write();
            let mut found = false;
            for pair in pairs.iter_mut() {
                if pair.get("id").and_then(|v| v.as_str()) == Some(rid) {
                    if let Some(obj) = pair.as_object_mut() {
                        obj.insert("status".to_string(), serde_json::json!(new_status));
                    }
                    found = true;
                    break;
                }
            }
            if found {
                OcResponseFrame::success(request.id.clone(), serde_json::json!({ "ok": true }))
            } else {
                OcResponseFrame::error(
                    request.id.clone(),
                    format!("Pairing request not found: {}", rid),
                    Some(-32600),
                )
            }
        }
        None => OcResponseFrame::error(
            request.id.clone(),
            "Missing request id".to_string(),
            Some(-32602),
        ),
    }
}

fn handle_node_describe(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let node_id = request
        .params
        .as_ref()
        .and_then(|p| p.get("nodeId"))
        .and_then(|v| v.as_str());

    match node_id {
        Some(nid) => {
            let nodes = state.rpc.nodes.read();
            match nodes.get(nid) {
                Some(node) => {
                    OcResponseFrame::success(request.id.clone(), node.clone())
                }
                None => OcResponseFrame::error(
                    request.id.clone(),
                    format!("Node not found: {}", nid),
                    Some(-32600),
                ),
            }
        }
        None => OcResponseFrame::error(
            request.id.clone(),
            "Missing nodeId".to_string(),
            Some(-32602),
        ),
    }
}

fn handle_node_rename(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
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

    let node_id = match params.get("nodeId").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => {
            return OcResponseFrame::error(
                request.id.clone(),
                "Missing nodeId".to_string(),
                Some(-32602),
            )
        }
    };

    let new_name = params
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("Renamed Node");

    let mut nodes = state.rpc.nodes.write();
    if let Some(node) = nodes.get_mut(node_id) {
        if let Some(obj) = node.as_object_mut() {
            obj.insert("name".to_string(), serde_json::json!(new_name));
        }
        OcResponseFrame::success(request.id.clone(), serde_json::json!({ "ok": true }))
    } else {
        OcResponseFrame::error(
            request.id.clone(),
            format!("Node not found: {}", node_id),
            Some(-32600),
        )
    }
}

fn handle_node_invoke(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let invoke_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    let result = serde_json::json!({
        "invokeId": invoke_id,
        "status": "pending",
        "submittedAt": now,
    });

    state
        .rpc
        .node_invoke_results
        .write()
        .insert(invoke_id.clone(), result.clone());

    OcResponseFrame::success(request.id.clone(), result)
}

fn handle_node_invoke_result(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let invoke_id = request
        .params
        .as_ref()
        .and_then(|p| p.get("invokeId"))
        .and_then(|v| v.as_str());

    match invoke_id {
        Some(iid) => {
            let results = state.rpc.node_invoke_results.read();
            match results.get(iid) {
                Some(result) => {
                    OcResponseFrame::success(request.id.clone(), result.clone())
                }
                None => OcResponseFrame::error(
                    request.id.clone(),
                    format!("Invoke not found: {}", iid),
                    Some(-32600),
                ),
            }
        }
        None => OcResponseFrame::error(
            request.id.clone(),
            "Missing invokeId".to_string(),
            Some(-32602),
        ),
    }
}

// ============================================================================
// Send (channel message dispatch)
// ============================================================================

async fn handle_send(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
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

    let channel = params
        .get("channel")
        .and_then(|v| v.as_str())
        .unwrap_or("telegram");
    let to = params.get("to").and_then(|v| v.as_str()).unwrap_or("");
    let message = params
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let config = state.config.read().await;
    match crate::channels::send_message(&config, channel, to, message).await {
        Ok(()) => {
            OcResponseFrame::success(request.id.clone(), serde_json::json!({ "ok": true }))
        }
        Err(e) => OcResponseFrame::error(
            request.id.clone(),
            format!("Send failed: {}", e),
            Some(-32603),
        ),
    }
}

// ============================================================================
// Status
// ============================================================================

async fn handle_status(state: &GatewayState, request: &RequestFrame) -> OcResponseFrame {
    let uptime = state.start_time.elapsed().as_secs();
    let session_count = state.sessions.active_count();
    let channel_status = state.channels.get_status().await;

    OcResponseFrame::success(
        request.id.clone(),
        serde_json::json!({
            "version": state.version,
            "uptime": uptime,
            "sessions": session_count,
            "channels": channel_status,
            "memory": {
                "status": "ok",
                "backend": "sqlite-fts5",
            },
        }),
    )
}

// ============================================================================
// Exec Approval Handlers
// ============================================================================

fn handle_exec_approval_request(
    state: &GatewayState,
    request: &RequestFrame,
) -> OcResponseFrame {
    let params = request.params.as_ref();
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    let approval_req = serde_json::json!({
        "id": id,
        "tool": params.and_then(|p| p.get("tool")).and_then(|v| v.as_str()).unwrap_or("unknown"),
        "args": params.and_then(|p| p.get("args")),
        "status": "pending",
        "requestedAt": now,
    });

    state
        .rpc
        .exec_requests
        .write()
        .insert(id.clone(), approval_req.clone());

    OcResponseFrame::success(request.id.clone(), approval_req)
}

fn handle_exec_approval_resolve(
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

    let approval_id = match params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => {
            return OcResponseFrame::error(
                request.id.clone(),
                "Missing approval id".to_string(),
                Some(-32602),
            )
        }
    };

    let decision = params
        .get("decision")
        .and_then(|v| v.as_str())
        .unwrap_or("approved");

    let mut requests = state.rpc.exec_requests.write();
    if let Some(req) = requests.get_mut(approval_id) {
        if let Some(obj) = req.as_object_mut() {
            obj.insert("status".to_string(), serde_json::json!(decision));
            obj.insert(
                "resolvedAt".to_string(),
                serde_json::json!(chrono::Utc::now().to_rfc3339()),
            );
        }
        OcResponseFrame::success(
            request.id.clone(),
            serde_json::json!({ "ok": true, "decision": decision }),
        )
    } else {
        OcResponseFrame::error(
            request.id.clone(),
            format!("Approval request not found: {}", approval_id),
            Some(-32600),
        )
    }
}

// ============================================================================
// Secrets Management (v2026.2.26)
// ============================================================================

async fn handle_secrets_reload(
    state: &GatewayState,
    request: &RequestFrame,
) -> OcResponseFrame {
    let config = state.config.read().await;
    let config_value = serde_json::to_value(&*config).unwrap_or_default();

    let base_dir = request
        .params
        .as_ref()
        .and_then(|p| p.get("baseDir"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let fail_on_missing = request
        .params
        .as_ref()
        .and_then(|p| p.get("failOnMissing"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let required_paths: Vec<&str> = request
        .params
        .as_ref()
        .and_then(|p| p.get("requiredPaths"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .collect()
        })
        .unwrap_or_default();

    let (_resolved_config, result) = crate::infra::secrets::reload::reload_secrets(
        &config_value,
        base_dir,
        &required_paths,
        fail_on_missing,
    )
    .await;

    OcResponseFrame::success(
        request.id.clone(),
        serde_json::to_value(&result).unwrap_or_default(),
    )
}

// ============================================================================
// ACP Agents (v2026.2.26)
// ============================================================================

async fn handle_acp_spawn(
    state: &GatewayState,
    request: &RequestFrame,
) -> OcResponseFrame {
    use crate::agents::acp::AcpSpawnParams;

    let params: AcpSpawnParams = match request
        .params
        .as_ref()
        .and_then(|p| serde_json::from_value(p.clone()).ok())
    {
        Some(p) => p,
        None => {
            return OcResponseFrame::error(
                request.id.clone(),
                "Invalid spawn params".to_string(),
                Some(-32602),
            );
        }
    };

    let mgr = state.rpc.acp_manager.read().await;
    let agent = mgr.spawn(params).await;
    OcResponseFrame::success(
        request.id.clone(),
        serde_json::to_value(&agent).unwrap_or_default(),
    )
}

async fn handle_acp_send(
    state: &GatewayState,
    request: &RequestFrame,
) -> OcResponseFrame {
    use crate::agents::acp::AcpSendParams;

    let params: AcpSendParams = match request
        .params
        .as_ref()
        .and_then(|p| serde_json::from_value(p.clone()).ok())
    {
        Some(p) => p,
        None => {
            return OcResponseFrame::error(
                request.id.clone(),
                "Invalid send params".to_string(),
                Some(-32602),
            );
        }
    };

    let mgr = state.rpc.acp_manager.read().await;
    let result = mgr.send(&params).await;
    OcResponseFrame::success(
        request.id.clone(),
        serde_json::to_value(&result).unwrap_or_default(),
    )
}

async fn handle_acp_stop(
    state: &GatewayState,
    request: &RequestFrame,
) -> OcResponseFrame {
    let agent_id = request
        .params
        .as_ref()
        .and_then(|p| p.get("agentId"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let mgr = state.rpc.acp_manager.read().await;
    let stopped = mgr.stop(agent_id).await;
    OcResponseFrame::success(
        request.id.clone(),
        serde_json::json!({ "ok": stopped }),
    )
}

async fn handle_acp_list(
    state: &GatewayState,
    request: &RequestFrame,
) -> OcResponseFrame {
    let mgr = state.rpc.acp_manager.read().await;
    let agents = mgr.list().await;
    OcResponseFrame::success(
        request.id.clone(),
        serde_json::json!({ "agents": serde_json::to_value(&agents).unwrap_or_default() }),
    )
}

// ============================================================================
// Agent Routing Bindings (v2026.2.26)
// ============================================================================

async fn handle_agents_bindings(
    state: &GatewayState,
    request: &RequestFrame,
) -> OcResponseFrame {
    let account_id = request
        .params
        .as_ref()
        .and_then(|p| p.get("accountId"))
        .and_then(|v| v.as_str());

    let mgr = state.rpc.route_manager.read().await;
    let routes = mgr.list(account_id).await;
    OcResponseFrame::success(
        request.id.clone(),
        serde_json::json!({ "bindings": serde_json::to_value(&routes).unwrap_or_default() }),
    )
}

async fn handle_agents_bind(
    state: &GatewayState,
    request: &RequestFrame,
) -> OcResponseFrame {
    use crate::config::AgentBindingMatch;
    use crate::routing::RouteEntry;

    let params = match request.params.as_ref() {
        Some(p) => p,
        None => {
            return OcResponseFrame::error(
                request.id.clone(),
                "Missing params".to_string(),
                Some(-32602),
            );
        }
    };

    let agent_id = params
        .get("agentId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let account_id = params
        .get("accountId")
        .and_then(|v| v.as_str())
        .map(String::from);

    let binding = AgentBindingMatch {
        channel: params.get("channel").and_then(|v| v.as_str()).map(String::from),
        account_id: account_id.clone(),
        peer: params.get("peer").and_then(|v| v.as_str()).map(String::from),
        guild_id: params.get("guildId").and_then(|v| v.as_str()).map(String::from),
        team_id: params.get("teamId").and_then(|v| v.as_str()).map(String::from),
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let entry = RouteEntry {
        agent_id,
        binding,
        account_id,
        created_at: now_ms,
    };

    let mgr = state.rpc.route_manager.read().await;
    mgr.bind(entry).await;
    OcResponseFrame::success(
        request.id.clone(),
        serde_json::json!({ "ok": true }),
    )
}

async fn handle_agents_unbind(
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
            );
        }
    };

    let agent_id = params
        .get("agentId")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let account_id = params
        .get("accountId")
        .and_then(|v| v.as_str());

    let mgr = state.rpc.route_manager.read().await;
    let removed = mgr.unbind(agent_id, account_id).await;
    OcResponseFrame::success(
        request.id.clone(),
        serde_json::json!({ "ok": true, "removed": removed }),
    )
}
