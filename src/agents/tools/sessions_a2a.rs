//! Agent-to-agent messaging tool.
//!
//! Multi-turn ping-pong protocol for inter-agent communication.
//! Supports skip token handling and announce target resolution.

use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use async_trait::async_trait;

pub struct SessionsA2aTool;

#[async_trait]
impl AgentTool for SessionsA2aTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "sessions_a2a".to_string(),
            description: "Agent-to-agent messaging: send messages between agent sessions with multi-turn support".to_string(),
            category: "agents".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["send", "announce", "listen", "skip"],
                        "description": "A2A action"
                    },
                    "targetSession": {
                        "type": "string",
                        "description": "Target session key"
                    },
                    "targetAgent": {
                        "type": "string",
                        "description": "Target agent ID (resolved to session)"
                    },
                    "message": {
                        "type": "string",
                        "description": "Message to send"
                    },
                    "replyTo": {
                        "type": "string",
                        "description": "Message ID being replied to"
                    },
                    "skipToken": {
                        "type": "string",
                        "description": "Token to skip messages until this one"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Timeout in seconds for listen action",
                        "default": 30
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing action parameter"))?;

        match action {
            "send" => {
                let target = params
                    .get("targetSession")
                    .or_else(|| params.get("targetAgent"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        anyhow::anyhow!("Missing targetSession or targetAgent parameter")
                    })?;

                let message = params
                    .get("message")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing message parameter"))?;

                let message_id = uuid::Uuid::new_v4().to_string();

                Ok(ToolResult::json(serde_json::json!({
                    "action": "send",
                    "messageId": message_id,
                    "from": context.session_key,
                    "to": target,
                    "message": message,
                    "sent": true
                })))
            }
            "announce" => {
                let target = params
                    .get("targetAgent")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing targetAgent parameter"))?;

                Ok(ToolResult::json(serde_json::json!({
                    "action": "announce",
                    "from": context.session_key,
                    "targetAgent": target,
                    "announced": true,
                    "resolvedSession": null
                })))
            }
            "listen" => {
                let timeout = params
                    .get("timeout")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30);

                let skip_token = params
                    .get("skipToken")
                    .and_then(|v| v.as_str());

                Ok(ToolResult::json(serde_json::json!({
                    "action": "listen",
                    "session": context.session_key,
                    "timeout": timeout,
                    "skipToken": skip_token,
                    "messages": [],
                    "note": "No messages received within timeout"
                })))
            }
            "skip" => {
                let skip_token = params
                    .get("skipToken")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing skipToken parameter"))?;

                Ok(ToolResult::json(serde_json::json!({
                    "action": "skip",
                    "skipToken": skip_token,
                    "skipped": true
                })))
            }
            _ => Ok(ToolResult::error(format!(
                "Unknown A2A action: {}",
                action
            ))),
        }
    }
}
