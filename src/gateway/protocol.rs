use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Protocol version for WebSocket communication.
/// Version 3 matches OpenClaw v2026.2.22.
pub const PROTOCOL_VERSION: u32 = 3;

/// Maximum WebSocket payload size (25 MB).
pub const MAX_WS_PAYLOAD: usize = 25 * 1024 * 1024;

// ============================================================================
// OC-Compatible Frame Types (OpenClaw v2026.2.22 wire protocol)
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
// Connect / Handshake Protocol (OC v2026.2.22)
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
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
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
