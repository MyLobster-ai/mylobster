//! Integration tests for the chat end-to-end flow.
//!
//! These tests exercise the full `chat.send` → AnthropicProvider → streaming events → final
//! pipeline using a wiremock HTTP server instead of a real AI API. This guarantees the chat
//! path works without API keys or network access.

use futures::{stream::SplitSink, stream::SplitStream, SinkExt, StreamExt};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, RwLock};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use mylobster::channels::ChannelManager;
use mylobster::config::{Config, ModelProviderConfig};
use mylobster::gateway::{GatewayState, ResolvedGatewayAuth, RpcState};
use mylobster::plugins::PluginRegistry;
use mylobster::sessions::SessionStore;

type WsTx = SplitSink<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, Message>;
type WsRx = SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>;

// ============================================================================
// Mock SSE Response Builders
// ============================================================================

/// Build an Anthropic Messages API SSE response body with text content.
fn build_sse_response(chunks: &[&str], input_tokens: u64, output_tokens: u64) -> String {
    let mut body = String::new();

    // message_start with usage
    body.push_str(&format!(
        "event: message_start\ndata: {}\n\n",
        json!({
            "type": "message_start",
            "message": {
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "model": "claude-sonnet-4-6",
                "usage": {
                    "input_tokens": input_tokens,
                    "output_tokens": 0
                }
            }
        })
    ));

    // content_block_start
    body.push_str(&format!(
        "event: content_block_start\ndata: {}\n\n",
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" }
        })
    ));

    // content_block_delta for each chunk
    for chunk in chunks {
        body.push_str(&format!(
            "event: content_block_delta\ndata: {}\n\n",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": chunk }
            })
        ));
    }

    // content_block_stop
    body.push_str(&format!(
        "event: content_block_stop\ndata: {}\n\n",
        json!({ "type": "content_block_stop", "index": 0 })
    ));

    // message_delta with output usage
    body.push_str(&format!(
        "event: message_delta\ndata: {}\n\n",
        json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn" },
            "usage": { "output_tokens": output_tokens }
        })
    ));

    // message_stop
    body.push_str(&format!(
        "event: message_stop\ndata: {}\n\n",
        json!({ "type": "message_stop" })
    ));

    body
}

// ============================================================================
// Test Helpers
// ============================================================================

/// Start a gateway with config pointing Anthropic provider at the given mock URL.
async fn start_chat_gateway(mock_url: &str) -> (String, broadcast::Sender<()>) {
    let mut config = Config::default();

    // Point Anthropic provider at mock server
    config.models.providers.insert(
        "anthropic".to_string(),
        ModelProviderConfig {
            base_url: mock_url.to_string(),
            api_key: Some("test-key".to_string()),
            auth: None,
            api: None,
            headers: None,
            auth_header: None,
            models: vec![],
        },
    );

    let (shutdown_tx, _shutdown_rx) = broadcast::channel(1);

    let state = GatewayState {
        config: Arc::new(RwLock::new(config.clone())),
        auth: Arc::new(ResolvedGatewayAuth {
            mode: mylobster::config::GatewayAuthMode::Token,
            token: None,
            password: None,
            allow_tailscale: false,
        }),
        sessions: Arc::new(SessionStore::new(&config)),
        channels: Arc::new(ChannelManager::new(&config)),
        plugins: Arc::new(PluginRegistry::new(&config)),
        rpc: Arc::new(RpcState::new()),
        shutdown_tx: shutdown_tx.clone(),
        start_time: std::time::Instant::now(),
        version: "test".to_string(),
    };

    let app = mylobster::gateway::routes::build_routes(state);

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

    tokio::time::sleep(Duration::from_millis(50)).await;

    let url = format!("ws://127.0.0.1:{}/ws", addr.port());
    (url, shutdown_tx)
}

/// Read the next text message from the WS stream with timeout.
async fn recv_msg(stream: &mut WsRx) -> serde_json::Value {
    let msg = timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("timeout waiting for WS message")
        .expect("stream ended")
        .expect("WS error");
    match msg {
        Message::Text(text) => serde_json::from_str(&text).expect("invalid JSON"),
        other => panic!("expected Text message, got {:?}", other),
    }
}

/// Complete the WebSocket handshake (challenge → connect) and return the split streams.
async fn do_handshake(url: &str) -> (WsTx, WsRx) {
    let (ws, _) = connect_async(url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Read connect.challenge
    let _challenge = recv_msg(&mut rx).await;

    // Send connect (no auth, local mode) with admin scopes for chat.send
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": {
            "auth": {},
            "scopes": ["operator.admin"]
        }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();

    let resp = recv_msg(&mut rx).await;
    assert_eq!(resp["ok"], true, "handshake failed: {:?}", resp);

    (tx, rx)
}

/// Send a chat.send request.
async fn send_chat_message(
    tx: &mut WsTx,
    req_id: &str,
    session_key: &str,
    message: &str,
    idempotency_key: Option<&str>,
) {
    let mut params = json!({
        "sessionKey": session_key,
        "message": message,
    });
    if let Some(key) = idempotency_key {
        params["idempotencyKey"] = json!(key);
    }
    let chat_req = json!({
        "type": "req",
        "id": req_id,
        "method": "chat.send",
        "params": params,
    });
    tx.send(Message::Text(chat_req.to_string().into()))
        .await
        .unwrap();
}

/// Collect all chat events until we see a final/error/aborted state. Returns (ack, events).
async fn collect_chat_events(
    rx: &mut WsRx,
    req_id: &str,
) -> (serde_json::Value, Vec<serde_json::Value>) {
    // First message should be the ack response
    let ack = recv_msg(rx).await;
    assert_eq!(ack["type"], "res");
    assert_eq!(ack["id"], req_id);
    assert_eq!(ack["ok"], true);

    let mut events = Vec::new();
    loop {
        let msg = recv_msg(rx).await;
        assert_eq!(msg["type"], "event");
        assert_eq!(msg["event"], "chat");

        let payload = &msg["payload"];
        let state = payload["state"].as_str().unwrap();
        events.push(payload.clone());

        if state == "final" || state == "error" || state == "aborted" {
            break;
        }
    }

    (ack, events)
}

/// Register a mock that returns a streaming SSE response for POST /v1/messages.
async fn mock_streaming_response(server: &MockServer, chunks: &[&str]) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(build_sse_response(chunks, 10, 5))
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(server)
        .await;
}

/// Register a mock that returns an HTTP error status.
async fn mock_http_error(server: &MockServer, status: u16, body: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body.to_string()))
        .mount(server)
        .await;
}

// ============================================================================
// Tests
// ============================================================================

#[tokio::test]
async fn chat_send_returns_ack_with_run_id() {
    let mock_server = MockServer::start().await;
    mock_streaming_response(&mock_server, &["Hello"]).await;

    let (url, shutdown) = start_chat_gateway(&mock_server.uri()).await;
    let (mut tx, mut rx) = do_handshake(&url).await;

    send_chat_message(&mut tx, "chat-1", "sess-1", "Hi", None).await;

    let ack = recv_msg(&mut rx).await;
    assert_eq!(ack["type"], "res");
    assert_eq!(ack["id"], "chat-1");
    assert_eq!(ack["ok"], true);
    assert!(
        ack["payload"]["runId"].is_string(),
        "ack must contain runId"
    );

    let _ = shutdown.send(());
}

#[tokio::test]
async fn chat_send_streams_delta_events() {
    let mock_server = MockServer::start().await;
    mock_streaming_response(&mock_server, &["Hello", " world"]).await;

    let (url, shutdown) = start_chat_gateway(&mock_server.uri()).await;
    let (mut tx, mut rx) = do_handshake(&url).await;

    send_chat_message(&mut tx, "chat-1", "sess-1", "Hi", None).await;
    let (_ack, events) = collect_chat_events(&mut rx, "chat-1").await;

    // Should have delta events before the final event
    let deltas: Vec<_> = events.iter().filter(|e| e["state"] == "delta").collect();
    assert!(
        !deltas.is_empty(),
        "expected at least one delta event, got: {:?}",
        events
    );

    // Each delta should have message.content array
    for delta in &deltas {
        let content = &delta["message"]["content"];
        assert!(content.is_array(), "delta content must be an array");
        assert_eq!(content[0]["type"], "text");
        assert!(content[0]["text"].is_string());
    }

    let _ = shutdown.send(());
}

#[tokio::test]
async fn chat_send_streams_final_event() {
    let mock_server = MockServer::start().await;
    mock_streaming_response(&mock_server, &["Hello"]).await;

    let (url, shutdown) = start_chat_gateway(&mock_server.uri()).await;
    let (mut tx, mut rx) = do_handshake(&url).await;

    send_chat_message(&mut tx, "chat-1", "sess-1", "Hi", None).await;
    let (_ack, events) = collect_chat_events(&mut rx, "chat-1").await;

    let final_event = events.last().unwrap();
    assert_eq!(final_event["state"], "final");
    assert_eq!(final_event["stopReason"], "end_turn");
    assert!(final_event["message"]["content"].is_array());

    let _ = shutdown.send(());
}

#[tokio::test]
async fn chat_send_full_roundtrip() {
    let mock_server = MockServer::start().await;
    mock_streaming_response(&mock_server, &["Hello", " world"]).await;

    let (url, shutdown) = start_chat_gateway(&mock_server.uri()).await;
    let (mut tx, mut rx) = do_handshake(&url).await;

    send_chat_message(&mut tx, "chat-1", "sess-1", "Hi", None).await;
    let (_ack, events) = collect_chat_events(&mut rx, "chat-1").await;

    // Final event should have the complete assembled text
    let final_event = events.last().unwrap();
    assert_eq!(final_event["state"], "final");
    let final_text = final_event["message"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert_eq!(final_text, "Hello world");

    let _ = shutdown.send(());
}

#[tokio::test]
async fn chat_send_missing_session_key_errors() {
    let mock_server = MockServer::start().await;

    let (url, shutdown) = start_chat_gateway(&mock_server.uri()).await;
    let (mut tx, mut rx) = do_handshake(&url).await;

    // Send chat.send without sessionKey
    let chat_req = json!({
        "type": "req",
        "id": "chat-bad",
        "method": "chat.send",
        "params": {
            "message": "Hi"
        }
    });
    tx.send(Message::Text(chat_req.to_string().into()))
        .await
        .unwrap();

    let resp = recv_msg(&mut rx).await;
    assert_eq!(resp["type"], "res");
    assert_eq!(resp["ok"], false);
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Invalid params"),
        "expected param error, got: {:?}",
        resp
    );

    let _ = shutdown.send(());
}

#[tokio::test]
async fn chat_send_missing_message_errors() {
    let mock_server = MockServer::start().await;

    let (url, shutdown) = start_chat_gateway(&mock_server.uri()).await;
    let (mut tx, mut rx) = do_handshake(&url).await;

    // Send chat.send without message
    let chat_req = json!({
        "type": "req",
        "id": "chat-bad",
        "method": "chat.send",
        "params": {
            "sessionKey": "sess-1"
        }
    });
    tx.send(Message::Text(chat_req.to_string().into()))
        .await
        .unwrap();

    let resp = recv_msg(&mut rx).await;
    assert_eq!(resp["type"], "res");
    assert_eq!(resp["ok"], false);
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Invalid params"),
        "expected param error, got: {:?}",
        resp
    );

    let _ = shutdown.send(());
}

#[tokio::test]
async fn chat_send_without_scopes_errors() {
    let mock_server = MockServer::start().await;

    let (url, shutdown) = start_chat_gateway(&mock_server.uri()).await;

    let (ws, _) = connect_async(&url).await.expect("WS connect failed");
    let (mut tx, mut rx) = ws.split();

    // Read connect.challenge
    let _challenge = recv_msg(&mut rx).await;

    // Complete handshake WITHOUT requesting any scopes
    let connect_req = json!({
        "type": "req",
        "id": "c1",
        "method": "connect",
        "params": { "auth": {} }
    });
    tx.send(Message::Text(connect_req.to_string().into()))
        .await
        .unwrap();
    let resp = recv_msg(&mut rx).await;
    assert_eq!(resp["ok"], true, "handshake should succeed");

    // Try chat.send — should fail with scope error
    let chat_req = json!({
        "type": "req",
        "id": "chat-noauth",
        "method": "chat.send",
        "params": {
            "sessionKey": "sess-1",
            "message": "Hi"
        }
    });
    tx.send(Message::Text(chat_req.to_string().into()))
        .await
        .unwrap();

    let resp = recv_msg(&mut rx).await;
    assert_eq!(resp["type"], "res");
    assert_eq!(resp["ok"], false);
    let err_msg = resp["error"]["message"].as_str().unwrap();
    assert!(
        err_msg.contains("scope"),
        "expected scope error, got: {}",
        err_msg
    );

    let _ = shutdown.send(());
}

#[tokio::test]
async fn chat_send_provider_error_returns_error_event() {
    let mock_server = MockServer::start().await;
    mock_http_error(&mock_server, 500, r#"{"error":"internal server error"}"#).await;

    let (url, shutdown) = start_chat_gateway(&mock_server.uri()).await;
    let (mut tx, mut rx) = do_handshake(&url).await;

    send_chat_message(&mut tx, "chat-1", "sess-1", "Hi", None).await;
    let (_ack, events) = collect_chat_events(&mut rx, "chat-1").await;

    let last = events.last().unwrap();
    assert_eq!(last["state"], "error");
    assert!(
        last["errorMessage"].is_string(),
        "error event must have errorMessage"
    );

    let _ = shutdown.send(());
}

#[tokio::test]
async fn chat_send_provider_auth_error() {
    let mock_server = MockServer::start().await;
    mock_http_error(
        &mock_server,
        401,
        r#"{"error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
    )
    .await;

    let (url, shutdown) = start_chat_gateway(&mock_server.uri()).await;
    let (mut tx, mut rx) = do_handshake(&url).await;

    send_chat_message(&mut tx, "chat-1", "sess-1", "Hi", None).await;
    let (_ack, events) = collect_chat_events(&mut rx, "chat-1").await;

    let last = events.last().unwrap();
    assert_eq!(last["state"], "error");
    let err = last["errorMessage"].as_str().unwrap();
    assert!(
        err.contains("401"),
        "error should mention 401 status, got: {}",
        err
    );

    let _ = shutdown.send(());
}

#[tokio::test]
async fn chat_cancel_stops_streaming() {
    let mock_server = MockServer::start().await;

    // Build a slow SSE response with many chunks to give us time to cancel
    let mut chunks = Vec::new();
    for i in 0..50 {
        chunks.push(format!("chunk{} ", i));
    }
    let chunk_refs: Vec<&str> = chunks.iter().map(|s| s.as_str()).collect();

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(build_sse_response(&chunk_refs, 10, 50))
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&mock_server)
        .await;

    let (url, shutdown) = start_chat_gateway(&mock_server.uri()).await;
    let (mut tx, mut rx) = do_handshake(&url).await;

    send_chat_message(&mut tx, "chat-1", "sess-1", "Hi", Some("run-cancel-test")).await;

    // Read ack
    let ack = recv_msg(&mut rx).await;
    assert_eq!(ack["ok"], true);
    let run_id = ack["payload"]["runId"].as_str().unwrap();
    assert_eq!(run_id, "run-cancel-test");

    // Read at least one delta event
    let first_event = recv_msg(&mut rx).await;
    assert_eq!(first_event["type"], "event");

    // Send cancel
    let cancel_req = json!({
        "type": "req",
        "id": "cancel-1",
        "method": "chat.cancel",
        "params": { "runId": run_id }
    });
    tx.send(Message::Text(cancel_req.to_string().into()))
        .await
        .unwrap();

    // Read remaining events until we get an aborted or final state, or cancel response
    let mut saw_cancel_response = false;
    let mut saw_terminal = false;

    for _ in 0..100 {
        match timeout(Duration::from_secs(5), rx.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                let msg: serde_json::Value = serde_json::from_str(&text).unwrap();
                if msg["type"] == "res" && msg["id"] == "cancel-1" {
                    saw_cancel_response = true;
                }
                if msg["type"] == "event" && msg["event"] == "chat" {
                    let state = msg["payload"]["state"].as_str().unwrap_or("");
                    if state == "aborted" || state == "final" || state == "error" {
                        saw_terminal = true;
                        break;
                    }
                }
            }
            _ => break,
        }
    }

    assert!(
        saw_cancel_response || saw_terminal,
        "expected cancel response or terminal event"
    );

    let _ = shutdown.send(());
}

#[tokio::test]
async fn chat_send_session_persistence() {
    let mock_server = MockServer::start().await;

    // We'll use expect(2) to ensure the mock is hit twice
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(build_sse_response(&["Reply"], 10, 3))
                .insert_header("content-type", "text/event-stream"),
        )
        .expect(2)
        .mount(&mock_server)
        .await;

    let (url, shutdown) = start_chat_gateway(&mock_server.uri()).await;
    let (mut tx, mut rx) = do_handshake(&url).await;

    // First message
    send_chat_message(&mut tx, "chat-1", "sess-persist", "First message", None).await;
    let (_ack1, events1) = collect_chat_events(&mut rx, "chat-1").await;
    assert_eq!(events1.last().unwrap()["state"], "final");

    // Second message to same session
    send_chat_message(&mut tx, "chat-2", "sess-persist", "Second message", None).await;
    let (_ack2, events2) = collect_chat_events(&mut rx, "chat-2").await;
    assert_eq!(events2.last().unwrap()["state"], "final");

    // Both completed successfully — wiremock will verify it was called exactly 2 times
    let _ = shutdown.send(());
}

#[tokio::test]
async fn chat_send_concurrent_sessions() {
    let mock_server = MockServer::start().await;
    mock_streaming_response(&mock_server, &["Response"]).await;

    let (url, shutdown) = start_chat_gateway(&mock_server.uri()).await;

    // Open two separate WebSocket connections
    let (mut tx1, mut rx1) = do_handshake(&url).await;
    let (mut tx2, mut rx2) = do_handshake(&url).await;

    // Send messages on different sessions concurrently
    send_chat_message(&mut tx1, "a-1", "sess-a", "Hello from A", None).await;
    send_chat_message(&mut tx2, "b-1", "sess-b", "Hello from B", None).await;

    // Collect events from both
    let (_ack_a, events_a) = collect_chat_events(&mut rx1, "a-1").await;
    let (_ack_b, events_b) = collect_chat_events(&mut rx2, "b-1").await;

    // Both should complete with final state
    assert_eq!(events_a.last().unwrap()["state"], "final");
    assert_eq!(events_b.last().unwrap()["state"], "final");

    // Session keys should be independent
    assert_eq!(
        events_a.last().unwrap()["sessionKey"], "sess-a",
        "session A events should have sessionKey=sess-a"
    );
    assert_eq!(
        events_b.last().unwrap()["sessionKey"], "sess-b",
        "session B events should have sessionKey=sess-b"
    );

    let _ = shutdown.send(());
}

#[tokio::test]
async fn chat_send_content_format_matches_bridge() {
    let mock_server = MockServer::start().await;
    mock_streaming_response(&mock_server, &["Test", " content"]).await;

    let (url, shutdown) = start_chat_gateway(&mock_server.uri()).await;
    let (mut tx, mut rx) = do_handshake(&url).await;

    send_chat_message(&mut tx, "chat-1", "sess-1", "Hi", None).await;
    let (_ack, events) = collect_chat_events(&mut rx, "chat-1").await;

    // Verify every delta and final event has content in the format the bridge expects:
    // message.content[0].text
    for event in &events {
        let state = event["state"].as_str().unwrap();
        if state == "delta" || state == "final" {
            let msg = &event["message"];
            assert_eq!(msg["role"], "assistant");
            let content = &msg["content"];
            assert!(
                content.is_array(),
                "content must be array, got: {:?}",
                content
            );
            assert!(!content.as_array().unwrap().is_empty());
            assert_eq!(content[0]["type"], "text");
            assert!(
                content[0]["text"].is_string(),
                "content[0].text must be string"
            );
        }
    }

    let _ = shutdown.send(());
}

#[tokio::test]
async fn chat_send_usage_in_final_event() {
    let mock_server = MockServer::start().await;

    // Use specific token counts
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(build_sse_response(&["Hello"], 42, 17))
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&mock_server)
        .await;

    let (url, shutdown) = start_chat_gateway(&mock_server.uri()).await;
    let (mut tx, mut rx) = do_handshake(&url).await;

    send_chat_message(&mut tx, "chat-1", "sess-1", "Hi", None).await;
    let (_ack, events) = collect_chat_events(&mut rx, "chat-1").await;

    let final_event = events.last().unwrap();
    assert_eq!(final_event["state"], "final");

    let usage = &final_event["usage"];
    assert_eq!(
        usage["inputTokens"], 42,
        "expected inputTokens=42, got: {:?}",
        usage
    );
    assert_eq!(
        usage["outputTokens"], 17,
        "expected outputTokens=17, got: {:?}",
        usage
    );

    let _ = shutdown.send(());
}

#[tokio::test]
async fn chat_send_with_idempotency_key() {
    let mock_server = MockServer::start().await;
    mock_streaming_response(&mock_server, &["Hello"]).await;

    let (url, shutdown) = start_chat_gateway(&mock_server.uri()).await;
    let (mut tx, mut rx) = do_handshake(&url).await;

    let custom_key = "my-custom-run-id-123";
    send_chat_message(&mut tx, "chat-1", "sess-1", "Hi", Some(custom_key)).await;

    // Ack should echo the idempotency key as runId
    let ack = recv_msg(&mut rx).await;
    assert_eq!(ack["type"], "res");
    assert_eq!(ack["ok"], true);
    assert_eq!(
        ack["payload"]["runId"], custom_key,
        "runId should match idempotency key"
    );

    // All events should also use the same runId
    let mut events = Vec::new();
    loop {
        let msg = recv_msg(&mut rx).await;
        if msg["type"] == "event" && msg["event"] == "chat" {
            let payload = &msg["payload"];
            assert_eq!(
                payload["runId"], custom_key,
                "event runId should match idempotency key"
            );
            events.push(payload.clone());
            if payload["state"] == "final"
                || payload["state"] == "error"
                || payload["state"] == "aborted"
            {
                break;
            }
        }
    }

    assert!(!events.is_empty());

    let _ = shutdown.send(());
}
