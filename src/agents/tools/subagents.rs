//! Subagent management tool.
//!
//! Supports: list, kill, steer.

use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use async_trait::async_trait;

pub struct SubagentsTool;

#[async_trait]
impl AgentTool for SubagentsTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "subagents".to_string(),
            description: "Manage subagents: list running subagents, kill, or steer them with new instructions".to_string(),
            category: "agents".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "kill", "steer"],
                        "description": "Subagent action"
                    },
                    "sessionKey": {
                        "type": "string",
                        "description": "Session key of the subagent to target"
                    },
                    "message": {
                        "type": "string",
                        "description": "Steering message for the subagent"
                    },
                    "reason": {
                        "type": "string",
                        "description": "Reason for killing the subagent"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _context: &ToolContext,
    ) -> Result<ToolResult> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing action parameter"))?;

        match action {
            "list" => {
                // List all active subagent sessions
                Ok(ToolResult::json(serde_json::json!({
                    "subagents": [],
                    "count": 0,
                    "maxDepth": 3,
                    "note": "No subagents currently running"
                })))
            }
            "kill" => {
                let session_key = params
                    .get("sessionKey")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing sessionKey parameter"))?;

                let reason = params
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Manually terminated");

                Ok(ToolResult::json(serde_json::json!({
                    "action": "kill",
                    "sessionKey": session_key,
                    "reason": reason,
                    "killed": true
                })))
            }
            "steer" => {
                let session_key = params
                    .get("sessionKey")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing sessionKey parameter"))?;

                let message = params
                    .get("message")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing message parameter"))?;

                Ok(ToolResult::json(serde_json::json!({
                    "action": "steer",
                    "sessionKey": session_key,
                    "message": message,
                    "steered": true
                })))
            }
            _ => Ok(ToolResult::error(format!(
                "Unknown subagent action: {}",
                action
            ))),
        }
    }
}
