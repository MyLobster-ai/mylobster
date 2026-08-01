//! Shared types and functions for OpenAI-compatible APIs.
//!
//! Used by OpenAI, Groq, and other providers that implement the
//! OpenAI chat completions API format.

use super::{ContentBlock, ProviderMessage, ProviderRequest, ProviderResponse, StreamEvent};
use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

// ============================================================================
// OpenAI-Compatible API Types
// ============================================================================

#[derive(Debug, Serialize)]
pub(crate) struct OpenAiRequest {
    pub model: String,
    pub messages: Vec<OpenAiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OpenAiMessage {
    pub role: String,
    pub content: serde_json::Value,
    /// v2026.6.x: model refusals surface as assistant text instead of being
    /// dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiResponse {
    pub choices: Vec<OpenAiChoice>,
    pub usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiChoice {
    pub message: OpenAiMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiUsage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    /// v2026.6.x: cached prompt tokens (input_tokens - cached must clamp ≥0).
    #[serde(default)]
    pub prompt_tokens_details: Option<OpenAiPromptTokensDetails>,
    /// v2026.6.x: reasoning tokens reported without double-counting output.
    #[serde(default)]
    pub completion_tokens_details: Option<OpenAiCompletionTokensDetails>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiPromptTokensDetails {
    pub cached_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiCompletionTokensDetails {
    pub reasoning_tokens: Option<u64>,
}

/// Clamp `input_tokens - cached_tokens` at zero (v2026.6.x: some providers
/// report cached counts exceeding prompt counts; the uncached remainder must
/// never underflow).
pub fn clamp_uncached_input_tokens(input_tokens: u64, cached_tokens: u64) -> u64 {
    input_tokens.saturating_sub(cached_tokens)
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiStreamChunk {
    pub choices: Vec<OpenAiStreamChoice>,
    pub usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiStreamChoice {
    pub delta: OpenAiStreamDelta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiStreamDelta {
    pub content: Option<String>,
    #[serde(default)]
    pub refusal: Option<String>,
    pub tool_calls: Option<Vec<serde_json::Value>>,
}

// ============================================================================
// Shared Functions
// ============================================================================

/// Convert ProviderMessages to OpenAI format.
pub(crate) fn convert_messages(messages: Vec<ProviderMessage>) -> Vec<OpenAiMessage> {
    messages
        .into_iter()
        .map(|m| OpenAiMessage {
            role: m.role,
            content: m.content,
            refusal: None,
            name: m.name,
            tool_call_id: m.tool_call_id,
            tool_calls: m.tool_calls,
        })
        .collect()
}

/// Normalize parameter-free tool schemas before OpenAI submission
/// (v2026.5.2, issue #75362): MCP tools whose top-level object `parameters`
/// (`properties`) is missing, null, or invalid would otherwise be rejected by
/// OpenAI. Any function tool whose `parameters` is not an object schema — or
/// is an object schema whose `properties` is missing/null/invalid — gets a
/// canonical empty object schema.
pub(crate) fn normalize_parameter_free_tool_schemas(tools: &mut [serde_json::Value]) {
    for tool in tools.iter_mut() {
        let Some(function) = tool.get_mut("function").and_then(|f| f.as_object_mut()) else {
            continue;
        };
        let needs_normalization = match function.get("parameters") {
            None | Some(serde_json::Value::Null) => true,
            Some(serde_json::Value::Object(params)) => {
                let type_ok = matches!(
                    params.get("type").and_then(|t| t.as_str()),
                    Some("object")
                );
                let properties_ok =
                    matches!(params.get("properties"), Some(serde_json::Value::Object(_)));
                !type_ok || !properties_ok
            }
            Some(_) => true,
        };
        if needs_normalization {
            // Preserve declared required entries only when the schema stays
            // parameter-free — an invalid schema collapses to no params.
            function.insert(
                "parameters".to_string(),
                serde_json::json!({"type": "object", "properties": {}}),
            );
        }
    }
}

/// Deterministic tool-payload ordering (v2026.7.1): stable-sort tool
/// definitions by function name so serialized tool payloads are byte-stable
/// across turns, keeping provider prompt caches warm.
pub(crate) fn sort_tools_deterministic(tools: &mut [serde_json::Value]) {
    tools.sort_by(|a, b| {
        let name = |t: &serde_json::Value| {
            t.pointer("/function/name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string()
        };
        name(a).cmp(&name(b))
    });
}

/// Build an OpenAI-compatible request body.
pub(crate) fn build_request(request: ProviderRequest, stream: bool) -> OpenAiRequest {
    let tools = request.tools.map(|mut tools| {
        normalize_parameter_free_tool_schemas(&mut tools);
        sort_tools_deterministic(&mut tools);
        tools
    });
    OpenAiRequest {
        model: request.model,
        messages: convert_messages(request.messages),
        max_tokens: request.max_tokens,
        temperature: request.temperature,
        stream: if stream { Some(true) } else { None },
        tools,
        tool_choice: request.tool_choice,
    }
}

// ============================================================================
// Streaming tool-call argument accumulation (v2026.6.x: parallel tool-call
// argument buffers must be separated by choice index)
// ============================================================================

/// Accumulates streamed tool-call fragments into complete tool calls, keyed
/// by the wire `index` so parallel tool calls never interleave their
/// argument buffers.
#[derive(Debug, Default)]
pub struct ToolCallAccumulator {
    calls: std::collections::BTreeMap<u64, AccumulatedToolCall>,
}

#[derive(Debug, Default, Clone)]
struct AccumulatedToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl ToolCallAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push one streamed tool-call delta fragment.
    pub fn push(&mut self, fragment: &serde_json::Value) {
        let index = fragment.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
        let entry = self.calls.entry(index).or_default();
        if let Some(id) = fragment.get("id").and_then(|v| v.as_str()) {
            if !id.is_empty() {
                entry.id = Some(id.to_string());
            }
        }
        if let Some(name) = fragment.pointer("/function/name").and_then(|v| v.as_str()) {
            if !name.is_empty() {
                entry.name = Some(name.to_string());
            }
        }
        if let Some(args) = fragment.pointer("/function/arguments").and_then(|v| v.as_str()) {
            entry.arguments.push_str(args);
        }
    }

    /// Finish accumulation, returning complete tool calls in index order.
    pub fn finish(self) -> Vec<serde_json::Value> {
        self.calls
            .into_iter()
            .map(|(index, call)| {
                serde_json::json!({
                    "index": index,
                    "id": call.id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": if call.arguments.is_empty() {
                            "{}".to_string()
                        } else {
                            call.arguments
                        },
                    }
                })
            })
            .collect()
    }
}

// ============================================================================
// Azure OpenAI Responses defaults (v2026.5.14)
// ============================================================================

/// Default Azure OpenAI API version when unset (`preview` routes through the
/// current `/openai/v1/responses` surface).
pub const AZURE_OPENAI_DEFAULT_API_VERSION: &str = "preview";

/// Resolve the Azure OpenAI API version, defaulting unset/blank to
/// `preview`.
pub fn resolve_azure_api_version(configured: Option<&str>) -> &str {
    configured
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(AZURE_OPENAI_DEFAULT_API_VERSION)
}

/// Build the Azure OpenAI Responses URL for a resource base URL. The
/// `preview` version routes through `/openai/v1/responses`; pinned GA
/// versions keep the legacy query-versioned route.
pub fn azure_responses_url(base_url: &str, api_version: Option<&str>) -> String {
    let base = base_url.trim_end_matches('/');
    let version = resolve_azure_api_version(api_version);
    if version == AZURE_OPENAI_DEFAULT_API_VERSION {
        format!("{}/openai/v1/responses?api-version={}", base, version)
    } else {
        format!("{}/openai/responses?api-version={}", base, version)
    }
}

// ============================================================================
// Compat capability knobs (v2026.5.x–6.x)
// ============================================================================

/// Per-model OpenAI-compat capability knobs (subset of upstream
/// `compat`): schema-strict providers declare what they cannot accept and
/// the transport strips accordingly.
#[derive(Debug, Clone, Default)]
pub struct CompatKnobs {
    /// Reasoning wire format (`openai`/`openrouter`/`deepseek`/`together`/
    /// `zai`/`qwen`/`qwen-chat-template`).
    pub thinking_format: Option<String>,
    /// Strip messages down to `role` + `content` (+`tool_call_id`/`tool_calls`).
    pub strict_message_keys: bool,
    /// `false` strips tool payloads entirely before submission.
    pub supports_tools: bool,
    /// JSON-schema keywords the model rejects; stripped recursively.
    pub unsupported_schema_keywords: Vec<String>,
    /// Opt into `stream_options.include_usage` (Volcengine/Ark reject it by
    /// default).
    pub include_usage_opt_in: bool,
}

impl CompatKnobs {
    pub fn permissive() -> Self {
        Self {
            thinking_format: None,
            strict_message_keys: false,
            supports_tools: true,
            unsupported_schema_keywords: Vec::new(),
            include_usage_opt_in: false,
        }
    }
}

const STRICT_MESSAGE_KEYS: &[&str] = &["role", "content", "tool_call_id", "tool_calls", "name"];

/// `compat.strictMessageKeys`: strip completion messages down to the
/// role/content core so schema-strict providers do not reject extra keys.
pub fn strip_messages_to_role_content(messages: &mut [serde_json::Value]) {
    for msg in messages.iter_mut() {
        let Some(obj) = msg.as_object_mut() else {
            continue;
        };
        obj.retain(|key, _| STRICT_MESSAGE_KEYS.contains(&key.as_str()));
    }
}

/// `compat.supportsTools: false`: strip tool payloads before submission.
pub fn strip_tool_payloads(request: &mut ProviderRequest) {
    request.tools = None;
    request.tool_choice = None;
}

fn strip_keywords_recursive(value: &mut serde_json::Value, keywords: &[String]) {
    match value {
        serde_json::Value::Object(map) => {
            map.retain(|key, _| !keywords.iter().any(|k| k == key));
            for child in map.values_mut() {
                strip_keywords_recursive(child, keywords);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                strip_keywords_recursive(item, keywords);
            }
        }
        _ => {}
    }
}

/// Model-declared unsupported-schema-keyword stripping: recursively remove
/// rejected JSON-schema keywords (e.g. `minLength`, `format`) from tool
/// parameter schemas.
pub fn strip_unsupported_schema_keywords(
    tools: &mut [serde_json::Value],
    keywords: &[String],
) {
    if keywords.is_empty() {
        return;
    }
    for tool in tools.iter_mut() {
        if let Some(params) = tool.pointer_mut("/function/parameters") {
            strip_keywords_recursive(params, keywords);
        }
    }
}

// ============================================================================
// Reasoning-effort / schema hardening shared helpers (v2026.5.x–6.x)
// ============================================================================

/// Shared thinking-level → `reasoning_effort` normalization: maps a
/// requested level onto the provider's supported set, downgrading/upgrading
/// to the nearest supported effort (`max` → `xhigh` → `high` …). Returns
/// `None` for `off` or when nothing is supported.
pub fn normalize_reasoning_effort(level: &str, supported: &[&str]) -> Option<String> {
    const ORDER: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
    let level = level.trim().to_ascii_lowercase();
    if level.is_empty() || level == "off" || supported.is_empty() {
        return None;
    }
    if supported.iter().any(|s| s.eq_ignore_ascii_case(&level)) {
        return Some(level);
    }
    let requested_rank = ORDER.iter().position(|l| *l == level)?;
    // Walk down from the requested rank to the highest supported level.
    for rank in (0..=requested_rank).rev() {
        if supported.iter().any(|s| s.eq_ignore_ascii_case(ORDER[rank])) {
            return Some(ORDER[rank].to_string());
        }
    }
    // Nothing lower is supported; take the lowest supported level.
    ORDER
        .iter()
        .find(|l| supported.iter().any(|s| s.eq_ignore_ascii_case(l)))
        .map(|l| l.to_string())
}

/// DeepSeek schema hardening (v2026.5.x): collapse `anyOf: [<schema>,
/// {type: "null"}]` pairs into the base schema so schema-strict DeepSeek
/// deployments accept optional-parameter tools.
pub fn normalize_anyof_schemas(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let collapsed = map.get("anyOf").and_then(|any_of| {
                let items = any_of.as_array()?;
                if items.len() != 2 {
                    return None;
                }
                let is_null = |v: &serde_json::Value| {
                    v.get("type").and_then(|t| t.as_str()) == Some("null")
                };
                match (is_null(&items[0]), is_null(&items[1])) {
                    (false, true) => Some(items[0].clone()),
                    (true, false) => Some(items[1].clone()),
                    _ => None,
                }
            });
            if let Some(serde_json::Value::Object(base)) = collapsed {
                map.remove("anyOf");
                for (k, v) in base {
                    map.entry(k).or_insert(v);
                }
            }
            for child in map.values_mut() {
                normalize_anyof_schemas(child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                normalize_anyof_schemas(item);
            }
        }
        _ => {}
    }
}

// ============================================================================
// Generic OpenAI-compatible embeddings (v2026.6.2 core embedding provider)
// ============================================================================

/// Embed texts through any OpenAI-compatible `/embeddings` endpoint. This is
/// the provider-side core of the v2026.6.2 "core OpenAI-compatible embedding
/// provider" — `memory/embeddings.rs` integration is the memory cluster's
/// half.
pub async fn openai_compat_embed(
    client: &Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    texts: &[String],
    provider_name: &str,
) -> Result<Vec<Vec<f32>>> {
    let resp = client
        .post(format!("{}/embeddings", base_url.trim_end_matches('/')))
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": model,
            "input": texts,
            "encoding_format": "float",
        }))
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("{} embeddings API error ({}): {}", provider_name, status, text);
    }
    let payload: serde_json::Value = resp.json().await?;
    let rows = payload
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| anyhow::anyhow!("{}: malformed embeddings response", provider_name))?;
    let mut indexed: Vec<(usize, Vec<f32>)> = Vec::new();
    for (fallback_index, row) in rows.iter().enumerate() {
        let index = row
            .get("index")
            .and_then(|i| i.as_u64())
            .map(|i| i as usize)
            .unwrap_or(fallback_index);
        let embedding = row
            .get("embedding")
            .and_then(|e| e.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_f64().map(|f| f as f32))
                    .collect::<Vec<f32>>()
            })
            .unwrap_or_default();
        indexed.push((index, embedding));
    }
    indexed.sort_by_key(|(i, _)| *i);
    Ok(indexed.into_iter().map(|(_, e)| e).collect())
}

// ============================================================================
// OpenAI-compatible TTS (v2026.5.2 extraBody passthrough, issue #39900)
// ============================================================================

/// Build a `/audio/speech` request body for OpenAI-compatible TTS endpoints.
///
/// `extra_body` fields are spread into the body last (after the canonical
/// fields), so custom speech servers can receive provider-specific fields
/// such as `lang`. Prototype-pollution-style keys are dropped for parity with
/// the upstream sanitizer.
///
/// NOTE (cross-cluster handoff): this is the provider-side plumbing only —
/// the TTS pipeline (voice-note queueing, directives) is owned elsewhere and
/// should call `build_speech_request_body` + `openai_compat_speech`.
pub fn build_speech_request_body(
    model: &str,
    input: &str,
    voice: &str,
    response_format: Option<&str>,
    speed: Option<f64>,
    instructions: Option<&str>,
    extra_body: Option<&serde_json::Map<String, serde_json::Value>>,
) -> serde_json::Value {
    let mut body = serde_json::Map::new();
    body.insert("model".to_string(), serde_json::Value::String(model.to_string()));
    body.insert("input".to_string(), serde_json::Value::String(input.to_string()));
    body.insert("voice".to_string(), serde_json::Value::String(voice.to_string()));
    if let Some(format) = response_format {
        body.insert(
            "response_format".to_string(),
            serde_json::Value::String(format.to_string()),
        );
    }
    if let Some(speed) = speed {
        if let Some(number) = serde_json::Number::from_f64(speed) {
            body.insert("speed".to_string(), serde_json::Value::Number(number));
        }
    }
    if let Some(instructions) = instructions {
        body.insert(
            "instructions".to_string(),
            serde_json::Value::String(instructions.to_string()),
        );
    }
    if let Some(extra) = extra_body {
        for (key, value) in extra {
            if key == "__proto__" || key == "constructor" || key == "prototype" {
                continue;
            }
            body.insert(key.clone(), value.clone());
        }
    }
    serde_json::Value::Object(body)
}

/// POST a speech request to an OpenAI-compatible `/audio/speech` endpoint and
/// return the audio bytes.
pub async fn openai_compat_speech(
    client: &Client,
    base_url: &str,
    api_key: &str,
    body: &serde_json::Value,
    provider_name: &str,
) -> Result<Vec<u8>> {
    let resp = client
        .post(format!("{}/audio/speech", base_url.trim_end_matches('/')))
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("{} TTS API error ({}): {}", provider_name, status, text);
    }
    Ok(resp.bytes().await?.to_vec())
}

/// Parse an OpenAI-compatible response into our ProviderResponse.
pub(crate) fn parse_openai_response(api_resp: OpenAiResponse) -> Result<ProviderResponse> {
    let choice = api_resp
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No choices in response"))?;

    let mut content = Vec::new();

    if let Some(text) = choice.message.content.as_str() {
        if !text.is_empty() {
            content.push(ContentBlock::Text(text.to_string()));
        }
    }

    // v2026.6.x: surface refusals as assistant text so the turn is visible
    // instead of coming back empty.
    if content.is_empty() {
        if let Some(refusal) = choice.message.refusal.as_deref() {
            if !refusal.is_empty() {
                content.push(ContentBlock::Text(refusal.to_string()));
            }
        }
    }

    if let Some(tool_calls) = choice.message.tool_calls {
        for tc in tool_calls {
            if let (Some(id), Some(function)) =
                (tc.get("id").and_then(|v| v.as_str()), tc.get("function"))
            {
                let name = function
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let arguments = function
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}");
                let input: serde_json::Value = serde_json::from_str(arguments)
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                content.push(ContentBlock::ToolUse {
                    id: id.to_string(),
                    name,
                    input,
                });
            }
        }
    }

    let usage = api_resp.usage.unwrap_or(OpenAiUsage {
        prompt_tokens: None,
        completion_tokens: None,
        prompt_tokens_details: None,
        completion_tokens_details: None,
    });

    let cached_tokens = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|d| d.cached_tokens);
    // v2026.6.x: uncached input = prompt - cached, clamped at zero.
    let input_tokens = match (usage.prompt_tokens, cached_tokens) {
        (Some(prompt), Some(cached)) => Some(clamp_uncached_input_tokens(prompt, cached)),
        (prompt, _) => prompt,
    };

    Ok(ProviderResponse {
        content,
        stop_reason: choice.finish_reason,
        usage: crate::gateway::TokenUsage {
            input_tokens,
            output_tokens: usage.completion_tokens,
            cache_read_tokens: cached_tokens,
            cache_write_tokens: None,
        },
    })
}

/// Make a non-streaming chat request to an OpenAI-compatible endpoint.
pub(crate) async fn openai_compat_chat(
    client: &Client,
    base_url: &str,
    api_key: &str,
    request: ProviderRequest,
    provider_name: &str,
) -> Result<ProviderResponse> {
    let body = build_request(request, false);

    let resp = client
        .post(format!("{}/chat/completions", base_url))
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .header("User-Agent", "MyLobster/2026.4.1")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("{} API error ({}): {}", provider_name, status, text);
    }

    let api_resp: OpenAiResponse = resp.json().await?;
    parse_openai_response(api_resp)
}

/// Make a streaming chat request to an OpenAI-compatible endpoint.
pub(crate) async fn openai_compat_stream_chat(
    client: &Client,
    base_url: &str,
    api_key: &str,
    request: ProviderRequest,
    provider_name: &str,
) -> Result<mpsc::Receiver<StreamEvent>> {
    let (tx, rx) = mpsc::channel(256);

    let body = build_request(request, true);

    let client = client.clone();
    let base_url = base_url.to_string();
    let api_key = api_key.to_string();
    let provider_name = provider_name.to_string();

    tokio::spawn(async move {
        let resp = match client
            .post(format!("{}/chat/completions", base_url))
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .header("User-Agent", "MyLobster/2026.4.1")
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let _ = tx
                    .send(StreamEvent::Error(format!("Request failed: {}", e)))
                    .await;
                return;
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let _ = tx
                .send(StreamEvent::Error(format!(
                    "{} API error ({}): {}",
                    provider_name, status, text
                )))
                .await;
            return;
        }

        let text = match resp.text().await {
            Ok(t) => t,
            Err(e) => {
                let _ = tx
                    .send(StreamEvent::Error(format!(
                        "Failed to read response: {}",
                        e
                    )))
                    .await;
                return;
            }
        };

        let mut total_input = None;
        let mut total_output = None;

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            // Replay/streaming safety (v2026.7.1, #96503): some non-conforming
            // OpenAI-compatible providers return event streams mislabeled as
            // JSON — chunk lines arrive without the `data: ` prefix. Accept
            // bare JSON chunk lines instead of dropping the whole stream.
            let data = match line.strip_prefix("data: ") {
                Some(data) => data,
                None if line.starts_with('{') || line == "[DONE]" => line,
                None => continue,
            };
            if data == "[DONE]" {
                break;
            }
            match serde_json::from_str::<OpenAiStreamChunk>(data) {
                Ok(chunk) => {
                    if let Some(usage) = chunk.usage {
                        total_input = usage.prompt_tokens;
                        total_output = usage.completion_tokens;
                    }
                    for choice in chunk.choices {
                        if let Some(content) = choice.delta.content {
                            if !content.is_empty() {
                                let _ = tx.send(StreamEvent::Delta(content)).await;
                            }
                        }
                        // v2026.6.x: a refusal arrives on its own delta field
                        // with `content` null. Surface it as assistant text —
                        // matching the non-streaming path — otherwise a refused
                        // turn streams as a completely empty reply.
                        if let Some(refusal) = choice.delta.refusal {
                            if !refusal.is_empty() {
                                let _ = tx.send(StreamEvent::Delta(refusal)).await;
                            }
                        }
                        if let Some(tool_calls) = choice.delta.tool_calls {
                            for tc in tool_calls {
                                let _ = tx.send(StreamEvent::ToolCall(tc)).await;
                            }
                        }
                    }
                }
                Err(_) => {
                    // Skip unparseable chunks
                }
            }
        }

        let _ = tx
            .send(StreamEvent::Done(crate::gateway::TokenUsage {
                input_tokens: total_input,
                output_tokens: total_output,
                cache_read_tokens: None,
                cache_write_tokens: None,
            }))
            .await;
    });

    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ------------------------------------------------------------------------
    // Test helpers
    // ------------------------------------------------------------------------

    fn user_msg(text: &str) -> ProviderMessage {
        ProviderMessage {
            role: "user".to_string(),
            content: serde_json::Value::String(text.to_string()),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn req(model: &str) -> ProviderRequest {
        ProviderRequest {
            model: model.to_string(),
            messages: vec![user_msg("hi")],
            max_tokens: None,
            temperature: None,
            stream: false,
            tools: None,
            tool_choice: None,
            thinking: None,
        }
    }

    fn ok_response_json() -> serde_json::Value {
        json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hello"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        })
    }

    async fn mock_with_body(body: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    async fn captured(
        server: &MockServer,
    ) -> (
        std::collections::HashMap<String, String>,
        serde_json::Value,
    ) {
        let reqs = server.received_requests().await.expect("requests recorded");
        let r = reqs.first().expect("at least one request");
        let mut hdrs = std::collections::HashMap::new();
        for (name, val) in r.headers.iter() {
            hdrs.insert(
                name.as_str().to_ascii_lowercase(),
                val.to_str().unwrap_or("").to_string(),
            );
        }
        let body: serde_json::Value =
            serde_json::from_slice(&r.body).expect("body is valid JSON");
        (hdrs, body)
    }

    fn sse_response(body: &str) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(body.to_string())
    }

    async fn collect_stream(
        mut rx: mpsc::Receiver<StreamEvent>,
    ) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            let done = matches!(ev, StreamEvent::Done(_) | StreamEvent::Error(_));
            out.push(ev);
            if done {
                break;
            }
        }
        out
    }

    // ------------------------------------------------------------------------
    // build_request
    // ------------------------------------------------------------------------

    #[test]
    fn build_request_omits_max_tokens_when_none() {
        let body = build_request(req("gpt-4o"), false);
        assert!(body.max_tokens.is_none());
        let serialized = serde_json::to_value(&body).unwrap();
        assert!(serialized.get("max_tokens").is_none());
    }

    #[test]
    fn build_request_omits_temperature_when_none() {
        let body = build_request(req("gpt-4o"), false);
        let serialized = serde_json::to_value(&body).unwrap();
        assert!(serialized.get("temperature").is_none());
    }

    #[test]
    fn build_request_omits_stream_when_not_streaming() {
        let body = build_request(req("gpt-4o"), false);
        let serialized = serde_json::to_value(&body).unwrap();
        assert!(serialized.get("stream").is_none());
    }

    #[test]
    fn build_request_sets_stream_true_when_streaming() {
        let body = build_request(req("gpt-4o"), true);
        let serialized = serde_json::to_value(&body).unwrap();
        assert_eq!(serialized["stream"], true);
    }

    #[test]
    fn build_request_passes_through_tools_and_tool_choice() {
        let mut r = req("gpt-4o");
        r.tools = Some(vec![json!({"type": "function", "function": {"name": "f"}})]);
        r.tool_choice = Some(json!("auto"));
        let serialized = serde_json::to_value(&build_request(r, false)).unwrap();
        assert_eq!(serialized["tools"][0]["function"]["name"], "f");
        assert_eq!(serialized["tool_choice"], "auto");
    }

    #[test]
    fn convert_messages_preserves_all_fields() {
        let msgs = vec![ProviderMessage {
            role: "tool".to_string(),
            content: json!("result"),
            name: Some("calc".to_string()),
            tool_call_id: Some("call_1".to_string()),
            tool_calls: Some(vec![json!({"id": "call_1"})]),
        }];
        let converted = convert_messages(msgs);
        assert_eq!(converted[0].role, "tool");
        assert_eq!(converted[0].name.as_deref(), Some("calc"));
        assert_eq!(converted[0].tool_call_id.as_deref(), Some("call_1"));
        assert!(converted[0].tool_calls.is_some());
    }

    // ------------------------------------------------------------------------
    // Headers + URL
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn chat_sets_bearer_auth_and_required_headers() {
        let server = mock_with_body(ok_response_json()).await;
        let client = Client::new();
        openai_compat_chat(&client, &server.uri(), "secret-key", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let (h, _) = captured(&server).await;
        assert_eq!(
            h.get("authorization").map(String::as_str),
            Some("Bearer secret-key")
        );
        assert_eq!(
            h.get("content-type").map(String::as_str),
            Some("application/json")
        );
        assert!(h
            .get("user-agent")
            .map(|v| v.starts_with("MyLobster/"))
            .unwrap_or(false));
    }

    #[tokio::test]
    async fn chat_endpoint_is_chat_completions() {
        let server = mock_with_body(ok_response_json()).await;
        let client = Client::new();
        openai_compat_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs[0].url.path(), "/chat/completions");
    }

    #[tokio::test]
    async fn chat_respects_base_url_with_trailing_path() {
        // OPENAI_BASE_URL overrides should be honored verbatim.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response_json()))
            .mount(&server)
            .await;
        let base = format!("{}/v1", server.uri());
        let client = Client::new();
        openai_compat_chat(&client, &base, "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs[0].url.path(), "/v1/chat/completions");
    }

    // ------------------------------------------------------------------------
    // Request body shaping
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn chat_passes_through_max_tokens() {
        let server = mock_with_body(ok_response_json()).await;
        let client = Client::new();
        let mut r = req("gpt-4o");
        r.max_tokens = Some(2048);
        openai_compat_chat(&client, &server.uri(), "k", r, "OpenAI")
            .await
            .unwrap();
        let (_, b) = captured(&server).await;
        assert_eq!(b["max_tokens"], 2048);
    }

    #[tokio::test]
    async fn chat_passes_through_temperature() {
        let server = mock_with_body(ok_response_json()).await;
        let client = Client::new();
        let mut r = req("gpt-4o");
        r.temperature = Some(0.3);
        openai_compat_chat(&client, &server.uri(), "k", r, "OpenAI")
            .await
            .unwrap();
        let (_, b) = captured(&server).await;
        assert_eq!(b["temperature"], 0.3);
    }

    #[tokio::test]
    async fn chat_does_not_send_stream_field() {
        let server = mock_with_body(ok_response_json()).await;
        let client = Client::new();
        openai_compat_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let (_, b) = captured(&server).await;
        assert!(b.get("stream").is_none() || b["stream"].is_null());
    }

    #[tokio::test]
    async fn chat_omits_optional_fields_when_unset() {
        let server = mock_with_body(ok_response_json()).await;
        let client = Client::new();
        openai_compat_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let (_, b) = captured(&server).await;
        assert!(b.get("max_tokens").is_none());
        assert!(b.get("temperature").is_none());
        assert!(b.get("tools").is_none());
        assert!(b.get("tool_choice").is_none());
    }

    // ------------------------------------------------------------------------
    // Response parsing
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn parses_text_response() {
        let server = mock_with_body(json!({
            "choices": [{
                "message": {"role": "assistant", "content": "world"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2}
        }))
        .await;
        let client = Client::new();
        let r = openai_compat_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        assert_eq!(r.content.len(), 1);
        assert!(matches!(&r.content[0], ContentBlock::Text(t) if t == "world"));
        assert_eq!(r.stop_reason.as_deref(), Some("stop"));
        assert_eq!(r.usage.input_tokens, Some(1));
        assert_eq!(r.usage.output_tokens, Some(2));
    }

    #[tokio::test]
    async fn parses_tool_call_response() {
        let server = mock_with_body(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_42",
                        "type": "function",
                        "function": {
                            "name": "weather",
                            "arguments": "{\"city\":\"SF\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }))
        .await;
        let client = Client::new();
        let r = openai_compat_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        match &r.content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_42");
                assert_eq!(name, "weather");
                assert_eq!(input["city"], "SF");
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
        assert_eq!(r.stop_reason.as_deref(), Some("tool_calls"));
    }

    #[tokio::test]
    async fn parses_tool_call_with_invalid_arguments_json_falls_back_to_empty_object() {
        let server = mock_with_body(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_x",
                        "function": {"name": "f", "arguments": "not-json"}
                    }]
                }
            }]
        }))
        .await;
        let client = Client::new();
        let r = openai_compat_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        match &r.content[0] {
            ContentBlock::ToolUse { input, .. } => {
                assert!(input.is_object());
                assert_eq!(input.as_object().unwrap().len(), 0);
            }
            other => panic!("expected ToolUse with empty input, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn skips_empty_text_content() {
        let server = mock_with_body(json!({
            "choices": [{
                "message": {"role": "assistant", "content": ""},
                "finish_reason": "stop"
            }]
        }))
        .await;
        let client = Client::new();
        let r = openai_compat_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        assert!(r.content.is_empty(), "empty content should not produce a Text block");
    }

    #[tokio::test]
    async fn errors_on_empty_choices() {
        let server = mock_with_body(json!({
            "choices": [],
            "usage": {"prompt_tokens": 0, "completion_tokens": 0}
        }))
        .await;
        let client = Client::new();
        let err = openai_compat_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap_err();
        assert!(err.to_string().to_lowercase().contains("no choices"));
    }

    #[tokio::test]
    async fn handles_missing_usage() {
        let server = mock_with_body(json!({
            "choices": [{"message": {"role": "assistant", "content": "x"}}]
        }))
        .await;
        let client = Client::new();
        let r = openai_compat_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        assert!(r.usage.input_tokens.is_none());
        assert!(r.usage.output_tokens.is_none());
        assert!(r.usage.cache_read_tokens.is_none());
        assert!(r.usage.cache_write_tokens.is_none());
    }

    #[tokio::test]
    async fn errors_include_provider_name_and_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string(r#"{"error":{"message":"bad key"}}"#),
            )
            .mount(&server)
            .await;
        let client = Client::new();
        let err = openai_compat_chat(&client, &server.uri(), "k", req("gpt-4o"), "Groq")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("401"), "should include status: {}", err);
        assert!(err.contains("Groq"), "should include provider name: {}", err);
    }

    // ------------------------------------------------------------------------
    // Streaming
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn stream_sends_stream_true() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(sse_response("data: [DONE]\n\n"))
            .mount(&server)
            .await;
        let client = Client::new();
        let rx = openai_compat_stream_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let _ = collect_stream(rx).await;
        let (_, b) = captured(&server).await;
        assert_eq!(b["stream"], true);
    }

    #[tokio::test]
    async fn stream_emits_text_deltas_in_order() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let client = Client::new();
        let rx = openai_compat_stream_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let events = collect_stream(rx).await;
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["hel", "lo"]);
        assert!(matches!(events.last(), Some(StreamEvent::Done(_))));
    }

    #[tokio::test]
    async fn stream_skips_empty_content_deltas() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let client = Client::new();
        let rx = openai_compat_stream_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let events = collect_stream(rx).await;
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["x"]);
    }

    #[tokio::test]
    async fn stream_emits_tool_call_deltas() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let client = Client::new();
        let rx = openai_compat_stream_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let events = collect_stream(rx).await;
        let tool_calls: Vec<&serde_json::Value> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCall(v) => Some(v),
                _ => None,
            })
            .collect();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_1");
        assert_eq!(tool_calls[0]["function"]["name"], "f");
    }

    #[tokio::test]
    async fn stream_captures_usage_from_chunk() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":7}}\n\n",
            "data: [DONE]\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let client = Client::new();
        let rx = openai_compat_stream_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let events = collect_stream(rx).await;
        let usage = match events.last() {
            Some(StreamEvent::Done(u)) => u.clone(),
            other => panic!("expected Done last, got {:?}", other.is_some()),
        };
        assert_eq!(usage.input_tokens, Some(42));
        assert_eq!(usage.output_tokens, Some(7));
    }

    #[tokio::test]
    async fn stream_skips_unparseable_chunks_and_comments() {
        let body = concat!(
            ": this is a comment line\n",
            "\n",
            "data: not-json\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"survived\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let client = Client::new();
        let rx = openai_compat_stream_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let events = collect_stream(rx).await;
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["survived"]);
    }

    #[tokio::test]
    async fn stream_terminates_on_done_sentinel() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n",
            "data: [DONE]\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"AFTER\"}}]}\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let client = Client::new();
        let rx = openai_compat_stream_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let events = collect_stream(rx).await;
        let saw_after = events
            .iter()
            .any(|e| matches!(e, StreamEvent::Delta(t) if t == "AFTER"));
        assert!(!saw_after, "[DONE] sentinel must terminate the stream");
    }

    // ------------------------------------------------------------------
    // MCP parameter-free tool schema normalization (v2026.5.2 #75362)
    // ------------------------------------------------------------------

    #[test]
    fn normalizes_missing_parameters() {
        let mut tools = vec![json!({"type": "function", "function": {"name": "f"}})];
        normalize_parameter_free_tool_schemas(&mut tools);
        assert_eq!(
            tools[0]["function"]["parameters"],
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn normalizes_null_and_invalid_parameters() {
        let mut tools = vec![
            json!({"type": "function", "function": {"name": "a", "parameters": null}}),
            json!({"type": "function", "function": {"name": "b", "parameters": "bogus"}}),
            json!({"type": "function", "function": {"name": "c",
                "parameters": {"type": "object", "properties": null}}}),
            json!({"type": "function", "function": {"name": "d",
                "parameters": {"properties": {}}}}),
        ];
        normalize_parameter_free_tool_schemas(&mut tools);
        for tool in &tools {
            assert_eq!(
                tool["function"]["parameters"],
                json!({"type": "object", "properties": {}}),
                "tool {} should be normalized",
                tool["function"]["name"]
            );
        }
    }

    #[test]
    fn leaves_valid_parameter_schemas_untouched() {
        let schema = json!({"type": "object", "properties": {"x": {"type": "string"}},
            "required": ["x"]});
        let mut tools =
            vec![json!({"type": "function", "function": {"name": "f", "parameters": schema}})];
        normalize_parameter_free_tool_schemas(&mut tools);
        assert_eq!(
            tools[0]["function"]["parameters"]["properties"]["x"]["type"],
            "string"
        );
        assert_eq!(tools[0]["function"]["parameters"]["required"][0], "x");
    }

    #[test]
    fn ignores_non_function_tools() {
        let mut tools = vec![json!({"type": "web_search"})];
        normalize_parameter_free_tool_schemas(&mut tools);
        assert_eq!(tools[0], json!({"type": "web_search"}));
    }

    #[test]
    fn build_request_normalizes_tool_schemas() {
        let mut r = req("gpt-4o");
        r.tools = Some(vec![json!({"type": "function", "function": {"name": "f"}})]);
        let body = serde_json::to_value(build_request(r, false)).unwrap();
        assert_eq!(
            body["tools"][0]["function"]["parameters"],
            json!({"type": "object", "properties": {}})
        );
    }

    // ------------------------------------------------------------------
    // TTS extra_body passthrough (v2026.5.2 #39900)
    // ------------------------------------------------------------------

    #[test]
    fn speech_body_includes_canonical_fields() {
        let body = build_speech_request_body("tts-1", "hello", "alloy", Some("mp3"), None, None, None);
        assert_eq!(body["model"], "tts-1");
        assert_eq!(body["input"], "hello");
        assert_eq!(body["voice"], "alloy");
        assert_eq!(body["response_format"], "mp3");
        assert!(body.get("speed").is_none());
        assert!(body.get("instructions").is_none());
    }

    #[test]
    fn speech_body_spreads_extra_body_fields() {
        let extra = json!({"lang": "id", "emotion": "warm"});
        let body = build_speech_request_body(
            "tts-1",
            "halo",
            "alloy",
            Some("mp3"),
            Some(1.1),
            Some("gently"),
            extra.as_object(),
        );
        assert_eq!(body["lang"], "id");
        assert_eq!(body["emotion"], "warm");
        assert_eq!(body["speed"], 1.1);
        assert_eq!(body["instructions"], "gently");
    }

    #[test]
    fn speech_body_extra_body_overrides_canonical_fields() {
        // Upstream spreads extraBody last: custom servers may override
        // canonical fields.
        let extra = json!({"voice": "custom-voice"});
        let body =
            build_speech_request_body("tts-1", "x", "alloy", None, None, None, extra.as_object());
        assert_eq!(body["voice"], "custom-voice");
    }

    #[test]
    fn speech_body_drops_prototype_pollution_keys() {
        let extra = json!({"__proto__": {"x": 1}, "constructor": 1, "prototype": 2, "ok": 3});
        let body =
            build_speech_request_body("tts-1", "x", "alloy", None, None, None, extra.as_object());
        assert!(body.get("__proto__").is_none());
        assert!(body.get("constructor").is_none());
        assert!(body.get("prototype").is_none());
        assert_eq!(body["ok"], 3);
    }

    #[tokio::test]
    async fn speech_posts_to_audio_speech_and_returns_bytes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/audio/speech"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1u8, 2, 3]))
            .mount(&server)
            .await;
        let client = Client::new();
        let body = build_speech_request_body("tts-1", "hi", "alloy", None, None, None, None);
        let bytes = openai_compat_speech(&client, &server.uri(), "k", &body, "OpenAI")
            .await
            .unwrap();
        assert_eq!(bytes, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn speech_error_includes_provider_and_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/audio/speech"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad voice"))
            .mount(&server)
            .await;
        let client = Client::new();
        let body = build_speech_request_body("tts-1", "hi", "alloy", None, None, None, None);
        let err = openai_compat_speech(&client, &server.uri(), "k", &body, "DeepInfra")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("400"));
        assert!(err.contains("DeepInfra"));
    }

    // ------------------------------------------------------------------
    // Streaming safety: bare-JSON chunk lines (v2026.7.1 #96503)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn stream_accepts_bare_json_chunk_lines() {
        let body = concat!(
            "{\"choices\":[{\"delta\":{\"content\":\"mis\"}}]}\n",
            "{\"choices\":[{\"delta\":{\"content\":\"labeled\"}}]}\n",
            "[DONE]\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let client = Client::new();
        let rx = openai_compat_stream_chat(&client, &server.uri(), "k", req("gpt-4o"), "OpenAI")
            .await
            .unwrap();
        let events = collect_stream(rx).await;
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["mis", "labeled"]);
    }

    #[tokio::test]
    async fn stream_emits_error_on_non_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .mount(&server)
            .await;
        let client = Client::new();
        let rx = openai_compat_stream_chat(&client, &server.uri(), "k", req("gpt-4o"), "Groq")
            .await
            .unwrap();
        let events = collect_stream(rx).await;
        match events.last() {
            Some(StreamEvent::Error(msg)) => {
                assert!(msg.contains("429"), "should include status: {}", msg);
                assert!(msg.contains("Groq"), "should include provider: {}", msg);
            }
            other => panic!("expected Error last, got {:?}", other.is_some()),
        }
    }

    // ------------------------------------------------------------------
    // v2026.7.1 — refusal, usage clamp, accumulator, azure, knobs, embed
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn refusal_surfaces_as_assistant_text() {
        let server = mock_with_body(json!({
            "choices": [{
                "message": {"role": "assistant", "content": null,
                            "refusal": "I cannot help with that."},
                "finish_reason": "stop"
            }]
        }))
        .await;
        let client = Client::new();
        let r = openai_compat_chat(&client, &server.uri(), "k", req("gpt-5.6"), "OpenAI")
            .await
            .unwrap();
        assert_eq!(r.content_text(), "I cannot help with that.");
    }

    #[tokio::test]
    async fn stream_refusal_deltas_surface_as_text() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"refusal\":\"no\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let client = Client::new();
        let rx = openai_compat_stream_chat(&client, &server.uri(), "k", req("gpt-5.6"), "OpenAI")
            .await
            .unwrap();
        let events = collect_stream(rx).await;
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::Delta(t) if t == "no")));
    }

    #[tokio::test]
    async fn cached_tokens_clamp_and_report() {
        let server = mock_with_body(json!({
            "choices": [{"message": {"role": "assistant", "content": "x"}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 1,
                      "prompt_tokens_details": {"cached_tokens": 25}}
        }))
        .await;
        let client = Client::new();
        let r = openai_compat_chat(&client, &server.uri(), "k", req("gpt-5.6"), "OpenAI")
            .await
            .unwrap();
        // 10 - 25 clamps to 0 instead of underflowing.
        assert_eq!(r.usage.input_tokens, Some(0));
        assert_eq!(r.usage.cache_read_tokens, Some(25));
    }

    #[test]
    fn clamp_uncached_never_underflows() {
        assert_eq!(clamp_uncached_input_tokens(100, 30), 70);
        assert_eq!(clamp_uncached_input_tokens(10, 25), 0);
    }

    #[test]
    fn tool_call_accumulator_separates_parallel_buffers() {
        let mut acc = ToolCallAccumulator::new();
        acc.push(&json!({"index": 0, "id": "call_a", "function": {"name": "alpha", "arguments": "{\"x\":"}}));
        acc.push(&json!({"index": 1, "id": "call_b", "function": {"name": "beta", "arguments": "{\"y\":"}}));
        acc.push(&json!({"index": 0, "function": {"arguments": "1}"}}));
        acc.push(&json!({"index": 1, "function": {"arguments": "2}"}}));
        let calls = acc.finish();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["function"]["arguments"], "{\"x\":1}");
        assert_eq!(calls[1]["function"]["arguments"], "{\"y\":2}");
        assert_eq!(calls[0]["id"], "call_a");
        assert_eq!(calls[1]["function"]["name"], "beta");
    }

    #[test]
    fn tool_call_accumulator_defaults_empty_args() {
        let mut acc = ToolCallAccumulator::new();
        acc.push(&json!({"index": 0, "id": "c", "function": {"name": "f"}}));
        let calls = acc.finish();
        assert_eq!(calls[0]["function"]["arguments"], "{}");
    }

    #[test]
    fn deterministic_tool_ordering_sorts_by_name() {
        let mut r = req("gpt-5.6");
        r.tools = Some(vec![
            json!({"type": "function", "function": {"name": "zeta", "parameters": {"type": "object", "properties": {}}}}),
            json!({"type": "function", "function": {"name": "alpha", "parameters": {"type": "object", "properties": {}}}}),
        ]);
        let body = serde_json::to_value(build_request(r, false)).unwrap();
        assert_eq!(body["tools"][0]["function"]["name"], "alpha");
        assert_eq!(body["tools"][1]["function"]["name"], "zeta");
    }

    #[test]
    fn azure_api_version_defaults_to_preview() {
        assert_eq!(resolve_azure_api_version(None), "preview");
        assert_eq!(resolve_azure_api_version(Some("  ")), "preview");
        assert_eq!(resolve_azure_api_version(Some("2025-04-01")), "2025-04-01");
    }

    #[test]
    fn azure_preview_routes_openai_v1_responses() {
        assert_eq!(
            azure_responses_url("https://res.openai.azure.com/", None),
            "https://res.openai.azure.com/openai/v1/responses?api-version=preview"
        );
        assert_eq!(
            azure_responses_url("https://res.openai.azure.com", Some("2025-04-01")),
            "https://res.openai.azure.com/openai/responses?api-version=2025-04-01"
        );
    }

    #[test]
    fn strict_message_keys_strips_extra_fields() {
        let mut msgs = vec![json!({"role": "assistant", "content": "x",
            "reasoning_content": "r", "cache_control": {"type": "ephemeral"}})];
        strip_messages_to_role_content(&mut msgs);
        let obj = msgs[0].as_object().unwrap();
        assert!(obj.contains_key("role"));
        assert!(obj.contains_key("content"));
        assert!(!obj.contains_key("reasoning_content"));
        assert!(!obj.contains_key("cache_control"));
    }

    #[test]
    fn supports_tools_false_strips_tool_payloads() {
        let mut r = req("m");
        r.tools = Some(vec![json!({"type": "function", "function": {"name": "f"}})]);
        r.tool_choice = Some(json!("auto"));
        strip_tool_payloads(&mut r);
        assert!(r.tools.is_none());
        assert!(r.tool_choice.is_none());
    }

    #[test]
    fn unsupported_schema_keywords_stripped_recursively() {
        let mut tools = vec![json!({"type": "function", "function": {"name": "f",
            "parameters": {"type": "object", "properties": {
                "a": {"type": "string", "minLength": 2,
                      "items": {"type": "string", "format": "uri"}}}}}})];
        strip_unsupported_schema_keywords(
            &mut tools,
            &["minLength".to_string(), "format".to_string()],
        );
        assert!(tools[0].pointer("/function/parameters/properties/a/minLength").is_none());
        assert!(tools[0]
            .pointer("/function/parameters/properties/a/items/format")
            .is_none());
    }

    #[test]
    fn reasoning_effort_normalization_walks_supported_set() {
        let supported = ["low", "medium", "high"];
        assert_eq!(normalize_reasoning_effort("high", &supported).as_deref(), Some("high"));
        // max downgrades to nearest supported (high).
        assert_eq!(normalize_reasoning_effort("max", &supported).as_deref(), Some("high"));
        assert_eq!(normalize_reasoning_effort("xhigh", &supported).as_deref(), Some("high"));
        // minimal upgrades to lowest supported when nothing lower exists.
        assert_eq!(normalize_reasoning_effort("minimal", &supported).as_deref(), Some("low"));
        assert_eq!(normalize_reasoning_effort("off", &supported), None);
        assert_eq!(normalize_reasoning_effort("high", &[]), None);
    }

    #[test]
    fn anyof_null_pairs_collapse_to_base_schema() {
        let mut schema = json!({"type": "object", "properties": {
            "opt": {"anyOf": [{"type": "string", "minLength": 1}, {"type": "null"}]}}});
        normalize_anyof_schemas(&mut schema);
        let opt = schema.pointer("/properties/opt").unwrap();
        assert!(opt.get("anyOf").is_none());
        assert_eq!(opt["type"], "string");
    }

    #[test]
    fn anyof_multi_variant_untouched() {
        let mut schema = json!({"anyOf": [{"type": "string"}, {"type": "number"}]});
        normalize_anyof_schemas(&mut schema);
        assert!(schema.get("anyOf").is_some());
    }

    #[tokio::test]
    async fn generic_embed_returns_ordered_vectors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"index": 1, "embedding": [0.3, 0.4]},
                    {"index": 0, "embedding": [0.1, 0.2]}
                ]
            })))
            .mount(&server)
            .await;
        let client = Client::new();
        let out = openai_compat_embed(
            &client,
            &server.uri(),
            "k",
            "text-embedding-3-small",
            &["a".to_string(), "b".to_string()],
            "OpenAI",
        )
        .await
        .unwrap();
        assert_eq!(out, vec![vec![0.1, 0.2], vec![0.3, 0.4]]);
    }
}
