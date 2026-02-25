use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Protocol version for WebSocket communication.
/// Version 3 matches OpenClaw v2026.2.24.
pub const PROTOCOL_VERSION: u32 = 3;

/// Maximum WebSocket payload size (25 MB).
pub const MAX_WS_PAYLOAD: usize = 25 * 1024 * 1024;

// ============================================================================
// OC-Compatible Frame Types (OpenClaw v2026.2.24 wire protocol)
// ============================================================================
//
// The bridge sends `type:"req"` and expects `type:"res"` with `ok`/`payload`
// fields and `type:"event"` with `event`/`payload` fields.

/// Incoming frame from bridge — accepts both `type:"req"` (OC) and
/// `type:"request"` (legacy MyLobster) for backwards compatibility.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum IncomingFrame {
    /// OC-format request: `{type:"req", id, method, params}`
    #[serde(rename = "req")]
    OcRequest(RequestFrame),
    /// Legacy MyLobster format: `{type:"request", id, method, params}`
    #[serde(rename = "request")]
    Request(RequestFrame),
}

impl IncomingFrame {
    pub fn into_request(self) -> RequestFrame {
        match self {
            IncomingFrame::OcRequest(r) | IncomingFrame::Request(r) => r,
        }
    }
}

/// A request from client to gateway (shared fields for both OC and legacy).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestFrame {
    pub id: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

/// OC-format response: `{type:"res", id, ok, payload?, error?}`
/// This is what the bridge expects.
#[derive(Debug, Clone, Serialize)]
pub struct OcResponseFrame {
    #[serde(rename = "type")]
    pub frame_type: &'static str, // always "res"
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<OcError>,
}

impl OcResponseFrame {
    pub fn success(id: String, payload: serde_json::Value) -> Self {
        Self {
            frame_type: "res",
            id,
            ok: true,
            payload: Some(payload),
            error: None,
        }
    }

    pub fn error(id: String, message: String, code: Option<i32>) -> Self {
        Self {
            frame_type: "res",
            id,
            ok: false,
            payload: None,
            error: Some(OcError {
                message,
                code: code.unwrap_or(-32603),
            }),
        }
    }
}

/// Error object in OC response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcError {
    pub message: String,
    #[serde(skip_serializing_if = "is_default_error_code")]
    pub code: i32,
}

fn is_default_error_code(code: &i32) -> bool {
    *code == 0
}

/// OC-format event: `{type:"event", event, payload?}`
#[derive(Debug, Clone, Serialize)]
pub struct OcEventFrame {
    #[serde(rename = "type")]
    pub frame_type: &'static str, // always "event"
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

impl OcEventFrame {
    pub fn new(event: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            frame_type: "event",
            event: event.into(),
            payload: Some(payload),
        }
    }
}

// ============================================================================
// Legacy Frame Types (kept for backwards compat / internal use)
// ============================================================================

/// A WebSocket frame in the legacy gateway protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Frame {
    #[serde(rename = "request")]
    Request(RequestFrame),
    #[serde(rename = "response")]
    Response(ResponseFrame),
    #[serde(rename = "event")]
    Event(EventFrame),
    #[serde(rename = "hello")]
    Hello(HelloFrame),
}

/// A legacy response from gateway to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseFrame {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ProtocolError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

/// A legacy unsolicited event from gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventFrame {
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

/// Initial handshake frame (unused in OC protocol — replaced by connect.challenge).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloFrame {
    pub protocol: u32,
    pub server: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub challenge: Option<String>,
}

/// A protocol error (legacy format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ProtocolError {}: {}", self.code, self.message)
    }
}

impl std::error::Error for ProtocolError {}

// ============================================================================
// Connect / Handshake Protocol (OC v2026.2.24)
// ============================================================================

/// Parameters sent in the `connect` request.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectParams {
    pub min_protocol: Option<u32>,
    pub max_protocol: Option<u32>,
    pub client: Option<ConnectClientInfo>,
    pub role: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub caps: Option<Vec<String>>,
    pub auth: Option<ConnectAuthField>,
    pub device: Option<DeviceParams>,
}

/// Client info sent during connect.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectClientInfo {
    pub id: Option<String>,
    pub display_name: Option<String>,
    pub version: Option<String>,
    pub platform: Option<String>,
    pub mode: Option<String>,
}

/// Auth field in connect request.
#[derive(Debug, Clone, Deserialize)]
pub struct ConnectAuthField {
    pub token: Option<String>,
    pub password: Option<String>,
}

/// Device identity params sent during connect handshake.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceParams {
    /// Device ID (SHA256 of raw public key).
    pub id: String,
    /// Raw 32-byte Ed25519 public key in base64url encoding.
    pub public_key: String,
    /// Ed25519 signature over the v2 payload in base64url encoding.
    pub signature: String,
    /// Timestamp when signature was created (milliseconds since epoch).
    pub signed_at: u64,
    /// Challenge nonce echoed back.
    pub nonce: String,
}

// ============================================================================
// Gateway Scopes
// ============================================================================

/// Scopes that control what a connection can do.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GatewayScope {
    OperatorAdmin,
    OperatorWrite,
    OperatorRead,
    OperatorPairing,
}

impl GatewayScope {
    /// Parse a scope string into a GatewayScope.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "operator.admin" => Some(Self::OperatorAdmin),
            "operator.write" => Some(Self::OperatorWrite),
            "operator.read" => Some(Self::OperatorRead),
            "operator.pairing" => Some(Self::OperatorPairing),
            _ => None,
        }
    }

    /// Resolve scope implications: admin implies write+read+pairing.
    pub fn resolve_scopes(requested: &[String]) -> Vec<GatewayScope> {
        let mut resolved = Vec::new();
        for s in requested {
            if let Some(scope) = Self::from_str(s) {
                match scope {
                    GatewayScope::OperatorAdmin => {
                        resolved.push(GatewayScope::OperatorAdmin);
                        resolved.push(GatewayScope::OperatorWrite);
                        resolved.push(GatewayScope::OperatorRead);
                        resolved.push(GatewayScope::OperatorPairing);
                    }
                    other => resolved.push(other),
                }
            }
        }
        resolved.sort_by_key(|s| format!("{:?}", s));
        resolved.dedup();
        resolved
    }
}

/// State tracked per WebSocket connection.
#[derive(Debug, Clone)]
pub struct ConnectionState {
    pub client_id: String,
    pub handshake_complete: bool,
    pub challenge_nonce: String,
    pub scopes: Vec<GatewayScope>,
    pub user_id: Option<String>,
    pub session_id: String,
}

// ============================================================================
// Chat Protocol
// ============================================================================

/// Parameters for sending a chat message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatSendParams {
    pub session_key: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deliver: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// When true, delivery failures are silently ignored (v2026.2.24).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_effort_deliver: Option<bool>,
}

/// Chat event streamed back to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatEvent {
    pub run_id: String,
    pub session_key: String,
    pub seq: u64,
    pub state: ChatEventState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatEventState {
    Delta,
    Final,
    Aborted,
    Error,
}

/// Token usage information.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
}

// ============================================================================
// Session Protocol
// ============================================================================

/// Session information.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: String,
    pub session_key: String,
    pub agent_id: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Parameters for listing sessions.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionListParams {
    pub agent_id: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

/// Parameters for updating a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPatchParams {
    pub session_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
}

// ============================================================================
// Channel Status Protocol
// ============================================================================

/// Channel status response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelStatusResponse {
    pub ts: u64,
    pub channel_order: Vec<String>,
    pub channel_labels: HashMap<String, String>,
    pub channel_detail_labels: Option<HashMap<String, String>>,
    pub channel_system_images: Option<HashMap<String, String>>,
    pub channel_meta: Option<Vec<serde_json::Value>>,
    pub channels: HashMap<String, serde_json::Value>,
    pub channel_accounts: HashMap<String, Vec<ChannelAccountSnapshot>>,
    pub channel_default_account_id: HashMap<String, String>,
}

/// Snapshot of a channel account's state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelAccountSnapshot {
    pub account_id: String,
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub configured: Option<bool>,
    pub connected: Option<bool>,
    pub last_connected_at: Option<u64>,
    pub last_error: Option<String>,
    pub mode: Option<String>,
    pub dm_policy: Option<String>,
    pub allow_from: Option<Vec<String>>,
    pub probe: Option<serde_json::Value>,
    pub audit: Option<serde_json::Value>,
}

// ============================================================================
// Connect Protocol
// ============================================================================

/// Hello response sent after successful auth.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloOk {
    pub protocol: u32,
    pub server: String,
    pub version: String,
    pub session_id: String,
    pub capabilities: Vec<String>,
}

// ============================================================================
// Gateway Info
// ============================================================================

/// Gateway information response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayInfo {
    pub version: String,
    pub protocol: u32,
    pub uptime_seconds: u64,
    pub sessions_active: u32,
    pub clients_connected: u32,
}

// ============================================================================
// Health
// ============================================================================

/// Health check response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub uptime: u64,
}

// ============================================================================
// OpenAI Compatibility
// ============================================================================

/// OpenAI-compatible chat completion request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatCompletionMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
}

/// A message in an OpenAI-compatible chat completion request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionMessage {
    pub role: String,
    pub content: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<serde_json::Value>>,
}

/// OpenAI-compatible chat completion response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChatCompletionChoice>,
    pub usage: Option<ChatCompletionUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionChoice {
    pub index: u32,
    pub message: ChatCompletionMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

/// OpenAI-compatible streaming delta.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChatCompletionChunkChoice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionChunkChoice {
    pub index: u32,
    pub delta: ChatCompletionDelta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<serde_json::Value>>,
}

// ============================================================================
// Tests — OpenClaw v2026.2.24 Protocol Parity
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ====================================================================
    // IncomingFrame: accepts both OC "req" and legacy "request"
    // ====================================================================

    #[test]
    fn parse_oc_req_frame() {
        let raw = r#"{"type":"req","id":"abc-123","method":"chat.send","params":{"sessionKey":"s1","message":"hi"}}"#;
        let frame: IncomingFrame = serde_json::from_str(raw).unwrap();
        let req = frame.into_request();
        assert_eq!(req.id, "abc-123");
        assert_eq!(req.method, "chat.send");
        assert!(req.params.is_some());
        let p = req.params.unwrap();
        assert_eq!(p["sessionKey"], "s1");
        assert_eq!(p["message"], "hi");
    }

    #[test]
    fn parse_legacy_request_frame() {
        let raw = r#"{"type":"request","id":"xyz","method":"sessions.list","params":{}}"#;
        let frame: IncomingFrame = serde_json::from_str(raw).unwrap();
        let req = frame.into_request();
        assert_eq!(req.id, "xyz");
        assert_eq!(req.method, "sessions.list");
    }

    #[test]
    fn reject_unknown_frame_type() {
        let raw = r#"{"type":"unknown","id":"1","method":"x"}"#;
        let result = serde_json::from_str::<IncomingFrame>(raw);
        assert!(result.is_err());
    }

    // ====================================================================
    // OcResponseFrame serialization — matches what bridge expects
    // ====================================================================

    #[test]
    fn oc_response_success_has_correct_fields() {
        let resp = OcResponseFrame::success("req-1".into(), json!({"runId": "run-1"}));
        let v: serde_json::Value = serde_json::to_value(&resp).unwrap();

        assert_eq!(v["type"], "res");
        assert_eq!(v["id"], "req-1");
        assert_eq!(v["ok"], true);
        assert_eq!(v["payload"]["runId"], "run-1");
        // error must be absent on success (not null)
        assert!(v.get("error").is_none());
    }

    #[test]
    fn oc_response_error_has_correct_fields() {
        let resp = OcResponseFrame::error("req-2".into(), "auth failed".into(), Some(1008));
        let v: serde_json::Value = serde_json::to_value(&resp).unwrap();

        assert_eq!(v["type"], "res");
        assert_eq!(v["id"], "req-2");
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["message"], "auth failed");
        assert_eq!(v["error"]["code"], 1008);
        // payload must be absent on error
        assert!(v.get("payload").is_none());
    }

    #[test]
    fn oc_response_roundtrips_as_json() {
        let resp = OcResponseFrame::success("x".into(), json!({"ok": true}));
        let s = serde_json::to_string(&resp).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed["type"], "res");
    }

    // ====================================================================
    // OcEventFrame serialization
    // ====================================================================

    #[test]
    fn oc_event_frame_has_correct_fields() {
        let event = OcEventFrame::new("connect.challenge", json!({"nonce": "abc"}));
        let v: serde_json::Value = serde_json::to_value(&event).unwrap();

        assert_eq!(v["type"], "event");
        assert_eq!(v["event"], "connect.challenge");
        assert_eq!(v["payload"]["nonce"], "abc");
    }

    #[test]
    fn oc_event_chat_wraps_chat_event() {
        let chat_event = ChatEvent {
            run_id: "run-1".into(),
            session_key: "user:u1:conv:c1".into(),
            seq: 0,
            state: ChatEventState::Delta,
            message: Some(json!({
                "content": [{"type": "text", "text": "Hello"}]
            })),
            error_message: None,
            usage: None,
            stop_reason: None,
        };
        let event = OcEventFrame::new("chat", serde_json::to_value(&chat_event).unwrap());
        let v: serde_json::Value = serde_json::to_value(&event).unwrap();

        assert_eq!(v["type"], "event");
        assert_eq!(v["event"], "chat");
        assert_eq!(v["payload"]["runId"], "run-1");
        assert_eq!(v["payload"]["state"], "delta");
        // Bridge reads content[0].text
        assert_eq!(v["payload"]["message"]["content"][0]["text"], "Hello");
    }

    // ====================================================================
    // ConnectParams deserialization
    // ====================================================================

    #[test]
    fn parse_connect_params_full() {
        let raw = json!({
            "minProtocol": 3,
            "maxProtocol": 3,
            "client": {
                "id": "gateway-client",
                "displayName": "Bridge",
                "version": "1.0.0",
                "platform": "linux",
                "mode": "backend"
            },
            "role": "operator",
            "scopes": ["operator.admin", "operator.write"],
            "caps": ["tool-events"],
            "auth": { "token": "secret-token-123" },
            "device": {
                "id": "abcdef0123456789",
                "publicKey": "dGVzdC1rZXktYmFzZTY0dXJsLWVuY29kZWQ",
                "signature": "c2lnbmF0dXJlLWJhc2U2NHVybC1lbmNvZGVk",
                "signedAt": 1708867200000u64,
                "nonce": "challenge-nonce-xyz"
            }
        });

        let params: ConnectParams = serde_json::from_value(raw).unwrap();
        assert_eq!(params.min_protocol, Some(3));
        assert_eq!(params.max_protocol, Some(3));
        assert_eq!(params.role.as_deref(), Some("operator"));
        assert_eq!(params.scopes.as_ref().unwrap().len(), 2);
        assert_eq!(params.caps.as_ref().unwrap(), &["tool-events"]);

        let client = params.client.unwrap();
        assert_eq!(client.id.as_deref(), Some("gateway-client"));
        assert_eq!(client.mode.as_deref(), Some("backend"));

        let auth = params.auth.unwrap();
        assert_eq!(auth.token.as_deref(), Some("secret-token-123"));

        let device = params.device.unwrap();
        assert_eq!(device.id, "abcdef0123456789");
        assert_eq!(device.nonce, "challenge-nonce-xyz");
        assert_eq!(device.signed_at, 1708867200000);
    }

    #[test]
    fn parse_connect_params_minimal() {
        let raw = json!({"auth": {"token": "t"}});
        let params: ConnectParams = serde_json::from_value(raw).unwrap();
        assert!(params.auth.is_some());
        assert!(params.device.is_none());
        assert!(params.client.is_none());
        assert!(params.scopes.is_none());
    }

    // ====================================================================
    // GatewayScope resolution
    // ====================================================================

    #[test]
    fn scope_admin_implies_all() {
        let scopes = GatewayScope::resolve_scopes(&["operator.admin".into()]);
        assert!(scopes.contains(&GatewayScope::OperatorAdmin));
        assert!(scopes.contains(&GatewayScope::OperatorWrite));
        assert!(scopes.contains(&GatewayScope::OperatorRead));
        assert!(scopes.contains(&GatewayScope::OperatorPairing));
    }

    #[test]
    fn scope_write_does_not_imply_admin() {
        let scopes = GatewayScope::resolve_scopes(&["operator.write".into()]);
        assert!(scopes.contains(&GatewayScope::OperatorWrite));
        assert!(!scopes.contains(&GatewayScope::OperatorAdmin));
        assert!(!scopes.contains(&GatewayScope::OperatorRead));
    }

    #[test]
    fn scope_deduplication() {
        let scopes = GatewayScope::resolve_scopes(&[
            "operator.admin".into(),
            "operator.write".into(),
        ]);
        assert_eq!(scopes.len(), 4);
    }

    #[test]
    fn scope_unknown_ignored() {
        let scopes = GatewayScope::resolve_scopes(&["operator.unknown".into()]);
        assert!(scopes.is_empty());
    }

    #[test]
    fn scope_empty_input() {
        let scopes = GatewayScope::resolve_scopes(&[]);
        assert!(scopes.is_empty());
    }

    // ====================================================================
    // ChatSendParams — camelCase deserialization
    // ====================================================================

    #[test]
    fn parse_chat_send_params() {
        let raw = json!({
            "sessionKey": "user:u1:conv:c1",
            "message": "hello world",
            "idempotencyKey": "idem-123"
        });
        let params: ChatSendParams = serde_json::from_value(raw).unwrap();
        assert_eq!(params.session_key, "user:u1:conv:c1");
        assert_eq!(params.message, "hello world");
        assert_eq!(params.idempotency_key.as_deref(), Some("idem-123"));
    }

    #[test]
    fn parse_chat_send_params_shared_mode_session_key() {
        let raw = json!({
            "sessionKey": "user:abc-def-123:conv:conv-456-789",
            "message": "test"
        });
        let params: ChatSendParams = serde_json::from_value(raw).unwrap();
        assert!(params.session_key.starts_with("user:"));
        assert!(params.session_key.contains(":conv:"));
    }

    // ====================================================================
    // ChatEvent serialization — content blocks format
    // ====================================================================

    #[test]
    fn chat_event_delta_uses_camel_case_fields() {
        let event = ChatEvent {
            run_id: "r1".into(),
            session_key: "s1".into(),
            seq: 5,
            state: ChatEventState::Delta,
            message: Some(json!({"content": [{"type": "text", "text": "hi"}]})),
            error_message: None,
            usage: None,
            stop_reason: None,
        };
        let v = serde_json::to_value(&event).unwrap();

        // Bridge reads these exact camelCase field names
        assert_eq!(v["runId"], "r1");
        assert_eq!(v["sessionKey"], "s1");
        assert_eq!(v["seq"], 5);
        assert_eq!(v["state"], "delta");
        assert_eq!(v["message"]["content"][0]["text"], "hi");
        // Absent fields must be omitted, not null
        assert!(v.get("errorMessage").is_none());
        assert!(v.get("usage").is_none());
        assert!(v.get("stopReason").is_none());
    }

    #[test]
    fn chat_event_final_includes_usage_and_stop_reason() {
        let event = ChatEvent {
            run_id: "r1".into(),
            session_key: "s1".into(),
            seq: 10,
            state: ChatEventState::Final,
            message: Some(json!({"content": [{"type": "text", "text": "done"}]})),
            error_message: None,
            usage: Some(TokenUsage {
                input_tokens: Some(100),
                output_tokens: Some(50),
                cache_read_tokens: None,
                cache_write_tokens: None,
            }),
            stop_reason: Some("end_turn".into()),
        };
        let v = serde_json::to_value(&event).unwrap();

        assert_eq!(v["state"], "final");
        assert_eq!(v["stopReason"], "end_turn");
        assert_eq!(v["usage"]["inputTokens"], 100);
        assert_eq!(v["usage"]["outputTokens"], 50);
        assert!(v["usage"].get("cacheReadTokens").is_none());
    }

    #[test]
    fn chat_event_error_has_error_message() {
        let event = ChatEvent {
            run_id: "r1".into(),
            session_key: "s1".into(),
            seq: 0,
            state: ChatEventState::Error,
            message: None,
            error_message: Some("Provider error: rate limited".into()),
            usage: None,
            stop_reason: None,
        };
        let v = serde_json::to_value(&event).unwrap();
        assert_eq!(v["state"], "error");
        assert_eq!(v["errorMessage"], "Provider error: rate limited");
        assert!(v.get("message").is_none());
    }

    #[test]
    fn chat_event_aborted_state() {
        let event = ChatEvent {
            run_id: "r1".into(),
            session_key: "s1".into(),
            seq: 0,
            state: ChatEventState::Aborted,
            message: None,
            error_message: Some("cancelled".into()),
            usage: None,
            stop_reason: None,
        };
        let v = serde_json::to_value(&event).unwrap();
        assert_eq!(v["state"], "aborted");
    }

    // ====================================================================
    // TokenUsage camelCase
    // ====================================================================

    #[test]
    fn token_usage_uses_camel_case() {
        let usage = TokenUsage {
            input_tokens: Some(10),
            output_tokens: Some(20),
            cache_read_tokens: Some(5),
            cache_write_tokens: Some(3),
        };
        let v = serde_json::to_value(&usage).unwrap();
        assert!(v.get("inputTokens").is_some());
        assert!(v.get("outputTokens").is_some());
        assert!(v.get("cacheReadTokens").is_some());
        // Must NOT have snake_case
        assert!(v.get("input_tokens").is_none());
        assert!(v.get("output_tokens").is_none());
    }

    // ====================================================================
    // Protocol version
    // ====================================================================

    #[test]
    fn protocol_version_matches_openclaw_v2026_2_24() {
        assert_eq!(PROTOCOL_VERSION, 3);
    }

    // ====================================================================
    // Full connect.challenge -> hello-ok round trip
    // ====================================================================

    #[test]
    fn connect_challenge_event_format() {
        let event = OcEventFrame::new(
            "connect.challenge",
            json!({"nonce": "test-nonce-123", "ts": 1708867200000u64}),
        );
        let s = serde_json::to_string(&event).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();

        assert_eq!(v["type"], "event");
        assert_eq!(v["event"], "connect.challenge");
        assert_eq!(v["payload"]["nonce"], "test-nonce-123");
        assert!(v["payload"]["ts"].is_number());
    }

    #[test]
    fn hello_ok_response_format() {
        let resp = OcResponseFrame::success(
            "connect-id".into(),
            json!({
                "protocol": PROTOCOL_VERSION,
                "server": "mylobster",
                "version": "2026.2.22",
                "sessionId": "sess-uuid",
            }),
        );
        let v = serde_json::to_value(&resp).unwrap();

        assert_eq!(v["type"], "res");
        assert_eq!(v["ok"], true);
        assert_eq!(v["payload"]["protocol"], 3);
        assert_eq!(v["payload"]["server"], "mylobster");
        assert!(v["payload"]["sessionId"].is_string());
    }

    // ====================================================================
    // SessionPatchParams camelCase
    // ====================================================================

    #[test]
    fn session_patch_params_camel_case() {
        let raw = json!({
            "sessionKey": "s1",
            "title": "New Title",
            "model": "claude-opus-4-6"
        });
        let params: SessionPatchParams = serde_json::from_value(raw).unwrap();
        assert_eq!(params.session_key, "s1");
        assert_eq!(params.title.as_deref(), Some("New Title"));
        assert_eq!(params.model.as_deref(), Some("claude-opus-4-6"));
    }

    // ====================================================================
    // v2026.2.24 — ChatSendParams.bestEffortDeliver
    // ====================================================================

    #[test]
    fn chat_send_params_best_effort_deliver() {
        let raw = json!({
            "sessionKey": "s1",
            "message": "hello",
            "bestEffortDeliver": true
        });
        let params: ChatSendParams = serde_json::from_value(raw).unwrap();
        assert_eq!(params.best_effort_deliver, Some(true));
    }

    #[test]
    fn chat_send_params_best_effort_deliver_absent() {
        let raw = json!({
            "sessionKey": "s1",
            "message": "hello"
        });
        let params: ChatSendParams = serde_json::from_value(raw).unwrap();
        assert!(params.best_effort_deliver.is_none());
    }

    #[test]
    fn chat_send_params_best_effort_deliver_roundtrip() {
        let params = ChatSendParams {
            session_key: "s1".into(),
            message: "hi".into(),
            thinking: None,
            deliver: None,
            attachments: None,
            timeout_ms: None,
            idempotency_key: None,
            best_effort_deliver: Some(true),
        };
        let v = serde_json::to_value(&params).unwrap();
        assert_eq!(v["bestEffortDeliver"], true);

        let back: ChatSendParams = serde_json::from_value(v).unwrap();
        assert_eq!(back.best_effort_deliver, Some(true));
    }

    #[test]
    fn chat_send_params_best_effort_deliver_skipped_when_none() {
        let params = ChatSendParams {
            session_key: "s1".into(),
            message: "hi".into(),
            thinking: None,
            deliver: None,
            attachments: None,
            timeout_ms: None,
            idempotency_key: None,
            best_effort_deliver: None,
        };
        let v = serde_json::to_value(&params).unwrap();
        assert!(v.get("bestEffortDeliver").is_none());
    }
}
