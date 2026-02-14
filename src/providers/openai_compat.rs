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
            name: m.name,
            tool_call_id: m.tool_call_id,
            tool_calls: m.tool_calls,
        })
        .collect()
}

/// Build an OpenAI-compatible request body.
pub(crate) fn build_request(request: ProviderRequest, stream: bool) -> OpenAiRequest {
    OpenAiRequest {
        model: request.model,
        messages: convert_messages(request.messages),
        max_tokens: request.max_tokens,
        temperature: request.temperature,
        stream: if stream { Some(true) } else { None },
        tools: request.tools,
        tool_choice: request.tool_choice,
    }
}

/// Parse an OpenAI-compatible response into our ProviderResponse.
fn parse_response(api_resp: OpenAiResponse) -> Result<ProviderResponse> {
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
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("{} API error ({}): {}", provider_name, status, text);
    }

    let api_resp: OpenAiResponse = resp.json().await?;
    parse_response(api_resp)
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
