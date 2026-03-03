//! Telegram channel actions tool.
//!
//! Supports multiple Telegram actions: sendMessage, editMessage, deleteMessage,
//! react, sendSticker, sendPhoto, sendDocument, sendVideo, createForumTopic.

use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use async_trait::async_trait;

pub struct TelegramActionsTool;

#[async_trait]
impl AgentTool for TelegramActionsTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "telegram".to_string(),
            description: "Perform Telegram actions: send/edit/delete messages, react, send media, manage forum topics".to_string(),
            category: "telegram".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": [
                            "sendMessage", "editMessage", "deleteMessage",
                            "react", "sendSticker", "sendPhoto",
                            "sendDocument", "sendVideo", "createForumTopic"
                        ],
                        "description": "The Telegram action to perform"
                    },
                    "chatId": { "type": "string", "description": "Telegram chat ID" },
                    "messageId": { "type": "integer", "description": "Message ID" },
                    "text": { "type": "string", "description": "Message text" },
                    "emoji": { "type": "string", "description": "Emoji for reaction" },
                    "stickerId": { "type": "string", "description": "Sticker file_id" },
                    "filePath": { "type": "string", "description": "Local file path for media" },
                    "fileUrl": { "type": "string", "description": "URL for media" },
                    "caption": { "type": "string", "description": "Media caption" },
                    "topicName": { "type": "string", "description": "Forum topic name" },
                    "parseMode": {
                        "type": "string",
                        "enum": ["HTML", "Markdown", "MarkdownV2"],
                        "default": "HTML"
                    },
                    "replyToMessageId": { "type": "integer" },
                    "inlineKeyboard": {
                        "type": "array",
                        "description": "Inline keyboard rows",
                        "items": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "text": { "type": "string" },
                                    "url": { "type": "string" },
                                    "callbackData": { "type": "string" }
                                }
                            }
                        }
                    }
                },
                "required": ["action", "chatId"]
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

        let chat_id = params
            .get("chatId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing chatId parameter"))?;

        let bot_token = context
            .config
            .channels
            .telegram
            .default_account
            .bot_token
            .clone()
            .or_else(|| std::env::var("TELEGRAM_BOT_TOKEN").ok())
            .ok_or_else(|| anyhow::anyhow!("No Telegram bot token configured"))?;

        let client = reqwest::Client::new();
        let base_url = format!("https://api.telegram.org/bot{}", bot_token);

        match action {
            "sendMessage" => {
                let text = get_str(&params, "text")?;
                let parse_mode = params
                    .get("parseMode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("HTML");

                let mut body = serde_json::json!({
                    "chat_id": chat_id,
                    "text": text,
                    "parse_mode": parse_mode
                });

                if let Some(reply_to) = params.get("replyToMessageId") {
                    body["reply_to_message_id"] = reply_to.clone();
                }

                if let Some(keyboard) = params.get("inlineKeyboard") {
                    body["reply_markup"] = serde_json::json!({
                        "inline_keyboard": keyboard
                    });
                }

                let resp = client
                    .post(format!("{}/sendMessage", base_url))
                    .json(&body)
                    .send()
                    .await?;

                let result: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(result))
            }
            "editMessage" => {
                let message_id = params
                    .get("messageId")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| anyhow::anyhow!("Missing messageId"))?;
                let text = get_str(&params, "text")?;

                let resp = client
                    .post(format!("{}/editMessageText", base_url))
                    .json(&serde_json::json!({
                        "chat_id": chat_id,
                        "message_id": message_id,
                        "text": text,
                        "parse_mode": "HTML"
                    }))
                    .send()
                    .await?;

                let result: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(result))
            }
            "deleteMessage" => {
                let message_id = params
                    .get("messageId")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| anyhow::anyhow!("Missing messageId"))?;

                let resp = client
                    .post(format!("{}/deleteMessage", base_url))
                    .json(&serde_json::json!({
                        "chat_id": chat_id,
                        "message_id": message_id
                    }))
                    .send()
                    .await?;

                let result: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(result))
            }
            "react" => {
                let message_id = params
                    .get("messageId")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| anyhow::anyhow!("Missing messageId"))?;
                let emoji = get_str(&params, "emoji")?;

                let resp = client
                    .post(format!("{}/setMessageReaction", base_url))
                    .json(&serde_json::json!({
                        "chat_id": chat_id,
                        "message_id": message_id,
                        "reaction": [{ "type": "emoji", "emoji": emoji }]
                    }))
                    .send()
                    .await?;

                let result: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(result))
            }
            "sendSticker" => {
                let sticker_id = get_str(&params, "stickerId")?;

                let resp = client
                    .post(format!("{}/sendSticker", base_url))
                    .json(&serde_json::json!({
                        "chat_id": chat_id,
                        "sticker": sticker_id
                    }))
                    .send()
                    .await?;

                let result: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(result))
            }
            "sendPhoto" => {
                let file_url = params
                    .get("fileUrl")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing fileUrl for sendPhoto"))?;

                let mut body = serde_json::json!({
                    "chat_id": chat_id,
                    "photo": file_url
                });

                if let Some(caption) = params.get("caption").and_then(|v| v.as_str()) {
                    body["caption"] = serde_json::json!(caption);
                }

                let resp = client
                    .post(format!("{}/sendPhoto", base_url))
                    .json(&body)
                    .send()
                    .await?;

                let result: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(result))
            }
            "sendDocument" => {
                let file_url = params
                    .get("fileUrl")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing fileUrl for sendDocument"))?;

                let mut body = serde_json::json!({
                    "chat_id": chat_id,
                    "document": file_url
                });

                if let Some(caption) = params.get("caption").and_then(|v| v.as_str()) {
                    body["caption"] = serde_json::json!(caption);
                }

                let resp = client
                    .post(format!("{}/sendDocument", base_url))
                    .json(&body)
                    .send()
                    .await?;

                let result: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(result))
            }
            "sendVideo" => {
                let file_url = params
                    .get("fileUrl")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing fileUrl for sendVideo"))?;

                let mut body = serde_json::json!({
                    "chat_id": chat_id,
                    "video": file_url
                });

                if let Some(caption) = params.get("caption").and_then(|v| v.as_str()) {
                    body["caption"] = serde_json::json!(caption);
                }

                let resp = client
                    .post(format!("{}/sendVideo", base_url))
                    .json(&body)
                    .send()
                    .await?;

                let result: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(result))
            }
            "createForumTopic" => {
                let topic_name = get_str(&params, "topicName")?;

                let resp = client
                    .post(format!("{}/createForumTopic", base_url))
                    .json(&serde_json::json!({
                        "chat_id": chat_id,
                        "name": topic_name
                    }))
                    .send()
                    .await?;

                let result: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(result))
            }
            _ => Ok(ToolResult::error(format!(
                "Unknown Telegram action: {}",
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
