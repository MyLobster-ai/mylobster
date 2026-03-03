//! Slack channel actions tool.
//!
//! Supports: sendMessage, editMessage, deleteMessage, react,
//! readMessages, downloadFile, pinMessage, unpinMessage, listPins,
//! getMemberInfo, listEmojis.

use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use async_trait::async_trait;

pub struct SlackActionsTool;

#[async_trait]
impl AgentTool for SlackActionsTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "slack".to_string(),
            description: "Perform Slack actions: send/edit/delete messages, react, read messages, manage pins, get member info".to_string(),
            category: "slack".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": [
                            "sendMessage", "editMessage", "deleteMessage",
                            "react", "readMessages", "downloadFile",
                            "pinMessage", "unpinMessage", "listPins",
                            "getMemberInfo", "listEmojis"
                        ]
                    },
                    "channel": { "type": "string", "description": "Slack channel ID" },
                    "text": { "type": "string" },
                    "ts": { "type": "string", "description": "Message timestamp" },
                    "emoji": { "type": "string", "description": "Emoji name (without colons)" },
                    "userId": { "type": "string" },
                    "fileUrl": { "type": "string" },
                    "threadTs": { "type": "string", "description": "Thread timestamp for replies" },
                    "limit": { "type": "integer", "default": 20 },
                    "blocks": { "type": "array", "description": "Block Kit blocks" }
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

        let bot_token = context
            .config
            .channels
            .slack
            .default_account
            .bot_token
            .clone()
            .or_else(|| std::env::var("SLACK_BOT_TOKEN").ok())
            .ok_or_else(|| anyhow::anyhow!("No Slack bot token configured"))?;

        let client = reqwest::Client::new();
        let base_url = "https://slack.com/api";

        match action {
            "sendMessage" => {
                let channel = get_str(&params, "channel")?;
                let text = get_str(&params, "text")?;

                let mut body = serde_json::json!({
                    "channel": channel,
                    "text": text
                });

                if let Some(thread_ts) = params.get("threadTs").and_then(|v| v.as_str()) {
                    body["thread_ts"] = serde_json::json!(thread_ts);
                }

                if let Some(blocks) = params.get("blocks") {
                    body["blocks"] = blocks.clone();
                }

                let resp = client
                    .post(format!("{}/chat.postMessage", base_url))
                    .header("Authorization", format!("Bearer {}", bot_token))
                    .json(&body)
                    .send()
                    .await?;

                let result: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(result))
            }
            "editMessage" => {
                let channel = get_str(&params, "channel")?;
                let ts = get_str(&params, "ts")?;
                let text = get_str(&params, "text")?;

                let resp = client
                    .post(format!("{}/chat.update", base_url))
                    .header("Authorization", format!("Bearer {}", bot_token))
                    .json(&serde_json::json!({
                        "channel": channel,
                        "ts": ts,
                        "text": text
                    }))
                    .send()
                    .await?;

                let result: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(result))
            }
            "deleteMessage" => {
                let channel = get_str(&params, "channel")?;
                let ts = get_str(&params, "ts")?;

                let resp = client
                    .post(format!("{}/chat.delete", base_url))
                    .header("Authorization", format!("Bearer {}", bot_token))
                    .json(&serde_json::json!({
                        "channel": channel,
                        "ts": ts
                    }))
                    .send()
                    .await?;

                let result: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(result))
            }
            "react" => {
                let channel = get_str(&params, "channel")?;
                let ts = get_str(&params, "ts")?;
                let emoji = get_str(&params, "emoji")?;

                let resp = client
                    .post(format!("{}/reactions.add", base_url))
                    .header("Authorization", format!("Bearer {}", bot_token))
                    .json(&serde_json::json!({
                        "channel": channel,
                        "timestamp": ts,
                        "name": emoji
                    }))
                    .send()
                    .await?;

                let result: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(result))
            }
            "readMessages" => {
                let channel = get_str(&params, "channel")?;
                let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(20);

                let resp = client
                    .get(format!("{}/conversations.history", base_url))
                    .header("Authorization", format!("Bearer {}", bot_token))
                    .query(&[
                        ("channel", channel.as_str()),
                        ("limit", &limit.to_string()),
                    ])
                    .send()
                    .await?;

                let result: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(result))
            }
            "pinMessage" => {
                let channel = get_str(&params, "channel")?;
                let ts = get_str(&params, "ts")?;

                let resp = client
                    .post(format!("{}/pins.add", base_url))
                    .header("Authorization", format!("Bearer {}", bot_token))
                    .json(&serde_json::json!({
                        "channel": channel,
                        "timestamp": ts
                    }))
                    .send()
                    .await?;

                let result: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(result))
            }
            "unpinMessage" => {
                let channel = get_str(&params, "channel")?;
                let ts = get_str(&params, "ts")?;

                let resp = client
                    .post(format!("{}/pins.remove", base_url))
                    .header("Authorization", format!("Bearer {}", bot_token))
                    .json(&serde_json::json!({
                        "channel": channel,
                        "timestamp": ts
                    }))
                    .send()
                    .await?;

                let result: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(result))
            }
            "listPins" => {
                let channel = get_str(&params, "channel")?;

                let resp = client
                    .get(format!("{}/pins.list", base_url))
                    .header("Authorization", format!("Bearer {}", bot_token))
                    .query(&[("channel", channel.as_str())])
                    .send()
                    .await?;

                let result: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(result))
            }
            "getMemberInfo" => {
                let user_id = get_str(&params, "userId")?;

                let resp = client
                    .get(format!("{}/users.info", base_url))
                    .header("Authorization", format!("Bearer {}", bot_token))
                    .query(&[("user", user_id.as_str())])
                    .send()
                    .await?;

                let result: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(result))
            }
            "listEmojis" => {
                let resp = client
                    .get(format!("{}/emoji.list", base_url))
                    .header("Authorization", format!("Bearer {}", bot_token))
                    .send()
                    .await?;

                let result: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(result))
            }
            "downloadFile" => {
                let file_url = get_str(&params, "fileUrl")?;

                let resp = client
                    .get(&file_url)
                    .header("Authorization", format!("Bearer {}", bot_token))
                    .send()
                    .await?;

                if !resp.status().is_success() {
                    return Ok(ToolResult::error(format!(
                        "Failed to download file: {}",
                        resp.status()
                    )));
                }

                let content_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/octet-stream")
                    .to_string();

                let bytes = resp.bytes().await?;

                Ok(ToolResult::json(serde_json::json!({
                    "size": bytes.len(),
                    "contentType": content_type,
                    "downloaded": true
                })))
            }
            _ => Ok(ToolResult::error(format!(
                "Unknown Slack action: {}",
                action
            ))),
        }
    }
}

fn get_str(params: &serde_json::Value, key: &str) -> Result<String> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter: {}", key))
}
