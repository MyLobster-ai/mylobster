//! Multi-step nested reasoning tool.
//!
//! Spawns a synchronous nested agent run for complex multi-step tasks.

use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use async_trait::async_trait;

pub struct AgentStepTool;

#[async_trait]
impl AgentTool for AgentStepTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "agent_step".to_string(),
            description: "Run a nested agent step for complex reasoning tasks. Spawns a sub-agent with its own context and returns the result.".to_string(),
            category: "agents".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "The task or question for the sub-agent"
                    },
                    "model": {
                        "type": "string",
                        "description": "Override model for this step"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Timeout in seconds",
                        "default": 120
                    },
                    "context": {
                        "type": "string",
                        "description": "Additional context to provide"
                    },
                    "idempotencyKey": {
                        "type": "string",
                        "description": "Idempotency key to prevent duplicate execution"
                    }
                },
                "required": ["message"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult> {
        let message = params
            .get("message")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing message parameter"))?;

        let default_model = "claude-sonnet-4-20250514";
        let config_model = context.config.agent.model.primary_model();
        let model = params
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or(config_model)
            .unwrap_or_else(|| default_model.to_string());

        let timeout = params
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(120);

        let additional_context = params
            .get("context")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let full_prompt = if additional_context.is_empty() {
            message.to_string()
        } else {
            format!("{}\n\nContext:\n{}", message, additional_context)
        };

        // Build a provider request for nested reasoning
        let provider = crate::providers::resolve_provider(
            &context.config,
            &model,
        )?;

        let request = crate::providers::ProviderRequest {
            model: model.clone(),
            messages: vec![crate::providers::ProviderMessage {
                role: "user".to_string(),
                content: serde_json::json!(full_prompt),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: Some(4096),
            temperature: Some(0.0),
            stream: false,
            tools: None,
            tool_choice: None,
            thinking: None,
        };

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(timeout),
            provider.chat(request),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Agent step timed out after {}s", timeout))??;

        let text = response.content_text();

        Ok(ToolResult::json(serde_json::json!({
            "result": text,
            "model": model,
            "usage": {
                "inputTokens": response.usage.input_tokens,
                "outputTokens": response.usage.output_tokens
            },
            "parentSession": context.session_key
        })))
    }
}
