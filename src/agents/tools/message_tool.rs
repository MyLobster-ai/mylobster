//! Message sending tool — dispatch messages to any channel.

use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use async_trait::async_trait;

/// Send a formatted message to any configured channel.
pub struct MessageSendTool;

#[async_trait]
impl AgentTool for MessageSendTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "message_send".to_string(),
            description: "Send a message to a specific channel (telegram, discord, slack, whatsapp, signal, imessage)".to_string(),
            category: "messaging".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "channel": {
                        "type": "string",
                        "description": "Target channel (telegram, discord, slack, whatsapp, signal, imessage)",
                        "enum": ["telegram", "discord", "slack", "whatsapp", "signal", "imessage", "synology_chat"]
                    },
                    "to": {
                        "type": "string",
                        "description": "Recipient identifier (chat ID, channel ID, phone number, etc.)"
                    },
                    "text": {
                        "type": "string",
                        "description": "Message text to send"
                    },
                    "format": {
                        "type": "string",
                        "enum": ["text", "markdown", "html"],
                        "description": "Message format",
                        "default": "text"
                    }
                },
                "required": ["channel", "to", "text"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult> {
        let channel = params
            .get("channel")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing channel parameter"))?;

        let to = params
            .get("to")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing to parameter"))?;

        let text = params
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing text parameter"))?;

        tracing::info!(channel, to, chars = text.len(), "sending message via tool");

        match crate::channels::send_message(&context.config, channel, to, text).await {
            Ok(()) => Ok(ToolResult::json(serde_json::json!({
                "sent": true,
                "channel": channel,
                "to": to,
                "chars": text.len()
            }))),
            Err(e) => Ok(ToolResult::error(format!(
                "Failed to send message via {}: {}",
                channel, e
            ))),
        }
    }
}
