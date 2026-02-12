use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Protocol version for WebSocket communication.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum WebSocket payload size (25 MB).
pub const MAX_WS_PAYLOAD: usize = 25 * 1024 * 1024;

// ============================================================================
// Frame Types
// ============================================================================

/// A WebSocket frame in the gateway protocol.
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

/// A request from client to gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestFrame {
    pub id: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

/// A response from gateway to client.
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

/// An unsolicited event from gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventFrame {
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

/// Initial handshake frame.
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

/// A protocol error.
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

/// Authentication sent during WebSocket connect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectAuth {
    pub token: Option<String>,
    pub password: Option<String>,
}

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
