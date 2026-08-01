pub mod acp;
pub mod codex;
pub mod commitments;
pub mod compaction;
pub mod failover;
pub mod heartbeat;
pub mod model_fallback;
pub mod reply_policy;
pub mod reply_sanitize;
pub mod subagents;
pub mod tool_loop;
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
        .unwrap_or_else(|| "claude-sonnet-4-6".to_string());

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
        thinking: None,
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
        thinking: None,
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

/// Normalize a tool result to ensure it always has valid structure.
///
/// Guarantees:
/// - `text` is never `None` (falls back to empty string).
/// - `json` is valid (null → None).
/// - `is_error` is preserved.
///
/// This prevents malformed tool results from crashing downstream
/// serialization or provider formatting.
pub fn normalize_tool_result(result: tools::ToolResult) -> tools::ToolResult {
    let text = result.text.or_else(|| {
        // If json is present, stringify it as the text fallback.
        result
            .json
            .as_ref()
            .map(|j| serde_json::to_string(j).unwrap_or_default())
    });

    let text = text.or(Some(String::new()));

    // Validate JSON — if it's serde_json::Value::Null, treat as None.
    let json = result.json.and_then(|j| {
        if j.is_null() {
            None
        } else {
            Some(j)
        }
    });

    tools::ToolResult {
        text,
        json,
        image: result.image,
        is_error: result.is_error,
    }
}

// ============================================================================
// Per-agent config resolution (v2026.4.26 / v2026.5.2)
// ============================================================================

/// Find the `agents.list[]` entry for an agent id.
pub fn find_agent_entry<'a>(
    config: &'a Config,
    agent_id: &str,
) -> Option<&'a crate::config::types::AgentEntry> {
    config.agents.list.iter().find(|a| a.id == agent_id)
}

/// Resolve the effective TTS config for an agent (v2026.4.26
/// `agents.list[].tts`): the per-agent override wins; otherwise the global
/// `tts` config applies. The TTS pipeline consumes the returned config.
pub fn resolve_agent_tts<'a>(
    config: &'a Config,
    agent_id: &str,
) -> &'a crate::config::types::TtsConfig {
    find_agent_entry(config, agent_id)
        .and_then(|entry| entry.tts.as_ref())
        .unwrap_or(&config.tts)
}

/// Whether optional workspace bootstrap files (TOOLS.md and friends) should
/// be skipped when building agent bootstrap context (v2026.5.2
/// `agents.defaults.skipOptionalBootstrapFiles`).
pub fn skip_optional_bootstrap_files(config: &Config) -> bool {
    config.agent.skip_optional_bootstrap_files.unwrap_or(false)
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
        .unwrap_or("claude-sonnet-4-6")
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
        thinking: None,
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

#[cfg(test)]
mod agent_config_tests {
    use super::*;
    use crate::config::types::{AgentEntry, TtsConfig};

    #[test]
    fn resolve_agent_tts_prefers_per_agent_override() {
        let mut config = Config::default();
        config.tts.provider = None;
        config.agents.list.push(AgentEntry {
            id: "fany".into(),
            tts: Some(TtsConfig {
                enabled: Some(true),
                mode: Some("voice".into()),
                ..Default::default()
            }),
            ..Default::default()
        });

        let tts = resolve_agent_tts(&config, "fany");
        assert_eq!(tts.mode.as_deref(), Some("voice"));
        assert_eq!(tts.enabled, Some(true));
    }

    #[test]
    fn resolve_agent_tts_falls_back_to_global() {
        let mut config = Config::default();
        config.tts.mode = Some("global-mode".into());
        config.agents.list.push(AgentEntry {
            id: "no-override".into(),
            ..Default::default()
        });

        // Agent without override → global config.
        assert_eq!(
            resolve_agent_tts(&config, "no-override").mode.as_deref(),
            Some("global-mode")
        );
        // Unknown agent → global config.
        assert_eq!(
            resolve_agent_tts(&config, "missing").mode.as_deref(),
            Some("global-mode")
        );
    }

    #[test]
    fn skip_optional_bootstrap_files_defaults_off() {
        let mut config = Config::default();
        assert!(!skip_optional_bootstrap_files(&config));
        config.agent.skip_optional_bootstrap_files = Some(true);
        assert!(skip_optional_bootstrap_files(&config));
    }

    #[test]
    fn find_agent_entry_by_id() {
        let mut config = Config::default();
        config.agents.list.push(AgentEntry {
            id: "a".into(),
            ..Default::default()
        });
        assert!(find_agent_entry(&config, "a").is_some());
        assert!(find_agent_entry(&config, "b").is_none());
    }
}
