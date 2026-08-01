use super::*;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// Check if a model supports the 1M context beta.
///
/// v2026.6.x: 1M context reached GA — kept only for capability metadata; the
/// retired `context-1m-2025-08-07` beta header is no longer sent.
#[allow(dead_code)]
fn is_1m_eligible_model(model: &str) -> bool {
    model.starts_with("claude-opus-4") || model.starts_with("claude-sonnet-4")
}

/// Sanitize thinking blocks in outbound replay (v2026.5.x–6.x):
///
/// * thinking disabled → thinking/redacted_thinking blocks are stripped
///   entirely (Anthropic rejects replayed thinking without the beta).
/// * thinking enabled → stale/empty `signature` fields are stripped while
///   seeded (non-empty) signatures are preserved.
fn sanitize_thinking_blocks(messages: &mut [AnthropicMessage], thinking_enabled: bool) {
    for message in messages.iter_mut() {
        let Some(blocks) = message.content.as_array_mut() else {
            continue;
        };
        if thinking_enabled {
            for block in blocks.iter_mut() {
                let Some(obj) = block.as_object_mut() else {
                    continue;
                };
                if obj.get("type").and_then(|t| t.as_str()) == Some("thinking") {
                    let stale = match obj.get("signature") {
                        Some(serde_json::Value::String(sig)) => sig.is_empty(),
                        Some(_) => true,
                        None => false,
                    };
                    if stale {
                        obj.remove("signature");
                    }
                }
            }
        } else {
            blocks.retain(|block| {
                !matches!(
                    block.get("type").and_then(|t| t.as_str()),
                    Some("thinking") | Some("redacted_thinking")
                )
            });
        }
    }
}

/// Merge consecutive assistant turns into one message (v2026.6.x):
/// Anthropic rejects transcripts with back-to-back assistant messages after
/// compaction/steering; adjacent assistant turns collapse into a single
/// message with concatenated content blocks.
fn merge_consecutive_assistant_turns(messages: &mut Vec<AnthropicMessage>) {
    let mut merged: Vec<AnthropicMessage> = Vec::with_capacity(messages.len());
    for message in messages.drain(..) {
        let can_merge = message.role == "assistant"
            && merged.last().map(|m: &AnthropicMessage| m.role == "assistant").unwrap_or(false);
        if !can_merge {
            merged.push(message);
            continue;
        }
        let previous = merged.last_mut().expect("checked non-empty");
        let mut prev_blocks = content_to_blocks(std::mem::take(&mut previous.content));
        prev_blocks.extend(content_to_blocks(message.content));
        previous.content = serde_json::Value::Array(prev_blocks);
    }
    *messages = merged;
}

fn content_to_blocks(content: serde_json::Value) -> Vec<serde_json::Value> {
    match content {
        serde_json::Value::Array(blocks) => blocks,
        serde_json::Value::String(text) => {
            if text.is_empty() {
                Vec::new()
            } else {
                vec![serde_json::json!({"type": "text", "text": text})]
            }
        }
        serde_json::Value::Null => Vec::new(),
        other => vec![serde_json::json!({"type": "text", "text": other.to_string()})],
    }
}

pub struct AnthropicProvider {
    api_key: String,
    base_url: String,
    model: String,
    client: Client,
    context1m: bool,
}

impl AnthropicProvider {
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        Self {
            api_key,
            base_url,
            model,
            client: Client::new(),
            context1m: false,
        }
    }

    pub fn with_context1m(mut self, enabled: bool) -> Self {
        self.context1m = enabled;
        self
    }
}

// ============================================================================
// Anthropic API Types
// ============================================================================

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    max_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
struct AnthropicMessage {
    role: String,
    content: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContentBlock>,
    stop_reason: Option<String>,
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

// ============================================================================
// SSE Event Types
// ============================================================================

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AnthropicSseEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: Option<serde_json::Value> },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: u32,
        content_block: Option<serde_json::Value>,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: u32, delta: AnthropicDelta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: u32 },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: Option<serde_json::Value>,
        usage: Option<AnthropicUsage>,
    },
    #[serde(rename = "message_stop")]
    MessageStop {},
    #[serde(rename = "ping")]
    Ping {},
    #[serde(rename = "error")]
    Error { error: Option<serde_json::Value> },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AnthropicDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { thinking: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
}

// ============================================================================
// ModelProvider Implementation
// ============================================================================

#[async_trait]
impl ModelProvider for AnthropicProvider {
    async fn chat(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        let mut messages: Vec<AnthropicMessage> = request
            .messages
            .into_iter()
            .map(|m| AnthropicMessage {
                role: m.role,
                content: m.content,
            })
            .collect();
        sanitize_thinking_blocks(&mut messages, request.thinking.is_some());
        merge_consecutive_assistant_turns(&mut messages);

        let thinking_enabled = request.thinking.is_some();
        let budget_tokens = request.thinking.as_ref().map(|t| t.budget_tokens).unwrap_or(0);

        let body = AnthropicRequest {
            model: request.model,
            messages,
            max_tokens: if thinking_enabled {
                request.max_tokens.unwrap_or(budget_tokens + 8192)
            } else {
                request.max_tokens.unwrap_or(4096)
            },
            temperature: if thinking_enabled { None } else { request.temperature },
            stream: None,
            tools: request.tools,
            tool_choice: request.tool_choice,
            thinking: if thinking_enabled {
                Some(serde_json::json!({
                    "type": "enabled",
                    "budget_tokens": budget_tokens
                }))
            } else {
                None
            },
        };

        let mut req_builder = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .header("User-Agent", "MyLobster/2026.4.1");

        let mut betas = Vec::new();
        if thinking_enabled {
            betas.push("interleaved-thinking-2025-05-14");
        }
        // v2026.6.x: 1M context is GA — the retired `context-1m-2025-08-07`
        // beta header is no longer sent (eligible models get 1M by default).
        if !betas.is_empty() {
            req_builder = req_builder.header("anthropic-beta", betas.join(","));
        }

        let resp = req_builder.json(&body).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API error ({}): {}", status, text);
        }

        let api_resp: AnthropicResponse = resp.json().await?;

        let content: Vec<ContentBlock> = api_resp
            .content
            .into_iter()
            .map(|block| match block {
                AnthropicContentBlock::Text { text } => ContentBlock::Text(text),
                AnthropicContentBlock::Thinking { thinking } => ContentBlock::Thinking(thinking),
                AnthropicContentBlock::ToolUse { id, name, input } => {
                    ContentBlock::ToolUse { id, name, input }
                }
            })
            .collect();

        let usage = api_resp.usage.unwrap_or(AnthropicUsage {
            input_tokens: None,
            output_tokens: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        });

        Ok(ProviderResponse {
            content,
            stop_reason: api_resp.stop_reason,
            usage: crate::gateway::TokenUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: usage.cache_read_input_tokens,
                cache_write_tokens: usage.cache_creation_input_tokens,
            },
        })
    }

    async fn stream_chat(&self, request: ProviderRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        let (tx, rx) = mpsc::channel(256);

        let mut messages: Vec<AnthropicMessage> = request
            .messages
            .into_iter()
            .map(|m| AnthropicMessage {
                role: m.role,
                content: m.content,
            })
            .collect();
        sanitize_thinking_blocks(&mut messages, request.thinking.is_some());
        merge_consecutive_assistant_turns(&mut messages);

        let thinking_enabled = request.thinking.is_some();
        let budget_tokens = request.thinking.as_ref().map(|t| t.budget_tokens).unwrap_or(0);

        let body = AnthropicRequest {
            model: request.model,
            messages,
            max_tokens: if thinking_enabled {
                request.max_tokens.unwrap_or(budget_tokens + 8192)
            } else {
                request.max_tokens.unwrap_or(4096)
            },
            temperature: if thinking_enabled { None } else { request.temperature },
            stream: Some(true),
            tools: request.tools,
            tool_choice: request.tool_choice,
            thinking: if thinking_enabled {
                Some(serde_json::json!({
                    "type": "enabled",
                    "budget_tokens": budget_tokens
                }))
            } else {
                None
            },
        };

        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();
        let context1m = self.context1m;

        tokio::spawn(async move {
            let mut req_builder = client
                .post(format!("{}/v1/messages", base_url))
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .header("User-Agent", "MyLobster/2026.4.1");

            let mut betas = Vec::new();
            if thinking_enabled {
                betas.push("interleaved-thinking-2025-05-14");
            }
            // v2026.6.x: 1M context is GA — retired beta header not sent.
            let _ = context1m;
            if !betas.is_empty() {
                req_builder = req_builder.header("anthropic-beta", betas.join(","));
            }

            let resp = match req_builder.json(&body).send().await {
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
                        "Anthropic API error ({}): {}",
                        status, text
                    )))
                    .await;
                return;
            }

            let mut final_usage = crate::gateway::TokenUsage {
                input_tokens: None,
                output_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
            };

            // Track active content blocks for tool call accumulation
            struct ActiveBlock {
                block_type: String,
                tool_id: String,
                tool_name: String,
                accumulated_json: String,
            }
            let mut active_blocks: std::collections::HashMap<u32, ActiveBlock> =
                std::collections::HashMap::new();
            let mut done = false;

            // Stream SSE events as they arrive (true streaming — not buffered)
            use futures::StreamExt;
            let mut byte_stream = resp.bytes_stream();
            let mut buf = String::new();

            while let Some(chunk_result) = byte_stream.next().await {
                if done {
                    break;
                }
                let chunk = match chunk_result {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx
                            .send(StreamEvent::Error(format!("Stream error: {}", e)))
                            .await;
                        return;
                    }
                };

                buf.push_str(&String::from_utf8_lossy(&chunk));

                // Process complete lines
                while let Some(newline_pos) = buf.find('\n') {
                    let line = buf[..newline_pos].trim().to_string();
                    buf = buf[newline_pos + 1..].to_string();

                    if line.is_empty() || line.starts_with(':') {
                        continue;
                    }

                    let data = match line.strip_prefix("data: ") {
                        Some(d) => d,
                        None => continue,
                    };
                    if data == "[DONE]" {
                        done = true;
                        break;
                    }

                    match serde_json::from_str::<AnthropicSseEvent>(data) {
                        Ok(event) => match event {
                            AnthropicSseEvent::ContentBlockStart {
                                index,
                                content_block,
                            } => {
                                if let Some(cb) = content_block {
                                    let block_type = cb
                                        .get("type")
                                        .and_then(|t| t.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let tool_id = cb
                                        .get("id")
                                        .and_then(|t| t.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let tool_name = cb
                                        .get("name")
                                        .and_then(|t| t.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    active_blocks.insert(
                                        index,
                                        ActiveBlock {
                                            block_type,
                                            tool_id,
                                            tool_name,
                                            accumulated_json: String::new(),
                                        },
                                    );
                                }
                            }
                            AnthropicSseEvent::ContentBlockDelta { index, delta } => {
                                match delta {
                                    AnthropicDelta::TextDelta { text } => {
                                        let _ = tx.send(StreamEvent::Delta(text)).await;
                                    }
                                    AnthropicDelta::ThinkingDelta { thinking } => {
                                        let _ =
                                            tx.send(StreamEvent::Thinking(thinking)).await;
                                    }
                                    AnthropicDelta::InputJsonDelta { partial_json } => {
                                        if let Some(block) = active_blocks.get_mut(&index) {
                                            block.accumulated_json.push_str(&partial_json);
                                        }
                                    }
                                }
                            }
                            AnthropicSseEvent::ContentBlockStop { index } => {
                                if let Some(block) = active_blocks.remove(&index) {
                                    if block.block_type == "tool_use" {
                                        let input: serde_json::Value =
                                            serde_json::from_str(&block.accumulated_json)
                                                .unwrap_or(serde_json::Value::Object(
                                                    Default::default(),
                                                ));
                                        let tool_call = serde_json::json!({
                                            "id": block.tool_id,
                                            "name": block.tool_name,
                                            "input": input,
                                        });
                                        let _ =
                                            tx.send(StreamEvent::ToolCall(tool_call)).await;
                                    }
                                }
                            }
                            AnthropicSseEvent::MessageDelta { usage, .. } => {
                                if let Some(u) = usage {
                                    final_usage.output_tokens = u.output_tokens;
                                }
                            }
                            AnthropicSseEvent::MessageStart { message } => {
                                if let Some(msg) = message {
                                    if let Some(u) = msg.get("usage") {
                                        if let Ok(usage) =
                                            serde_json::from_value::<AnthropicUsage>(
                                                u.clone(),
                                            )
                                        {
                                            final_usage.input_tokens = usage.input_tokens;
                                            final_usage.cache_read_tokens =
                                                usage.cache_read_input_tokens;
                                            final_usage.cache_write_tokens =
                                                usage.cache_creation_input_tokens;
                                        }
                                    }
                                }
                            }
                            AnthropicSseEvent::MessageStop {} => {
                                done = true;
                                break;
                            }
                            AnthropicSseEvent::Error { error } => {
                                let msg = error
                                    .and_then(|e| {
                                        e.get("message")
                                            .and_then(|m| m.as_str())
                                            .map(|s| s.to_string())
                                    })
                                    .unwrap_or_else(|| "Unknown error".to_string());
                                let _ = tx.send(StreamEvent::Error(msg)).await;
                                return;
                            }
                            _ => {}
                        },
                        Err(_) => {
                            // Skip unparseable events
                        }
                    }
                }
            }

            let _ = tx.send(StreamEvent::Done(final_usage)).await;
        });

        Ok(rx)
    }

    fn name(&self) -> &str {
        "anthropic"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderMessage;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ------------------------------------------------------------------------
    // 1M-context model eligibility
    // ------------------------------------------------------------------------

    #[test]
    fn test_1m_eligible_opus_4() {
        assert!(is_1m_eligible_model("claude-opus-4-20250514"));
    }

    #[test]
    fn test_1m_eligible_opus_46() {
        assert!(is_1m_eligible_model("claude-opus-4-6-20250514"));
    }

    #[test]
    fn test_1m_eligible_sonnet_4() {
        assert!(is_1m_eligible_model("claude-sonnet-4-20250514"));
    }

    #[test]
    fn test_1m_eligible_sonnet_46() {
        assert!(is_1m_eligible_model("claude-sonnet-4-6-20250514"));
    }

    #[test]
    fn test_not_1m_eligible_claude_3() {
        assert!(!is_1m_eligible_model("claude-3-5-sonnet-20241022"));
        assert!(!is_1m_eligible_model("claude-3-opus-20240229"));
        assert!(!is_1m_eligible_model("claude-haiku-3-5-20241022"));
    }

    #[test]
    fn test_not_1m_eligible_other_providers() {
        assert!(!is_1m_eligible_model("gpt-4"));
        assert!(!is_1m_eligible_model("gemini-pro"));
    }

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
            "id": "msg_01",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })
    }

    async fn mock_with_body(body: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    /// Capture and decode the first POSTed request body and return (headers, body json).
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

    // ------------------------------------------------------------------------
    // Headers
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn chat_sets_required_headers() {
        let server = mock_with_body(ok_response_json()).await;
        let p = AnthropicProvider::new(
            "test-key".into(),
            server.uri(),
            "claude-3-5-sonnet-20241022".into(),
        );
        p.chat(req("claude-3-5-sonnet-20241022")).await.unwrap();
        let (h, _) = captured(&server).await;
        assert_eq!(h.get("x-api-key").map(String::as_str), Some("test-key"));
        assert_eq!(
            h.get("anthropic-version").map(String::as_str),
            Some("2023-06-01")
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
    async fn chat_omits_anthropic_beta_when_no_thinking_no_1m() {
        let server = mock_with_body(ok_response_json()).await;
        let p = AnthropicProvider::new(
            "k".into(),
            server.uri(),
            "claude-3-5-sonnet-20241022".into(),
        );
        p.chat(req("claude-3-5-sonnet-20241022")).await.unwrap();
        let (h, _) = captured(&server).await;
        assert!(h.get("anthropic-beta").is_none());
    }

    #[tokio::test]
    async fn chat_sets_thinking_beta_when_thinking_enabled() {
        let server = mock_with_body(ok_response_json()).await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "claude-opus-4-20250514".into());
        let mut r = req("claude-opus-4-20250514");
        r.thinking = Some(ThinkingConfig { budget_tokens: 1024 });
        p.chat(r).await.unwrap();
        let (h, _) = captured(&server).await;
        assert_eq!(
            h.get("anthropic-beta").map(String::as_str),
            Some("interleaved-thinking-2025-05-14")
        );
    }

    #[tokio::test]
    async fn chat_no_longer_sends_retired_1m_beta_after_ga() {
        // v2026.6.x: 1M context reached GA — the retired beta header must not
        // be sent even when context1m is configured on an eligible model.
        let server = mock_with_body(ok_response_json()).await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "claude-opus-4-20250514".into())
            .with_context1m(true);
        p.chat(req("claude-opus-4-20250514")).await.unwrap();
        let (h, _) = captured(&server).await;
        assert!(h.get("anthropic-beta").is_none());
    }

    #[tokio::test]
    async fn chat_omits_1m_beta_when_context1m_but_ineligible_model() {
        let server = mock_with_body(ok_response_json()).await;
        let p = AnthropicProvider::new(
            "k".into(),
            server.uri(),
            "claude-3-5-sonnet-20241022".into(),
        )
        .with_context1m(true);
        p.chat(req("claude-3-5-sonnet-20241022")).await.unwrap();
        let (h, _) = captured(&server).await;
        assert!(h.get("anthropic-beta").is_none());
    }

    #[tokio::test]
    async fn chat_thinking_beta_stands_alone_after_1m_ga() {
        // v2026.6.x GA migration: with thinking + context1m only the
        // interleaved-thinking beta remains on the wire.
        let server = mock_with_body(ok_response_json()).await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "claude-opus-4-20250514".into())
            .with_context1m(true);
        let mut r = req("claude-opus-4-20250514");
        r.thinking = Some(ThinkingConfig { budget_tokens: 512 });
        p.chat(r).await.unwrap();
        let (h, _) = captured(&server).await;
        let beta = h.get("anthropic-beta").map(String::as_str).unwrap_or("");
        assert_eq!(beta, "interleaved-thinking-2025-05-14");
        assert!(!beta.contains("context-1m-2025-08-07"));
    }

    // ------------------------------------------------------------------------
    // Request body
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn chat_default_max_tokens_4096_when_not_thinking() {
        let server = mock_with_body(ok_response_json()).await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        p.chat(req("m")).await.unwrap();
        let (_, b) = captured(&server).await;
        assert_eq!(b["max_tokens"], 4096);
    }

    #[tokio::test]
    async fn chat_default_max_tokens_budget_plus_8192_when_thinking() {
        let server = mock_with_body(ok_response_json()).await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let mut r = req("m");
        r.thinking = Some(ThinkingConfig { budget_tokens: 1024 });
        p.chat(r).await.unwrap();
        let (_, b) = captured(&server).await;
        assert_eq!(b["max_tokens"], 1024 + 8192);
    }

    #[tokio::test]
    async fn chat_respects_explicit_max_tokens() {
        let server = mock_with_body(ok_response_json()).await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let mut r = req("m");
        r.max_tokens = Some(2048);
        p.chat(r).await.unwrap();
        let (_, b) = captured(&server).await;
        assert_eq!(b["max_tokens"], 2048);
    }

    #[tokio::test]
    async fn chat_passes_through_temperature_when_not_thinking() {
        let server = mock_with_body(ok_response_json()).await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let mut r = req("m");
        r.temperature = Some(0.7);
        p.chat(r).await.unwrap();
        let (_, b) = captured(&server).await;
        assert_eq!(b["temperature"], 0.7);
    }

    #[tokio::test]
    async fn chat_drops_temperature_when_thinking() {
        let server = mock_with_body(ok_response_json()).await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let mut r = req("m");
        r.temperature = Some(0.7);
        r.thinking = Some(ThinkingConfig { budget_tokens: 100 });
        p.chat(r).await.unwrap();
        let (_, b) = captured(&server).await;
        assert!(
            b.get("temperature").is_none() || b["temperature"].is_null(),
            "temperature should be omitted when thinking is enabled, got {:?}",
            b.get("temperature")
        );
    }

    #[tokio::test]
    async fn chat_sends_thinking_object_when_enabled() {
        let server = mock_with_body(ok_response_json()).await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let mut r = req("m");
        r.thinking = Some(ThinkingConfig { budget_tokens: 4096 });
        p.chat(r).await.unwrap();
        let (_, b) = captured(&server).await;
        assert_eq!(b["thinking"]["type"], "enabled");
        assert_eq!(b["thinking"]["budget_tokens"], 4096);
    }

    #[tokio::test]
    async fn chat_does_not_send_stream_field() {
        let server = mock_with_body(ok_response_json()).await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        p.chat(req("m")).await.unwrap();
        let (_, b) = captured(&server).await;
        assert!(
            b.get("stream").is_none() || b["stream"].is_null(),
            "non-streaming chat should omit stream field"
        );
    }

    #[tokio::test]
    async fn chat_passes_through_tools_and_tool_choice() {
        let server = mock_with_body(ok_response_json()).await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let mut r = req("m");
        r.tools = Some(vec![json!({"name": "ping", "description": "p", "input_schema": {}})]);
        r.tool_choice = Some(json!({"type": "auto"}));
        p.chat(r).await.unwrap();
        let (_, b) = captured(&server).await;
        assert_eq!(b["tools"][0]["name"], "ping");
        assert_eq!(b["tool_choice"]["type"], "auto");
    }

    #[tokio::test]
    async fn chat_endpoint_is_v1_messages() {
        let server = mock_with_body(ok_response_json()).await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        p.chat(req("m")).await.unwrap();
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs[0].url.path(), "/v1/messages");
    }

    // ------------------------------------------------------------------------
    // Non-streaming response parsing
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn chat_parses_text_response() {
        let server = mock_with_body(json!({
            "content": [{"type": "text", "text": "world"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 2}
        }))
        .await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let r = p.chat(req("m")).await.unwrap();
        assert_eq!(r.content.len(), 1);
        assert!(matches!(&r.content[0], ContentBlock::Text(t) if t == "world"));
        assert_eq!(r.stop_reason.as_deref(), Some("end_turn"));
    }

    #[tokio::test]
    async fn chat_parses_thinking_response() {
        let server = mock_with_body(json!({
            "content": [
                {"type": "thinking", "thinking": "reasoning..."},
                {"type": "text", "text": "answer"}
            ],
            "stop_reason": "end_turn"
        }))
        .await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let r = p.chat(req("m")).await.unwrap();
        assert_eq!(r.content.len(), 2);
        assert!(matches!(&r.content[0], ContentBlock::Thinking(t) if t == "reasoning..."));
        assert!(matches!(&r.content[1], ContentBlock::Text(t) if t == "answer"));
    }

    #[tokio::test]
    async fn chat_parses_tool_use_response() {
        let server = mock_with_body(json!({
            "content": [{
                "type": "tool_use",
                "id": "toolu_1",
                "name": "weather",
                "input": {"city": "SF"}
            }],
            "stop_reason": "tool_use"
        }))
        .await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let r = p.chat(req("m")).await.unwrap();
        match &r.content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_1");
                assert_eq!(name, "weather");
                assert_eq!(input["city"], "SF");
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn chat_parses_usage_with_cache_tokens() {
        let server = mock_with_body(json!({
            "content": [{"type": "text", "text": "x"}],
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
                "cache_read_input_tokens": 70,
                "cache_creation_input_tokens": 30
            }
        }))
        .await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let r = p.chat(req("m")).await.unwrap();
        assert_eq!(r.usage.input_tokens, Some(100));
        assert_eq!(r.usage.output_tokens, Some(50));
        assert_eq!(r.usage.cache_read_tokens, Some(70));
        assert_eq!(r.usage.cache_write_tokens, Some(30));
    }

    #[tokio::test]
    async fn chat_handles_missing_usage() {
        let server = mock_with_body(json!({
            "content": [{"type": "text", "text": "x"}]
        }))
        .await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let r = p.chat(req("m")).await.unwrap();
        assert!(r.usage.input_tokens.is_none());
        assert!(r.usage.output_tokens.is_none());
    }

    #[tokio::test]
    async fn chat_returns_error_on_non_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(429).set_body_string(r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#),
            )
            .mount(&server)
            .await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let err = p.chat(req("m")).await.unwrap_err().to_string();
        assert!(err.contains("429"), "error should mention status: {}", err);
        assert!(err.to_lowercase().contains("anthropic"), "error should mention provider: {}", err);
    }

    // ------------------------------------------------------------------------
    // Streaming
    // ------------------------------------------------------------------------

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

    #[tokio::test]
    async fn stream_chat_sends_stream_true() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(sse_response(
                "data: {\"type\":\"message_stop\"}\n\n",
            ))
            .mount(&server)
            .await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let rx = p.stream_chat(req("m")).await.unwrap();
        let _ = collect_stream(rx).await;
        let (_, b) = captured(&server).await;
        assert_eq!(b["stream"], true);
    }

    #[tokio::test]
    async fn stream_chat_emits_text_deltas() {
        let body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hel\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let rx = p.stream_chat(req("m")).await.unwrap();
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
    async fn stream_chat_emits_thinking_deltas() {
        let body = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"because\"}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let rx = p.stream_chat(req("m")).await.unwrap();
        let events = collect_stream(rx).await;
        let thinking: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Thinking(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, vec!["because"]);
    }

    #[tokio::test]
    async fn stream_chat_accumulates_tool_use_and_emits_on_block_stop() {
        let body = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_42\",\"name\":\"calc\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"x\\\":\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"42}\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let rx = p.stream_chat(req("m")).await.unwrap();
        let events = collect_stream(rx).await;
        let tool_calls: Vec<&serde_json::Value> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCall(v) => Some(v),
                _ => None,
            })
            .collect();
        assert_eq!(tool_calls.len(), 1, "exactly one tool call should be emitted");
        let tc = tool_calls[0];
        assert_eq!(tc["id"], "tu_42");
        assert_eq!(tc["name"], "calc");
        assert_eq!(tc["input"]["x"], 42);
    }

    #[tokio::test]
    async fn stream_chat_captures_usage_from_message_start_and_delta() {
        let body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":40,\"cache_creation_input_tokens\":20}}}\n\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":17}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let rx = p.stream_chat(req("m")).await.unwrap();
        let events = collect_stream(rx).await;
        let usage = match events.last() {
            Some(StreamEvent::Done(u)) => u.clone(),
            other => panic!("expected Done as last event, got {:?}", other.is_some()),
        };
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(17));
        assert_eq!(usage.cache_read_tokens, Some(40));
        assert_eq!(usage.cache_write_tokens, Some(20));
    }

    #[tokio::test]
    async fn stream_chat_terminates_on_message_stop() {
        let body = concat!(
            "data: {\"type\":\"message_stop\"}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"AFTER\"}}\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let rx = p.stream_chat(req("m")).await.unwrap();
        let events = collect_stream(rx).await;
        let saw_after = events.iter().any(|e| matches!(e, StreamEvent::Delta(t) if t == "AFTER"));
        assert!(!saw_after, "deltas after message_stop must not be emitted");
    }

    #[tokio::test]
    async fn stream_chat_terminates_on_done_sentinel() {
        let body = concat!(
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "data: [DONE]\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"AFTER\"}}\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let rx = p.stream_chat(req("m")).await.unwrap();
        let events = collect_stream(rx).await;
        let saw_after = events.iter().any(|e| matches!(e, StreamEvent::Delta(t) if t == "AFTER"));
        assert!(!saw_after, "[DONE] sentinel must terminate stream");
    }

    #[tokio::test]
    async fn stream_chat_emits_error_on_error_event() {
        let body = concat!(
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"servers busy\"}}\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let rx = p.stream_chat(req("m")).await.unwrap();
        let events = collect_stream(rx).await;
        let last = events.last().expect("at least one event");
        match last {
            StreamEvent::Error(msg) => assert!(msg.contains("servers busy")),
            _ => panic!("expected Error event, got something else"),
        }
    }

    #[tokio::test]
    async fn stream_chat_ignores_ping_and_comment_lines() {
        let body = concat!(
            ": this is a comment\n",
            "\n",
            "data: {\"type\":\"ping\"}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let rx = p.stream_chat(req("m")).await.unwrap();
        let events = collect_stream(rx).await;
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["ok"]);
    }

    #[tokio::test]
    async fn stream_chat_emits_error_on_non_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;
        let p = AnthropicProvider::new("bad-key".into(), server.uri(), "m".into());
        let rx = p.stream_chat(req("m")).await.unwrap();
        let events = collect_stream(rx).await;
        match events.last() {
            Some(StreamEvent::Error(msg)) => {
                assert!(msg.contains("401"), "error should mention status: {}", msg);
            }
            other => panic!("expected Error event, got {:?}", other.is_some()),
        }
    }

    #[tokio::test]
    async fn stream_chat_skips_unparseable_events() {
        let body = concat!(
            "data: {this is not valid json}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"survived\"}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(sse_response(body))
            .mount(&server)
            .await;
        let p = AnthropicProvider::new("k".into(), server.uri(), "m".into());
        let rx = p.stream_chat(req("m")).await.unwrap();
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

    // ------------------------------------------------------------------
    // v2026.7.1 — thinking-block sanitize + assistant-turn merge
    // ------------------------------------------------------------------

    fn msg(role: &str, content: serde_json::Value) -> AnthropicMessage {
        AnthropicMessage { role: role.to_string(), content }
    }

    #[test]
    fn thinking_blocks_stripped_when_disabled() {
        let mut messages = vec![msg(
            "assistant",
            serde_json::json!([
                {"type": "thinking", "thinking": "t", "signature": "sig"},
                {"type": "redacted_thinking", "data": "x"},
                {"type": "text", "text": "visible"}
            ]),
        )];
        sanitize_thinking_blocks(&mut messages, false);
        let blocks = messages[0].content.as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
    }

    #[test]
    fn stale_signatures_stripped_seeded_preserved_when_enabled() {
        let mut messages = vec![msg(
            "assistant",
            serde_json::json!([
                {"type": "thinking", "thinking": "a", "signature": ""},
                {"type": "thinking", "thinking": "b", "signature": {"bad": true}},
                {"type": "thinking", "thinking": "c", "signature": "seeded-sig"}
            ]),
        )];
        sanitize_thinking_blocks(&mut messages, true);
        let blocks = messages[0].content.as_array().unwrap();
        assert!(blocks[0].get("signature").is_none());
        assert!(blocks[1].get("signature").is_none());
        assert_eq!(blocks[2]["signature"], "seeded-sig");
        // Thinking blocks themselves are preserved when enabled.
        assert_eq!(blocks.len(), 3);
    }

    #[test]
    fn consecutive_assistant_turns_merge() {
        let mut messages = vec![
            msg("user", serde_json::json!("q")),
            msg("assistant", serde_json::json!("first")),
            msg("assistant", serde_json::json!([{"type": "text", "text": "second"}])),
            msg("user", serde_json::json!("next")),
        ];
        merge_consecutive_assistant_turns(&mut messages);
        assert_eq!(messages.len(), 3);
        let blocks = messages[1].content.as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["text"], "first");
        assert_eq!(blocks[1]["text"], "second");
    }

    #[test]
    fn non_adjacent_assistant_turns_untouched() {
        let mut messages = vec![
            msg("assistant", serde_json::json!("a")),
            msg("user", serde_json::json!("u")),
            msg("assistant", serde_json::json!("b")),
        ];
        merge_consecutive_assistant_turns(&mut messages);
        assert_eq!(messages.len(), 3);
    }
}
