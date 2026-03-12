//! Integration tests for the MyLobster gateway WebSocket handshake.
//!
//! These tests verify full OpenClaw v2026.2.22 wire-protocol compatibility
//! by starting a real gateway server and connecting via WebSocket.

use futures::{SinkExt, StreamExt};
use serde_json::json;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, RwLock};
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use std::sync::Arc;

use mylobster::config::Config;
use mylobster::gateway::{ResolvedGatewayAuth, GatewayState, RpcState};
use mylobster::channels::ChannelManager;
use mylobster::plugins::PluginRegistry;
use mylobster::sessions::SessionStore;

/// Spin up a gateway server on an ephemeral port and return its URL.
async fn start_test_gateway(auth: ResolvedGatewayAuth) -> (String, broadcast::Sender<()>) {
    let config = Config::default();
    let (shutdown_tx, _shutdown_rx) = broadcast::channel(1);

    let state = GatewayState {
        config: Arc::new(RwLock::new(config.clone())),
        auth: Arc::new(auth),
        sessions: Arc::new(SessionStore::new(&config)),
        channels: Arc::new(ChannelManager::new(&config)),
        plugins: Arc::new(PluginRegistry::new(&config)),
        rpc: Arc::new(RpcState::new()),
        shutdown_tx: shutdown_tx.clone(),
        start_time: std::time::Instant::now(),
        version: "test".to_string(),
    };

    let app = mylobster::gateway::routes::build_routes(state);

    // Bind to port 0 to get an ephemeral port
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let shutdown = shutdown_tx.clone();
    tokio::spawn(async move {
        let mut shutdown_rx = shutdown.subscribe();
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.recv().await;
        })
        .await
        .unwrap();
    });

    // Small delay to ensure server is ready
    tokio::time::sleep(Duration::from_millis(50)).await;

    let url = format!("ws://127.0.0.1:{}/ws", addr.port());
    (url, shutdown_tx)
}

/// Helper: no-auth gateway (allows local connections without tokens).
async fn start_no_auth_gateway() -> (String, broadcast::Sender<()>) {
    let auth = ResolvedGatewayAuth {
        mode: mylobster::config::GatewayAuthMode::Token,
        token: None,
        password: None,
        allow_tailscale: false,
    };
    start_test_gateway(auth).await
}

/// Helper: token-auth gateway.
async fn start_token_auth_gateway(token: &str) -> (String, broadcast::Sender<()>) {
    let auth = ResolvedGatewayAuth {
        mode: mylobster::config::GatewayAuthMode::Token,
        token: Some(token.to_string()),
        password: None,
        allow_tailscale: false,
    };
    start_test_gateway(auth).await
}

/// Read the next text message from the WS stream with timeout.
async fn recv_text(
    stream: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> serde_json::Value {
    let msg = timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("timeout waiting for WS message")
        .expect("stream ended")
        .expect("WS error");
    match msg {
        Message::Text(text) => serde_json::from_str(&text).expect("invalid JSON"),
        other => panic!("expected Text message, got {:?}", other),
    }
}

// =========================================================================
// Tests
// =========================================================================

#[tokio::test]
async fn gateway_sends_connect_challenge_on_connect() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut _tx, mut rx) = ws.split();

    // First message must be connect.challenge
    let msg = recv_text(&mut rx).await;
    assert_eq!(msg["type"], "event");
    assert_eq!(msg["event"], "connect.challenge");
    assert!(msg["payload"]["nonce"].is_string());
    assert!(msg["payload"]["ts"].is_u64());

    let _ = shutdown.send(());
}

#[tokio::test]
async fn handshake_succeeds_local_no_auth() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Read connect.challenge
    let challenge = recv_text(&mut rx).await;
    let _nonce = challenge["payload"]["nonce"].as_str().unwrap();

    // Send connect request (no auth needed for local)
    let connect_req = json!({
        "type": "req",
        "id": "conn-1",
        "method": "connect",
        "params": {
            "auth": {}
        }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();

    // Read response
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["type"], "res");
    assert_eq!(resp["id"], "conn-1");
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["payload"]["protocol"], 4);
    assert_eq!(resp["payload"]["server"], "mylobster");
    assert!(resp["payload"]["sessionId"].is_string());
    assert!(resp["payload"]["version"].is_string());

    let _ = shutdown.send(());
}

#[tokio::test]
async fn handshake_succeeds_with_token_auth() {
    let token = "test-secret-token";
    let (url, shutdown) = start_token_auth_gateway(token).await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Read connect.challenge
    let _challenge = recv_text(&mut rx).await;

    // Send connect with correct token
    let connect_req = json!({
        "type": "req",
        "id": "auth-1",
        "method": "connect",
        "params": {
            "auth": { "token": token },
            "scopes": ["operator.admin"]
        }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();

    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["type"], "res");
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["payload"]["protocol"], 4);

    let _ = shutdown.send(());
}

#[tokio::test]
async fn handshake_fails_with_wrong_token() {
    let (url, shutdown) = start_token_auth_gateway("real-token").await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Read connect.challenge
    let _challenge = recv_text(&mut rx).await;

    // Send connect with wrong token
    let connect_req = json!({
        "type": "req",
        "id": "auth-bad",
        "method": "connect",
        "params": {
            "auth": { "token": "wrong-token" }
        }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();

    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["type"], "res");
    assert_eq!(resp["ok"], false);
    assert!(resp["error"]["message"].as_str().unwrap().contains("token"));

    let _ = shutdown.send(());
}

#[tokio::test]
async fn handshake_fails_missing_connect_params() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Read connect.challenge
    let _challenge = recv_text(&mut rx).await;

    // Send connect without params
    let connect_req = json!({
        "type": "req",
        "id": "no-params",
        "method": "connect"
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();

    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["type"], "res");
    assert_eq!(resp["ok"], false);

    let _ = shutdown.send(());
}

#[tokio::test]
async fn legacy_request_type_accepted() {
    // The bridge may send `type:"request"` (legacy) — gateway must accept both
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Read connect.challenge
    let _challenge = recv_text(&mut rx).await;

    // Send legacy-format connect request
    let connect_req = json!({
        "type": "request",
        "id": "legacy-1",
        "method": "connect",
        "params": {
            "auth": {}
        }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();

    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["type"], "res");
    assert_eq!(resp["ok"], true);

    let _ = shutdown.send(());
}

#[tokio::test]
async fn post_handshake_method_returns_oc_response_format() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    // Call sessions.list — should return OC-format response
    let list_req = json!({
        "type": "req",
        "id": "list-1",
        "method": "sessions.list"
    });
    tx.send(Message::Text(list_req.to_string().into()))
        .await
        .unwrap();

    let list_resp = recv_text(&mut rx).await;
    assert_eq!(list_resp["type"], "res");
    assert_eq!(list_resp["id"], "list-1");
    assert_eq!(list_resp["ok"], true);
    // payload should be an array (possibly empty)
    assert!(list_resp["payload"].is_array());

    let _ = shutdown.send(());
}

#[tokio::test]
async fn gateway_info_returns_oc_response_format() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    // Call gateway.info
    let info_req = json!({
        "type": "req",
        "id": "info-1",
        "method": "gateway.info"
    });
    tx.send(Message::Text(info_req.to_string().into()))
        .await
        .unwrap();

    let info_resp = recv_text(&mut rx).await;
    assert_eq!(info_resp["type"], "res");
    assert_eq!(info_resp["id"], "info-1");
    assert_eq!(info_resp["ok"], true);
    assert!(info_resp["payload"]["version"].is_string());
    assert_eq!(info_resp["payload"]["protocol"], 4);

    let _ = shutdown.send(());
}

#[tokio::test]
async fn config_get_returns_hash() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    // Call config.get
    let config_req = json!({
        "type": "req",
        "id": "cfg-1",
        "method": "config.get"
    });
    tx.send(Message::Text(config_req.to_string().into()))
        .await
        .unwrap();

    let cfg_resp = recv_text(&mut rx).await;
    assert_eq!(cfg_resp["type"], "res");
    assert_eq!(cfg_resp["id"], "cfg-1");
    assert_eq!(cfg_resp["ok"], true);
    // Must include hash for CAS
    assert!(cfg_resp["payload"]["hash"].is_string());
    assert_eq!(cfg_resp["payload"]["exists"], true);

    let _ = shutdown.send(());
}

#[tokio::test]
async fn unknown_method_returns_error() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    // Call unknown method
    let bad_req = json!({
        "type": "req",
        "id": "bad-1",
        "method": "nonexistent.method"
    });
    tx.send(Message::Text(bad_req.to_string().into()))
        .await
        .unwrap();

    let bad_resp = recv_text(&mut rx).await;
    assert_eq!(bad_resp["type"], "res");
    assert_eq!(bad_resp["id"], "bad-1");
    assert_eq!(bad_resp["ok"], false);
    assert!(bad_resp["error"]["message"].is_string());

    let _ = shutdown.send(());
}

#[tokio::test]
async fn token_in_password_field_fallback() {
    // Bridge sometimes sends token in the password field
    let token = "my-token";
    let (url, shutdown) = start_token_auth_gateway(token).await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    let _challenge = recv_text(&mut rx).await;

    let connect_req = json!({
        "type": "req",
        "id": "pw-1",
        "method": "connect",
        "params": {
            "auth": { "password": token }
        }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();

    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["type"], "res");
    assert_eq!(resp["ok"], true, "token-in-password-field should succeed: {:?}", resp);

    let _ = shutdown.send(());
}

#[tokio::test]
async fn health_endpoint_accessible() {
    let (url, shutdown) = start_no_auth_gateway().await;
    let http_url = url.replace("ws://", "http://").replace("/ws", "/api/health");

    let client = reqwest::Client::new();
    let resp = client.get(&http_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert!(body["version"].is_string());

    let _ = shutdown.send(());
}

// =========================================================================
// v2026.3.11 — config.validate RPC
// =========================================================================

#[tokio::test]
async fn config_validate_returns_valid() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    // Call config.validate
    let validate_req = json!({
        "type": "req",
        "id": "cv-1",
        "method": "config.validate"
    });
    tx.send(Message::Text(validate_req.to_string().into()))
        .await
        .unwrap();

    let validate_resp = recv_text(&mut rx).await;
    assert_eq!(validate_resp["type"], "res");
    assert_eq!(validate_resp["id"], "cv-1");
    assert_eq!(validate_resp["ok"], true);
    assert!(validate_resp["payload"]["valid"].is_boolean());
    assert!(validate_resp["payload"]["issues"].is_array());
    assert!(validate_resp["payload"]["truncated"].is_boolean());

    let _ = shutdown.send(());
}

// =========================================================================
// v2026.3.11 — node.pending.enqueue / drain RPC
// =========================================================================

#[tokio::test]
async fn node_pending_enqueue_and_drain() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    // Enqueue work item 1
    let enqueue1 = json!({
        "type": "req",
        "id": "npe-1",
        "method": "node.pending.enqueue",
        "params": {
            "nodeId": "node-A",
            "workId": "work-1",
            "payload": { "task": "summarize" }
        }
    });
    tx.send(Message::Text(enqueue1.to_string().into()))
        .await
        .unwrap();
    let resp1 = recv_text(&mut rx).await;
    assert_eq!(resp1["ok"], true);
    assert_eq!(resp1["payload"]["workId"], "work-1");
    assert_eq!(resp1["payload"]["depth"], 1);

    // Enqueue work item 2
    let enqueue2 = json!({
        "type": "req",
        "id": "npe-2",
        "method": "node.pending.enqueue",
        "params": {
            "nodeId": "node-A",
            "workId": "work-2",
            "payload": { "task": "translate" }
        }
    });
    tx.send(Message::Text(enqueue2.to_string().into()))
        .await
        .unwrap();
    let resp2 = recv_text(&mut rx).await;
    assert_eq!(resp2["ok"], true);
    assert_eq!(resp2["payload"]["depth"], 2);

    // Drain with limit 1
    let drain = json!({
        "type": "req",
        "id": "npd-1",
        "method": "node.pending.drain",
        "params": {
            "nodeId": "node-A",
            "limit": 1
        }
    });
    tx.send(Message::Text(drain.to_string().into()))
        .await
        .unwrap();
    let drain_resp = recv_text(&mut rx).await;
    assert_eq!(drain_resp["ok"], true);
    assert_eq!(drain_resp["payload"]["count"], 1);
    let items = drain_resp["payload"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["workId"], "work-1");

    // Drain remaining
    let drain2 = json!({
        "type": "req",
        "id": "npd-2",
        "method": "node.pending.drain",
        "params": {
            "nodeId": "node-A",
            "limit": 10
        }
    });
    tx.send(Message::Text(drain2.to_string().into()))
        .await
        .unwrap();
    let drain_resp2 = recv_text(&mut rx).await;
    assert_eq!(drain_resp2["ok"], true);
    assert_eq!(drain_resp2["payload"]["count"], 1);
    let items2 = drain_resp2["payload"]["items"].as_array().unwrap();
    assert_eq!(items2[0]["workId"], "work-2");

    let _ = shutdown.send(());
}

#[tokio::test]
async fn node_pending_drain_empty_queue() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    // Drain from non-existent node
    let drain = json!({
        "type": "req",
        "id": "npd-1",
        "method": "node.pending.drain",
        "params": {
            "nodeId": "nonexistent-node",
            "limit": 10
        }
    });
    tx.send(Message::Text(drain.to_string().into()))
        .await
        .unwrap();
    let drain_resp = recv_text(&mut rx).await;
    assert_eq!(drain_resp["ok"], true);
    assert_eq!(drain_resp["payload"]["count"], 0);
    assert_eq!(drain_resp["payload"]["items"].as_array().unwrap().len(), 0);

    let _ = shutdown.send(());
}

// =========================================================================
// v2026.3.11 — node.pending.enqueue generates workId if absent
// =========================================================================

#[tokio::test]
async fn node_pending_enqueue_auto_generates_work_id() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    // Enqueue without explicit workId
    let enqueue = json!({
        "type": "req",
        "id": "npe-auto",
        "method": "node.pending.enqueue",
        "params": {
            "nodeId": "node-B",
            "payload": { "task": "auto" }
        }
    });
    tx.send(Message::Text(enqueue.to_string().into()))
        .await
        .unwrap();
    let enqueue_resp = recv_text(&mut rx).await;
    assert_eq!(enqueue_resp["ok"], true);
    // Should have an auto-generated workId (UUID format)
    let work_id = enqueue_resp["payload"]["workId"].as_str().unwrap();
    assert!(!work_id.is_empty());
    assert!(work_id.len() >= 32); // UUID with hyphens is 36 chars

    let _ = shutdown.send(());
}

// =========================================================================
// v2026.3.11 — status endpoint includes runtimeVersion and protocolVersion
// =========================================================================

#[tokio::test]
async fn status_rpc_includes_version_fields() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    // Call status
    let status_req = json!({
        "type": "req",
        "id": "st-1",
        "method": "status"
    });
    tx.send(Message::Text(status_req.to_string().into()))
        .await
        .unwrap();

    let status_resp = recv_text(&mut rx).await;
    assert_eq!(status_resp["type"], "res");
    assert_eq!(status_resp["ok"], true);
    assert!(status_resp["payload"]["runtimeVersion"].is_string());
    assert_eq!(status_resp["payload"]["protocolVersion"], 4);

    let _ = shutdown.send(());
}

// =========================================================================
// v2026.3.11 — sessions.list RPC
// =========================================================================

#[tokio::test]
async fn sessions_list_returns_empty() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    let list_req = json!({
        "type": "req",
        "id": "sl-1",
        "method": "sessions.list"
    });
    tx.send(Message::Text(list_req.to_string().into()))
        .await
        .unwrap();

    let list_resp = recv_text(&mut rx).await;
    assert_eq!(list_resp["type"], "res");
    assert_eq!(list_resp["ok"], true);
    // payload is the sessions array directly
    assert!(list_resp["payload"].is_array());

    let _ = shutdown.send(());
}

// =========================================================================
// v2026.3.11 — HTTP endpoint tests
// =========================================================================

#[tokio::test]
async fn http_status_endpoint_includes_runtime_version() {
    let (url, shutdown) = start_no_auth_gateway().await;
    let http_url = url.replace("ws://", "http://").replace("/ws", "/api/status");

    let client = reqwest::Client::new();
    let resp = client.get(&http_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert!(body["version"].is_string());
    assert!(body["runtimeVersion"].is_string());
    assert_eq!(body["protocolVersion"], 4);
    assert!(body["uptime"].is_number());
    assert!(body["sessions"].is_number());

    let _ = shutdown.send(());
}

#[tokio::test]
async fn http_config_validate_endpoint() {
    let (url, shutdown) = start_no_auth_gateway().await;
    let http_url = url
        .replace("ws://", "http://")
        .replace("/ws", "/api/config/validate");

    let client = reqwest::Client::new();
    let resp = client.post(&http_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["valid"].is_boolean());
    assert!(body["issues"].is_array());

    let _ = shutdown.send(());
}

#[tokio::test]
async fn http_config_schema_endpoint() {
    let (url, shutdown) = start_no_auth_gateway().await;
    let http_url = url
        .replace("ws://", "http://")
        .replace("/ws", "/api/config/schema");

    let client = reqwest::Client::new();
    let resp = client.get(&http_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["schema"].is_object());
    assert!(body["schema"]["properties"]["agents"].is_object());
    assert!(body["schema"]["properties"]["models"].is_object());
    assert!(body["schema"]["properties"]["gateway"].is_object());

    let _ = shutdown.send(());
}

#[tokio::test]
async fn http_cron_jobs_endpoint() {
    let (url, shutdown) = start_no_auth_gateway().await;
    let http_url = url
        .replace("ws://", "http://")
        .replace("/ws", "/api/cron/jobs");

    let client = reqwest::Client::new();
    let resp = client.get(&http_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["jobs"].is_array());

    let _ = shutdown.send(());
}

#[tokio::test]
async fn http_cron_job_detail_not_found() {
    let (url, shutdown) = start_no_auth_gateway().await;
    let http_url = url
        .replace("ws://", "http://")
        .replace("/ws", "/api/cron/jobs/nonexistent-id");

    let client = reqwest::Client::new();
    let resp = client.get(&http_url).send().await.unwrap();
    assert_eq!(resp.status(), 404);

    let _ = shutdown.send(());
}

#[tokio::test]
async fn http_cron_status_endpoint() {
    let (url, shutdown) = start_no_auth_gateway().await;
    let http_url = url
        .replace("ws://", "http://")
        .replace("/ws", "/api/cron/status");

    let client = reqwest::Client::new();
    let resp = client.get(&http_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["running"].as_bool().unwrap());
    assert_eq!(body["jobCount"], 0);
    assert_eq!(body["errorCount"], 0);

    let _ = shutdown.send(());
}

#[tokio::test]
async fn http_usage_endpoint() {
    let (url, shutdown) = start_no_auth_gateway().await;
    let http_url = url
        .replace("ws://", "http://")
        .replace("/ws", "/api/usage");

    let client = reqwest::Client::new();
    let resp = client.get(&http_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["totalInputTokens"], 0);
    assert_eq!(body["totalOutputTokens"], 0);
    assert_eq!(body["totalRequests"], 0);

    let _ = shutdown.send(());
}

#[tokio::test]
async fn http_agents_list_endpoint() {
    let (url, shutdown) = start_no_auth_gateway().await;
    let http_url = url
        .replace("ws://", "http://")
        .replace("/ws", "/api/agents");

    let client = reqwest::Client::new();
    let resp = client.get(&http_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    let agents = body["agents"].as_array().unwrap();
    // Should contain at least the default agent
    assert!(agents.iter().any(|a| a["id"] == "default"));

    let _ = shutdown.send(());
}

#[tokio::test]
async fn http_models_list_endpoint() {
    let (url, shutdown) = start_no_auth_gateway().await;
    let http_url = url
        .replace("ws://", "http://")
        .replace("/ws", "/api/models");

    let client = reqwest::Client::new();
    let resp = client.get(&http_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["models"].is_array());

    let _ = shutdown.send(());
}

#[tokio::test]
async fn http_sessions_list_endpoint() {
    let (url, shutdown) = start_no_auth_gateway().await;
    let http_url = url
        .replace("ws://", "http://")
        .replace("/ws", "/api/sessions");

    let client = reqwest::Client::new();
    let resp = client.get(&http_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    // HTTP endpoint returns a plain array (not wrapped in {sessions: []})
    assert!(body.is_array());

    let _ = shutdown.send(());
}

// =========================================================================
// v2026.3.11 — config.reload RPC
// =========================================================================

#[tokio::test]
async fn config_reload_basic() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    // Reload without secrets resolution
    let reload_req = json!({
        "type": "req",
        "id": "cr-1",
        "method": "config.reload"
    });
    tx.send(Message::Text(reload_req.to_string().into()))
        .await
        .unwrap();

    let reload_resp = recv_text(&mut rx).await;
    assert_eq!(reload_resp["type"], "res");
    assert_eq!(reload_resp["id"], "cr-1");
    assert_eq!(reload_resp["ok"], true);
    assert!(reload_resp["payload"]["hash"].is_string());
    assert_eq!(reload_resp["payload"]["secretsResolved"], false);

    let _ = shutdown.send(());
}

#[tokio::test]
async fn config_reload_with_secrets_resolution() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    // Reload with secrets resolution enabled (no actual secret refs in default config)
    let reload_req = json!({
        "type": "req",
        "id": "cr-2",
        "method": "config.reload",
        "params": {
            "resolveSecrets": true
        }
    });
    tx.send(Message::Text(reload_req.to_string().into()))
        .await
        .unwrap();

    let reload_resp = recv_text(&mut rx).await;
    assert_eq!(reload_resp["type"], "res");
    assert_eq!(reload_resp["id"], "cr-2");
    assert_eq!(reload_resp["ok"], true);
    assert_eq!(reload_resp["payload"]["secretsResolved"], true);
    assert!(reload_resp["payload"]["hash"].is_string());

    let _ = shutdown.send(());
}

// =========================================================================
// v2026.3.11 — secrets.resolve RPC
// =========================================================================

#[tokio::test]
async fn secrets_resolve_from_env_var() {
    // Set a test env var
    std::env::set_var("MYLOBSTER_TEST_SECRET_KEY", "test-secret-value-42");

    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    // Resolve env var
    let resolve_req = json!({
        "type": "req",
        "id": "sr-1",
        "method": "secrets.resolve",
        "params": {
            "key": "MYLOBSTER_TEST_SECRET_KEY"
        }
    });
    tx.send(Message::Text(resolve_req.to_string().into()))
        .await
        .unwrap();

    let resolve_resp = recv_text(&mut rx).await;
    assert_eq!(resolve_resp["type"], "res");
    assert_eq!(resolve_resp["ok"], true);
    assert_eq!(resolve_resp["payload"]["key"], "MYLOBSTER_TEST_SECRET_KEY");
    assert_eq!(resolve_resp["payload"]["value"], "test-secret-value-42");
    assert_eq!(resolve_resp["payload"]["resolved"], true);

    std::env::remove_var("MYLOBSTER_TEST_SECRET_KEY");
    let _ = shutdown.send(());
}

#[tokio::test]
async fn secrets_resolve_from_config_path() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    // Resolve dotted config path — gateway.port exists in default config
    let resolve_req = json!({
        "type": "req",
        "id": "sr-2",
        "method": "secrets.resolve",
        "params": {
            "key": "gateway.port"
        }
    });
    tx.send(Message::Text(resolve_req.to_string().into()))
        .await
        .unwrap();

    let resolve_resp = recv_text(&mut rx).await;
    assert_eq!(resolve_resp["type"], "res");
    assert_eq!(resolve_resp["ok"], true);
    assert_eq!(resolve_resp["payload"]["key"], "gateway.port");
    assert_eq!(resolve_resp["payload"]["resolved"], true);
    // Default port is 18789
    assert_eq!(resolve_resp["payload"]["value"], 18789);

    let _ = shutdown.send(());
}

#[tokio::test]
async fn secrets_resolve_missing_key_returns_null() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    // Resolve a key that doesn't exist anywhere
    let resolve_req = json!({
        "type": "req",
        "id": "sr-3",
        "method": "secrets.resolve",
        "params": {
            "key": "NONEXISTENT_KEY_XYZZY_12345"
        }
    });
    tx.send(Message::Text(resolve_req.to_string().into()))
        .await
        .unwrap();

    let resolve_resp = recv_text(&mut rx).await;
    assert_eq!(resolve_resp["type"], "res");
    assert_eq!(resolve_resp["ok"], true);
    assert_eq!(resolve_resp["payload"]["key"], "NONEXISTENT_KEY_XYZZY_12345");
    assert!(resolve_resp["payload"]["value"].is_null());
    assert_eq!(resolve_resp["payload"]["resolved"], false);

    let _ = shutdown.send(());
}

#[tokio::test]
async fn secrets_resolve_missing_key_param_returns_error() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    // Call without key param
    let resolve_req = json!({
        "type": "req",
        "id": "sr-4",
        "method": "secrets.resolve",
        "params": {}
    });
    tx.send(Message::Text(resolve_req.to_string().into()))
        .await
        .unwrap();

    let resolve_resp = recv_text(&mut rx).await;
    assert_eq!(resolve_resp["type"], "res");
    assert_eq!(resolve_resp["ok"], false);
    assert!(resolve_resp["error"]["message"].as_str().unwrap().contains("key"));

    let _ = shutdown.send(());
}

// =========================================================================
// v2026.3.11 — secrets.reload RPC
// =========================================================================

#[tokio::test]
async fn secrets_reload_basic() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    // Basic secrets reload (no secret refs in default config)
    let reload_req = json!({
        "type": "req",
        "id": "srl-1",
        "method": "secrets.reload"
    });
    tx.send(Message::Text(reload_req.to_string().into()))
        .await
        .unwrap();

    let reload_resp = recv_text(&mut rx).await;
    assert_eq!(reload_resp["type"], "res");
    assert_eq!(reload_resp["id"], "srl-1");
    assert_eq!(reload_resp["ok"], true);
    assert_eq!(reload_resp["payload"]["ok"], true);
    assert_eq!(reload_resp["payload"]["resolvedCount"], 0);
    assert_eq!(reload_resp["payload"]["failedCount"], 0);
    assert_eq!(reload_resp["payload"]["configUpdated"], false);

    let _ = shutdown.send(());
}

#[tokio::test]
async fn secrets_reload_with_required_paths() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    // Secrets reload with requiredPaths (still no refs, so should succeed)
    let reload_req = json!({
        "type": "req",
        "id": "srl-2",
        "method": "secrets.reload",
        "params": {
            "requiredPaths": ["models"],
            "failOnMissing": false
        }
    });
    tx.send(Message::Text(reload_req.to_string().into()))
        .await
        .unwrap();

    let reload_resp = recv_text(&mut rx).await;
    assert_eq!(reload_resp["type"], "res");
    assert_eq!(reload_resp["ok"], true);
    assert_eq!(reload_resp["payload"]["ok"], true);

    let _ = shutdown.send(());
}

// =========================================================================
// v2026.3.11 — cron.status RPC includes errorCount
// =========================================================================

#[tokio::test]
async fn cron_status_rpc_includes_error_count() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    // Call cron.status via RPC
    let status_req = json!({
        "type": "req",
        "id": "cs-1",
        "method": "cron.status"
    });
    tx.send(Message::Text(status_req.to_string().into()))
        .await
        .unwrap();

    let status_resp = recv_text(&mut rx).await;
    assert_eq!(status_resp["type"], "res");
    assert_eq!(status_resp["id"], "cs-1");
    assert_eq!(status_resp["ok"], true);
    assert_eq!(status_resp["payload"]["running"], true);
    assert_eq!(status_resp["payload"]["jobCount"], 0);
    // v2026.3.11: errorCount must be present
    assert_eq!(status_resp["payload"]["errorCount"], 0);

    let _ = shutdown.send(());
}

// =========================================================================
// v2026.3.11 — gateway.info includes capabilities
// =========================================================================

#[tokio::test]
async fn gateway_info_includes_capabilities() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    // Call gateway.info and verify capabilities
    let info_req = json!({
        "type": "req",
        "id": "gi-1",
        "method": "gateway.info"
    });
    tx.send(Message::Text(info_req.to_string().into()))
        .await
        .unwrap();

    let info_resp = recv_text(&mut rx).await;
    assert_eq!(info_resp["ok"], true);
    // gateway.info includes version, protocol, uptimeSeconds, sessionsActive
    assert!(info_resp["payload"]["version"].is_string());
    assert!(info_resp["payload"]["uptimeSeconds"].is_number());
    assert!(info_resp["payload"]["sessionsActive"].is_number());

    let _ = shutdown.send(());
}

// =========================================================================
// v2026.3.11 — config.get returns config and hash together
// =========================================================================

#[tokio::test]
async fn config_get_contains_config_data() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    // config.get should include the actual config data
    let config_req = json!({
        "type": "req",
        "id": "cg-1",
        "method": "config.get"
    });
    tx.send(Message::Text(config_req.to_string().into()))
        .await
        .unwrap();

    let cfg_resp = recv_text(&mut rx).await;
    assert_eq!(cfg_resp["ok"], true);
    // config.get returns hash and exists flag
    assert!(cfg_resp["payload"]["hash"].is_string());
    assert_eq!(cfg_resp["payload"]["exists"], true);
    // Verify hash is non-empty
    let hash = cfg_resp["payload"]["hash"].as_str().unwrap();
    assert!(!hash.is_empty());

    let _ = shutdown.send(());
}

// =========================================================================
// v2026.3.11 — config.reload changes hash
// =========================================================================

#[tokio::test]
async fn config_reload_changes_hash() {
    let (url, shutdown) = start_no_auth_gateway().await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Complete handshake
    let _challenge = recv_text(&mut rx).await;
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_text(&mut rx).await;
    assert_eq!(resp["ok"], true);

    // Get initial config hash
    let get_req = json!({
        "type": "req",
        "id": "cg-1",
        "method": "config.get"
    });
    tx.send(Message::Text(get_req.to_string().into()))
        .await
        .unwrap();
    let get_resp = recv_text(&mut rx).await;
    let hash1 = get_resp["payload"]["hash"].as_str().unwrap().to_string();

    // Reload config
    let reload_req = json!({
        "type": "req",
        "id": "cr-1",
        "method": "config.reload"
    });
    tx.send(Message::Text(reload_req.to_string().into()))
        .await
        .unwrap();
    let reload_resp = recv_text(&mut rx).await;
    let hash2 = reload_resp["payload"]["hash"].as_str().unwrap().to_string();

    // Hashes should differ after reload
    assert_ne!(hash1, hash2, "config hash should change after reload");

    let _ = shutdown.send(());
}
