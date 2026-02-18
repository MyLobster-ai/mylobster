use super::*;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// Check if a model supports the 1M context beta.
fn is_1m_eligible_model(model: &str) -> bool {
    model.starts_with("claude-opus-4") || model.starts_with("claude-sonnet-4")
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
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
}

// ============================================================================
// ModelProvider Implementation
// ============================================================================

#[async_trait]
impl ModelProvider for AnthropicProvider {
    async fn chat(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        let messages: Vec<AnthropicMessage> = request
            .messages
            .into_iter()
            .map(|m| AnthropicMessage {
                role: m.role,
                content: m.content,
            })
            .collect();

        let body = AnthropicRequest {
            model: request.model,
            messages,
            max_tokens: request.max_tokens.unwrap_or(4096),
            temperature: request.temperature,
            stream: None,
            tools: request.tools,
            tool_choice: request.tool_choice,
        };

        let mut req_builder = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json");

        if self.context1m && is_1m_eligible_model(&body.model) {
            req_builder = req_builder.header("anthropic-beta", "context-1m-2025-08-07");
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

        let messages: Vec<AnthropicMessage> = request
            .messages
            .into_iter()
            .map(|m| AnthropicMessage {
                role: m.role,
                content: m.content,
            })
            .collect();

        let body = AnthropicRequest {
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
        let context1m = self.context1m;

        tokio::spawn(async move {
            let mut req_builder = client
                .post(format!("{}/v1/messages", base_url))
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json");

            if context1m && is_1m_eligible_model(&body.model) {
                req_builder = req_builder.header("anthropic-beta", "context-1m-2025-08-07");
            }

            let resp = match req_builder
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
                        "Anthropic API error ({}): {}",
                        status, text
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

            // Parse SSE events from the response text
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with(':') {
                    continue;
                }
                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" {
                        break;
                    }
                    match serde_json::from_str::<AnthropicSseEvent>(data) {
                        Ok(event) => match event {
                            AnthropicSseEvent::ContentBlockDelta { delta, .. } => match delta {
                                AnthropicDelta::TextDelta { text } => {
                                    let _ = tx.send(StreamEvent::Delta(text)).await;
                                }
                                AnthropicDelta::InputJsonDelta { partial_json } => {
                                    let _ = tx
                                        .send(StreamEvent::ToolCall(
                                            serde_json::json!({ "partial_json": partial_json }),
                                        ))
                                        .await;
                                }
                            },
                            AnthropicSseEvent::MessageDelta { usage, .. } => {
                                if let Some(u) = usage {
                                    final_usage.output_tokens = u.output_tokens;
                                }
                            }
                            AnthropicSseEvent::MessageStart { message } => {
                                if let Some(msg) = message {
                                    if let Some(u) = msg.get("usage") {
                                        if let Ok(usage) =
                                            serde_json::from_value::<AnthropicUsage>(u.clone())
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
