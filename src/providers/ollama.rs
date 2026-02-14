use super::*;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::debug;

pub struct OllamaProvider {
    base_url: String,
    model: String,
    api_key: Option<String>,
    client: Client,
}

impl OllamaProvider {
    pub fn new(base_url: String, model: String, api_key: Option<String>) -> Self {
        Self {
            base_url,
            model,
            api_key,
            client: Client::new(),
        }
    }

    /// Normalize the base URL: strip trailing `/v1` suffix since Ollama uses `/api/chat`.
    fn chat_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        let base = base.strip_suffix("/v1").unwrap_or(base);
        format!("{}/api/chat", base)
    }
}

// ============================================================================
// Ollama API Types
// ============================================================================

#[derive(Debug, Serialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<OllamaChatMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<OllamaOptions>,
}

#[derive(Debug, Serialize)]
struct OllamaOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<u64>,
    num_ctx: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaChatMessage {
    role: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaToolCall {
    function: OllamaToolFunction,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaToolFunction {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct OllamaChatResponse {
    message: Option<OllamaChatMessage>,
    done: bool,
    #[serde(default)]
    prompt_eval_count: Option<u64>,
    #[serde(default)]
    eval_count: Option<u64>,
}

// ============================================================================
// ModelProvider Implementation
// ============================================================================

fn convert_messages(messages: Vec<ProviderMessage>) -> Vec<OllamaChatMessage> {
    messages
        .into_iter()
        .map(|m| {
            let content = match &m.content {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            OllamaChatMessage {
                role: m.role,
                content,
                tool_calls: None,
            }
        })
        .collect()
}

fn build_request(request: &ProviderRequest, stream: bool) -> OllamaChatRequest {
    OllamaChatRequest {
        model: request.model.clone(),
        messages: convert_messages(request.messages.clone()),
        stream,
        tools: request.tools.clone(),
        options: Some(OllamaOptions {
            temperature: request.temperature,
            num_predict: request.max_tokens,
            num_ctx: 65536,
        }),
    }
}

fn parse_tool_calls(msg: &OllamaChatMessage) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();
    if let Some(tool_calls) = &msg.tool_calls {
        for (i, tc) in tool_calls.iter().enumerate() {
            blocks.push(ContentBlock::ToolUse {
                id: format!("call_{}", i),
                name: tc.function.name.clone(),
                input: tc.function.arguments.clone(),
            });
        }
    }
    blocks
}

#[async_trait]
impl ModelProvider for OllamaProvider {
    async fn chat(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        let body = build_request(&request, false);
        let url = self.chat_url();

        let mut req = self.client
            .post(&url)
            .header("Content-Type", "application/json");

        if let Some(ref key) = self.api_key {
            req = req.header("Authorization", format!("Bearer {}", key));
        }

        let resp = req.json(&body).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Ollama API error ({}): {}", status, text);
        }

        let api_resp: OllamaChatResponse = resp.json().await?;

        let mut content = Vec::new();
        if let Some(ref msg) = api_resp.message {
            if !msg.content.is_empty() {
                content.push(ContentBlock::Text(msg.content.clone()));
            }
            content.extend(parse_tool_calls(msg));
        }

        let stop_reason = if api_resp.done {
            Some("stop".to_string())
        } else {
            None
        };

        Ok(ProviderResponse {
            content,
            stop_reason,
            usage: crate::gateway::TokenUsage {
                input_tokens: api_resp.prompt_eval_count,
                output_tokens: api_resp.eval_count,
                cache_read_tokens: None,
                cache_write_tokens: None,
            },
        })
    }

    async fn stream_chat(&self, request: ProviderRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        let (tx, rx) = mpsc::channel(256);

        let body = build_request(&request, true);
        let url = self.chat_url();
        let client = self.client.clone();
        let api_key = self.api_key.clone();

        tokio::spawn(async move {
            let mut req = client
                .post(&url)
                .header("Content-Type", "application/json");

            if let Some(ref key) = api_key {
                req = req.header("Authorization", format!("Bearer {}", key));
            }

            let resp = match req.json(&body).send().await {
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
                        "Ollama API error ({}): {}",
                        status, text
                    )))
                    .await;
                return;
            }

            // Ollama streams NDJSON (one JSON object per line)
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
                if line.is_empty() {
                    continue;
                }

                match serde_json::from_str::<OllamaChatResponse>(line) {
                    Ok(chunk) => {
                        if let Some(ref msg) = chunk.message {
                            if !msg.content.is_empty() {
                                let _ = tx.send(StreamEvent::Delta(msg.content.clone())).await;
                            }
                            // Tool calls arrive in intermediate chunks
                            if let Some(ref tool_calls) = msg.tool_calls {
                                for tc in tool_calls {
                                    let _ = tx
                                        .send(StreamEvent::ToolCall(serde_json::json!({
                                            "function": {
                                                "name": tc.function.name,
                                                "arguments": tc.function.arguments.to_string()
                                            }
                                        })))
                                        .await;
                                }
                            }
                        }

                        if chunk.done {
                            total_input = chunk.prompt_eval_count;
                            total_output = chunk.eval_count;
                        }
                    }
                    Err(e) => {
                        debug!("Skipping unparseable Ollama NDJSON line: {}", e);
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
        "ollama"
    }
}
