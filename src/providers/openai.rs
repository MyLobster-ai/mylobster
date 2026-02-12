use super::*;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

pub struct OpenAiProvider {
    api_key: String,
    base_url: String,
    model: String,
    client: Client,
}

impl OpenAiProvider {
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        Self {
            api_key,
            base_url,
            model,
            client: Client::new(),
        }
    }
}

// ============================================================================
// OpenAI API Types
// ============================================================================

#[derive(Debug, Serialize)]
struct OpenAiRequest {
    model: String,
    messages: Vec<OpenAiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
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
struct OpenAiMessage {
    role: String,
    content: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OpenAiStreamChunk {
    choices: Vec<OpenAiStreamChoice>,
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiStreamChoice {
    delta: OpenAiStreamDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiStreamDelta {
    content: Option<String>,
    tool_calls: Option<Vec<serde_json::Value>>,
}

// ============================================================================
// ModelProvider Implementation
// ============================================================================

#[async_trait]
impl ModelProvider for OpenAiProvider {
    async fn chat(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        let messages: Vec<OpenAiMessage> = request
            .messages
            .into_iter()
            .map(|m| OpenAiMessage {
                role: m.role,
                content: m.content,
                name: m.name,
                tool_call_id: m.tool_call_id,
                tool_calls: m.tool_calls,
            })
            .collect();

        let body = OpenAiRequest {
            model: request.model,
            messages,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            stream: None,
            tools: request.tools,
            tool_choice: request.tool_choice,
        };

        let resp = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("OpenAI API error ({}): {}", status, text);
        }

        let api_resp: OpenAiResponse = resp.json().await?;

        let choice = api_resp
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No choices in OpenAI response"))?;

        let mut content = Vec::new();

        // Parse text content
        if let Some(text) = choice.message.content.as_str() {
            if !text.is_empty() {
                content.push(ContentBlock::Text(text.to_string()));
            }
        }

        // Parse tool calls
        if let Some(tool_calls) = choice.message.tool_calls {
            for tc in tool_calls {
                if let (Some(id), Some(function)) = (
                    tc.get("id").and_then(|v| v.as_str()),
                    tc.get("function"),
                ) {
                    let name = function
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let arguments = function
                        .get("arguments")
                        .and_then(|v| v.as_str())
                        .unwrap_or("{}");
                    let input: serde_json::Value =
                        serde_json::from_str(arguments).unwrap_or(serde_json::Value::Object(
                            serde_json::Map::new(),
                        ));
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
        });

        Ok(ProviderResponse {
            content,
            stop_reason: choice.finish_reason,
            usage: crate::gateway::TokenUsage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                cache_read_tokens: None,
                cache_write_tokens: None,
            },
        })
    }

    async fn stream_chat(&self, request: ProviderRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        let (tx, rx) = mpsc::channel(256);

        let messages: Vec<OpenAiMessage> = request
            .messages
            .into_iter()
            .map(|m| OpenAiMessage {
                role: m.role,
                content: m.content,
                name: m.name,
                tool_call_id: m.tool_call_id,
                tool_calls: m.tool_calls,
            })
            .collect();

        let body = OpenAiRequest {
            model: request.model,
            messages,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            stream: Some(true),
            tools: request.tools,
            tool_choice: request.tool_choice,
        };

        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();

        tokio::spawn(async move {
            let resp = match client
                .post(format!("{}/chat/completions", base_url))
                .header("Authorization", format!("Bearer {}", api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(StreamEvent::Error(format!("Request failed: {}", e))).await;
                    return;
                }
            };

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                let _ = tx
                    .send(StreamEvent::Error(format!(
                        "OpenAI API error ({}): {}",
                        status, text
                    )))
                    .await;
                return;
            }

            let text = match resp.text().await {
                Ok(t) => t,
                Err(e) => {
                    let _ = tx
                        .send(StreamEvent::Error(format!("Failed to read response: {}", e)))
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
                if let Some(data) = line.strip_prefix("data: ") {
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

    fn name(&self) -> &str {
        "openai"
    }
}
