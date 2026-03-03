//! Anthropic-compatible provider.
//!
//! Used by providers that implement the Anthropic Messages API format
//! with a custom base URL (Minimax, Xiaomi Mimo, Kimi Coding, Cloudflare AI Gateway).
//!
//! Reuses the same Messages API protocol as the Anthropic provider.

use super::*;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

// ============================================================================
// Anthropic Messages API Types (shared with anthropic.rs)
// ============================================================================

#[derive(Debug, Serialize)]
struct MessagesRequest {
    model: String,
    messages: Vec<Message>,
    max_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Message {
    role: String,
    content: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlockResp>,
    stop_reason: Option<String>,
    usage: Option<UsageResp>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ContentBlockResp {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Debug, Deserialize)]
struct UsageResp {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

// SSE event types for streaming
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum SseEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: Option<serde_json::Value> },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: u32,
        content_block: Option<serde_json::Value>,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: u32, delta: Delta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: u32 },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: Option<serde_json::Value>,
        usage: Option<UsageResp>,
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
enum Delta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
}

// ============================================================================
// Provider
// ============================================================================

pub struct AnthropicCompatProvider {
    api_key: String,
    base_url: String,
    model: String,
    provider_name: String,
    client: Client,
}

impl AnthropicCompatProvider {
    pub fn new(
        api_key: String,
        base_url: String,
        model: String,
        provider_name: String,
    ) -> Self {
        Self {
            api_key,
            base_url,
            model,
            provider_name,
            client: Client::new(),
        }
    }
}

#[async_trait]
impl ModelProvider for AnthropicCompatProvider {
    async fn chat(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        let messages: Vec<Message> = request
            .messages
            .into_iter()
            .map(|m| Message {
                role: m.role,
                content: m.content,
            })
            .collect();

        let body = MessagesRequest {
            model: request.model,
            messages,
            max_tokens: request.max_tokens.unwrap_or(4096),
            temperature: request.temperature,
            stream: None,
            tools: request.tools,
            tool_choice: request.tool_choice,
        };

        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("{} API error ({}): {}", self.provider_name, status, text);
        }

        let api_resp: MessagesResponse = resp.json().await?;

        let content: Vec<ContentBlock> = api_resp
            .content
            .into_iter()
            .map(|block| match block {
                ContentBlockResp::Text { text } => ContentBlock::Text(text),
                ContentBlockResp::ToolUse { id, name, input } => {
                    ContentBlock::ToolUse { id, name, input }
                }
            })
            .collect();

        let usage = api_resp.usage.unwrap_or(UsageResp {
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

        let messages: Vec<Message> = request
            .messages
            .into_iter()
            .map(|m| Message {
                role: m.role,
                content: m.content,
            })
            .collect();

        let body = MessagesRequest {
            model: request.model,
            messages,
            max_tokens: request.max_tokens.unwrap_or(4096),
            temperature: request.temperature,
            stream: Some(true),
            tools: request.tools,
            tool_choice: request.tool_choice,
        };

        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();
        let provider_name = self.provider_name.clone();

        tokio::spawn(async move {
            let resp = match client
                .post(format!("{}/v1/messages", base_url))
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
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

            let mut final_usage = crate::gateway::TokenUsage {
                input_tokens: None,
                output_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
            };

            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with(':') {
                    continue;
                }
                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" {
                        break;
                    }
                    match serde_json::from_str::<SseEvent>(data) {
                        Ok(event) => match event {
                            SseEvent::ContentBlockDelta { delta, .. } => match delta {
                                Delta::TextDelta { text } => {
                                    let _ = tx.send(StreamEvent::Delta(text)).await;
                                }
                                Delta::InputJsonDelta { partial_json } => {
                                    let _ = tx
                                        .send(StreamEvent::ToolCall(
                                            serde_json::json!({ "partial_json": partial_json }),
                                        ))
                                        .await;
                                }
                            },
                            SseEvent::MessageDelta { usage, .. } => {
                                if let Some(u) = usage {
                                    final_usage.output_tokens = u.output_tokens;
                                }
                            }
                            SseEvent::MessageStart { message } => {
                                if let Some(msg) = message {
                                    if let Some(u) = msg.get("usage") {
                                        if let Ok(usage) =
                                            serde_json::from_value::<UsageResp>(u.clone())
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
                            SseEvent::MessageStop {} => {
                                break;
                            }
                            SseEvent::Error { error } => {
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
                        Err(_) => {}
                    }
                }
            }

            let _ = tx.send(StreamEvent::Done(final_usage)).await;
        });

        Ok(rx)
    }

    fn name(&self) -> &str {
        &self.provider_name
    }
}
