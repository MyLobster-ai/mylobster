//! WhatsApp channel actions tool.
//!
//! Supports: react, sendMessage with target auth and allowlist.

use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use async_trait::async_trait;

pub struct WhatsAppActionsTool;

#[async_trait]
impl AgentTool for WhatsAppActionsTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "whatsapp".to_string(),
            description: "Perform WhatsApp actions: send messages, react to messages".to_string(),
            category: "whatsapp".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["sendMessage", "react"],
                        "description": "The WhatsApp action to perform"
                    },
                    "to": { "type": "string", "description": "Phone number (E.164 format)" },
                    "text": { "type": "string", "description": "Message text" },
                    "messageId": { "type": "string", "description": "Message ID for reactions" },
                    "emoji": { "type": "string", "description": "Emoji for reaction" }
                },
                "required": ["action", "to"]
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

        let to = params
            .get("to")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'to' parameter"))?;

        let api_token = std::env::var("WHATSAPP_API_TOKEN")
            .ok()
            .ok_or_else(|| anyhow::anyhow!("No WhatsApp API token configured (WHATSAPP_API_TOKEN)"))?;

        let phone_id = std::env::var("WHATSAPP_PHONE_NUMBER_ID")
            .ok()
            .ok_or_else(|| anyhow::anyhow!("No WhatsApp phone number ID configured (WHATSAPP_PHONE_NUMBER_ID)"))?;

        // Allowlist check
        if let Some(ref allowlist) = context.config.channels.whatsapp.default_account.allow_from {
            if !allowlist.is_empty() && !allowlist.contains(&to.to_string()) {
                return Ok(ToolResult::error(format!(
                    "Phone number {} is not in the WhatsApp allowlist",
                    to
                )));
            }
        }

        let client = reqwest::Client::new();
        let base_url = format!(
            "https://graph.facebook.com/v18.0/{}/messages",
            phone_id
        );

        match action {
            "sendMessage" => {
                let text = params
                    .get("text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing text parameter"))?;

                let resp = client
                    .post(&base_url)
                    .header("Authorization", format!("Bearer {}", api_token))
                    .json(&serde_json::json!({
                        "messaging_product": "whatsapp",
                        "to": to,
                        "type": "text",
                        "text": { "body": text }
                    }))
                    .send()
                    .await?;

                let result: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(result))
            }
            "react" => {
                let message_id = params
                    .get("messageId")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing messageId parameter"))?;
                let emoji = params
                    .get("emoji")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing emoji parameter"))?;

                let resp = client
                    .post(&base_url)
                    .header("Authorization", format!("Bearer {}", api_token))
                    .json(&serde_json::json!({
                        "messaging_product": "whatsapp",
                        "to": to,
                        "type": "reaction",
                        "reaction": {
                            "message_id": message_id,
                            "emoji": emoji
                        }
                    }))
                    .send()
                    .await?;

                let result: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(result))
            }
            _ => Ok(ToolResult::error(format!(
                "Unknown WhatsApp action: {}",
                action
            ))),
        }
    }
}
