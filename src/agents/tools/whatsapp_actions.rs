//! WhatsApp channel actions tool.
//!
//! Supports: `sendMessage` and `react`, with target normalization and the
//! allowFrom policy ported from OpenClaw
//! `extensions/whatsapp/src/resolve-outbound-target.ts` /
//! `channel-actions.ts` (v2026.7.1): targets are normalized to canonical
//! form (E.164 / group JID / newsletter JID), group and newsletter targets
//! bypass the DM allowlist, and `@newsletter` targets are classified as
//! channel conversations rather than DMs.
//!
//! Transport note: this tool speaks the WhatsApp Cloud (Graph) API, which can
//! only address direct user targets. Group/newsletter targets resolve and
//! classify correctly but require the Web-socket transport, so they are
//! rejected here with an explanatory error instead of being silently
//! mis-routed as DMs.

use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use crate::channels::whatsapp::{
    resolve_whatsapp_outbound_target, WhatsAppChatType,
};
use anyhow::Result;
use async_trait::async_trait;

/// Convert a resolved canonical user target (`+E.164`) into the recipient
/// format the Graph API expects (digits, no leading `+`).
fn graph_api_recipient(canonical_target: &str) -> String {
    canonical_target
        .strip_prefix('+')
        .unwrap_or(canonical_target)
        .to_string()
}

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
                    "to": { "type": "string", "description": "Phone number (E.164), group JID (…@g.us), or newsletter JID (…@newsletter)" },
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

        // Normalize the target and enforce the allowFrom policy exactly like
        // the channel send path (groups/newsletters bypass the DM allowlist).
        let allow_from = context
            .config
            .channels
            .whatsapp
            .default_account
            .allow_from
            .clone();
        let resolution =
            match resolve_whatsapp_outbound_target(Some(to), allow_from.as_deref()) {
                Ok(resolution) => resolution,
                Err(message) => return Ok(ToolResult::error(message)),
            };

        // The Cloud API transport is DM-only; group/newsletter targets need
        // the Web-socket transport.
        match resolution.chat_type {
            WhatsAppChatType::Direct => {}
            WhatsAppChatType::Group => {
                return Ok(ToolResult::error(format!(
                    "Group target {} requires the WhatsApp Web session transport; \
                     the Cloud API tool can only message direct contacts",
                    resolution.to
                )));
            }
            WhatsAppChatType::Newsletter => {
                return Ok(ToolResult::error(format!(
                    "Newsletter (channel) target {} requires the WhatsApp Web session \
                     transport; the Cloud API tool can only message direct contacts",
                    resolution.to
                )));
            }
        }
        let recipient = graph_api_recipient(&resolution.to);

        let api_token = std::env::var("WHATSAPP_API_TOKEN")
            .ok()
            .ok_or_else(|| anyhow::anyhow!("No WhatsApp API token configured (WHATSAPP_API_TOKEN)"))?;

        let phone_id = std::env::var("WHATSAPP_PHONE_NUMBER_ID")
            .ok()
            .ok_or_else(|| anyhow::anyhow!("No WhatsApp phone number ID configured (WHATSAPP_PHONE_NUMBER_ID)"))?;

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
                        "to": recipient,
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
                        "to": recipient,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_recipient_strips_plus() {
        assert_eq!(graph_api_recipient("+15551234567"), "15551234567");
        assert_eq!(graph_api_recipient("15551234567"), "15551234567");
    }

    #[test]
    fn tool_targets_resolve_through_channel_normalization() {
        // Sanity: the same resolver the channel path uses accepts JID input
        // and enforces the allowlist for the tool.
        let allow = vec!["15551234567".to_string()];
        let ok = resolve_whatsapp_outbound_target(
            Some("15551234567@s.whatsapp.net"),
            Some(&allow),
        )
        .unwrap();
        assert_eq!(ok.to, "+15551234567");
        assert_eq!(ok.chat_type, WhatsAppChatType::Direct);
        assert!(
            resolve_whatsapp_outbound_target(Some("+19998887777"), Some(&allow)).is_err()
        );
    }
}
