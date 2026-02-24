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
use mylobster::gateway::{ResolvedGatewayAuth, GatewayState};
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
    assert_eq!(resp["payload"]["protocol"], 3);
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
    assert_eq!(resp["payload"]["protocol"], 3);

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
    assert_eq!(info_resp["payload"]["protocol"], 3);

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
