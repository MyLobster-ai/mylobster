pub mod tools;

use crate::config::Config;
use crate::gateway::*;
use crate::providers::{ModelProvider, ProviderMessage, ProviderRequest};
use crate::sessions::SessionStore;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};
use uuid::Uuid;

// ============================================================================
// Agent Runtime
// ============================================================================

/// Run a single message through the agent pipeline (CLI mode).
pub async fn run_single_message(
    config: &Config,
    message: &str,
    session_key: Option<&str>,
) -> Result<()> {
    let model = config
        .agent
        .model
        .primary_model()
        .unwrap_or_else(|| "claude-opus-4-6".to_string());

    info!("Running agent with model: {}", model);

    let provider = crate::providers::resolve_provider(config, &model)?;

    let messages = vec![ProviderMessage {
        role: "user".to_string(),
        content: serde_json::Value::String(message.to_string()),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }];

    let request = ProviderRequest {
        model,
        messages,
        max_tokens: None,
        temperature: None,
        stream: false,
        tools: None,
        tool_choice: None,
    };

    let response = provider.chat(request).await?;
    println!("{}", response.content_text());

    Ok(())
}

/// Handle an OpenAI-compatible chat completion request.
pub async fn handle_chat_completion(
    config: &Config,
    sessions: &SessionStore,
    req: ChatCompletionRequest,
) -> Result<ChatCompletionResponse> {
    let provider = crate::providers::resolve_provider(config, &req.model)?;

    let messages: Vec<ProviderMessage> = req
        .messages
        .iter()
        .map(|m| ProviderMessage {
            role: m.role.clone(),
            content: m.content.clone(),
            name: m.name.clone(),
            tool_call_id: m.tool_call_id.clone(),
            tool_calls: m.tool_calls.clone(),
        })
        .collect();

    let request = ProviderRequest {
        model: req.model.clone(),
        messages,
        max_tokens: req.max_tokens,
        temperature: req.temperature,
        stream: false,
        tools: req.tools,
        tool_choice: req.tool_choice,
    };

    let response = provider.chat(request).await?;

    let completion = ChatCompletionResponse {
        id: format!("chatcmpl-{}", Uuid::new_v4()),
        object: "chat.completion".to_string(),
        created: chrono::Utc::now().timestamp() as u64,
        model: req.model,
        choices: vec![ChatCompletionChoice {
            index: 0,
            message: ChatCompletionMessage {
                role: "assistant".to_string(),
                content: serde_json::Value::String(response.content_text()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            finish_reason: Some(response.stop_reason.unwrap_or_else(|| "stop".to_string())),
        }],
        usage: Some(ChatCompletionUsage {
            prompt_tokens: response.usage.input_tokens.unwrap_or(0),
            completion_tokens: response.usage.output_tokens.unwrap_or(0),
            total_tokens: response.usage.input_tokens.unwrap_or(0)
                + response.usage.output_tokens.unwrap_or(0),
        }),
    };

    Ok(completion)
}

/// Handle an OpenResponses API request.
pub async fn handle_responses_api(
    config: &Config,
    sessions: &SessionStore,
    req: serde_json::Value,
) -> Result<serde_json::Value> {
    // Extract model and input from the request
    let model = req
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("claude-opus-4-6")
        .to_string();

    let input = req.get("input").cloned().unwrap_or(serde_json::Value::Null);

    let provider = crate::providers::resolve_provider(config, &model)?;

    let messages = match input {
        serde_json::Value::String(text) => vec![ProviderMessage {
            role: "user".to_string(),
            content: serde_json::Value::String(text),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        serde_json::Value::Array(arr) => arr
            .iter()
            .map(|m| ProviderMessage {
                role: m
                    .get("role")
                    .and_then(|v| v.as_str())
                    .unwrap_or("user")
                    .to_string(),
                content: m.get("content").cloned().unwrap_or(serde_json::Value::Null),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            })
            .collect(),
        _ => vec![],
    };

    let request = ProviderRequest {
        model: model.clone(),
        messages,
        max_tokens: req.get("max_output_tokens").and_then(|v| v.as_u64()),
        temperature: req.get("temperature").and_then(|v| v.as_f64()),
        stream: false,
        tools: None,
        tool_choice: None,
    };

    let response = provider.chat(request).await?;

    Ok(serde_json::json!({
        "id": format!("resp-{}", Uuid::new_v4()),
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "model": model,
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": response.content_text()
            }]
        }],
        "usage": {
            "input_tokens": response.usage.input_tokens.unwrap_or(0),
            "output_tokens": response.usage.output_tokens.unwrap_or(0),
        }
    }))
}
