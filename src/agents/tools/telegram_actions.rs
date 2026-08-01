//! Telegram channel actions tool.
//!
//! Supports multiple Telegram actions: sendMessage, editMessage, deleteMessage,
//! react, sendSticker, sendPhoto, sendDocument, sendVideo, createForumTopic.
//!
//! v2026.5.2 behavior: all Bot API calls go through `TelegramApi`, which
//! applies the upstream per-method timeout guards (60 s outbound text, 30 s
//! media, 15 s control-plane). Long `sendMessage` text is split into safe
//! HTML chunks with plain-text fallback; media text over the 1024-char caption
//! limit is sent as a chunked follow-up message; `editMessage` is durable
//! (benign "not modified" / "no text" 400s treated as no-op); benign
//! `deleteMessage` 400s are no-op warnings instead of failures.

use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use crate::channels::telegram::{DurableEditOutcome, TelegramApi};
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

        let api = TelegramApi::from_account(&context.config.channels.telegram.default_account)
            .ok_or_else(|| anyhow::anyhow!("No Telegram bot token configured"))?;

        match action {
            "sendMessage" => {
                let text = get_str(&params, "text")?;

                // Long messages are split into safe HTML chunks with a
                // plain-text fallback per chunk; short messages take the same
                // path (single chunk).
                let reply_markup = params.get("inlineKeyboard").map(|keyboard| {
                    serde_json::json!({ "inline_keyboard": keyboard })
                });
                let last_id = api
                    .send_message_chunked(chat_id, &text, None, reply_markup.as_ref())
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                Ok(ToolResult::json(serde_json::json!({
                    "ok": true,
                    "messageId": last_id,
                })))
            }
            "editMessage" => {
                let message_id = params
                    .get("messageId")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| anyhow::anyhow!("Missing messageId"))?;
                let text = get_str(&params, "text")?;

                let outcome = api
                    .edit_message_durable(chat_id, message_id, &text)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                Ok(ToolResult::json(serde_json::json!({
                    "ok": true,
                    "outcome": match outcome {
                        DurableEditOutcome::Edited => "edited",
                        DurableEditOutcome::NotModified => "not_modified",
                        DurableEditOutcome::NoTextToEdit => "no_text_to_edit",
                        DurableEditOutcome::MessageGone => "message_gone",
                    },
                })))
            }
            "deleteMessage" => {
                let message_id = params
                    .get("messageId")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| anyhow::anyhow!("Missing messageId"))?;

                // Benign 400s (already deleted / can't be deleted / forbidden)
                // are no-op warnings, not failures.
                let deleted = api
                    .delete_message_benign(chat_id, message_id)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                Ok(ToolResult::json(serde_json::json!({
                    "ok": true,
                    "deleted": deleted,
                })))
            }
            "react" => {
                let message_id = params
                    .get("messageId")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| anyhow::anyhow!("Missing messageId"))?;
                let emoji = get_str(&params, "emoji")?;

                let result = api
                    .call(
                        "setMessageReaction",
                        &serde_json::json!({
                            "chat_id": chat_id,
                            "message_id": message_id,
                            "reaction": [{ "type": "emoji", "emoji": emoji }]
                        }),
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                Ok(ToolResult::json(result))
            }
            "sendSticker" => {
                let sticker_id = get_str(&params, "stickerId")?;

                let result = api
                    .call(
                        "sendSticker",
                        &serde_json::json!({
                            "chat_id": chat_id,
                            "sticker": sticker_id
                        }),
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                Ok(ToolResult::json(result))
            }
            "sendPhoto" | "sendDocument" | "sendVideo" => {
                let file_url = params
                    .get("fileUrl")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing fileUrl for {action}"))?;
                let media_field = match action {
                    "sendPhoto" => "photo",
                    "sendDocument" => "document",
                    _ => "video",
                };
                let caption = params.get("caption").and_then(|v| v.as_str());

                // Captions over the 1024-char limit are sent as a chunked
                // follow-up text message after the media.
                let result = api
                    .send_media_with_follow_up(chat_id, action, media_field, file_url, caption, None)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                Ok(ToolResult::json(result))
            }
            "createForumTopic" => {
                let topic_name = get_str(&params, "topicName")?;

                let result = api
                    .call(
                        "createForumTopic",
                        &serde_json::json!({
                            "chat_id": chat_id,
                            "name": topic_name
                        }),
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
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
