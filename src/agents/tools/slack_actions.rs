//! Slack channel actions tool.
//!
//! Supports: sendMessage, editMessage, deleteMessage, react,
//! readMessages, downloadFile, pinMessage, unpinMessage, listPins,
//! getMemberInfo, listEmojis.
//!
//! v2026.7.1 parity (upstream `extensions/slack/src/actions.ts`, `send.ts`):
//! - targets accept `channel:C…`, `#C…`, and bare ids (`target-parsing.ts`)
//! - outbound unfurl defaults off; `replyBroadcast` honored with `threadTs`
//! - Block Kit blocks validated (max 50, object shape)
//! - mutating calls run under the process-wide Slack write lock
//! - thread reads paginate unbounded via the shared cursor-loop helper

use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use crate::channels::slack::{
    build_slack_post_message_payload, collect_slack_cursor_items, read_slack_next_cursor,
    resolve_slack_bot_token, resolve_slack_channel_id, resolve_slack_thread_ts_value,
    strip_slack_reasoning_from_outbound, validate_slack_blocks_array, with_slack_write_lock,
    SlackUnfurlOptions,
};
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
                    "channel": { "type": "string", "description": "Slack channel (id, channel:<id>, or #<id>)" },
                    "text": { "type": "string" },
                    "ts": { "type": "string", "description": "Message timestamp" },
                    "emoji": { "type": "string", "description": "Emoji name (without colons)" },
                    "userId": { "type": "string" },
                    "fileUrl": { "type": "string" },
                    "threadTs": { "type": "string", "description": "Thread timestamp for replies" },
                    "replyBroadcast": { "type": "boolean", "description": "Broadcast a thread reply to the channel" },
                    "limit": { "type": "integer", "default": 20 },
                    "blocks": { "type": "array", "description": "Block Kit blocks (max 50)" }
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

        // v2026.7.1: SecretRef-shaped tokens tolerated (env shorthands etc.).
        let account = &context.config.channels.slack.default_account;
        let bot_token = resolve_slack_bot_token(account)
            .ok_or_else(|| anyhow::anyhow!("No Slack bot token configured"))?;

        let client = reqwest::Client::new();
        let base_url = "https://slack.com/api";

        match action {
            "sendMessage" => {
                let channel = get_channel(&params)?;
                let text = get_str(&params, "text")?;

                // Never leak reasoning payloads to the channel (v2026.7.1).
                let text = strip_slack_reasoning_from_outbound(&text);

                let thread_ts = resolve_slack_thread_ts_value(
                    None,
                    params.get("threadTs").and_then(|v| v.as_str()),
                );
                let reply_broadcast = params
                    .get("replyBroadcast")
                    .and_then(|v| v.as_bool())
                    .or(account.reply_broadcast)
                    .unwrap_or(false);
                let blocks = match params.get("blocks") {
                    Some(raw) => Some(validate_slack_blocks_array(raw)?),
                    None => None,
                };

                let body = build_slack_post_message_payload(
                    &channel,
                    &text,
                    thread_ts.as_deref(),
                    reply_broadcast,
                    blocks.as_deref(),
                    SlackUnfurlOptions::from_account(account),
                );

                let result = with_slack_write_lock(async {
                    let resp = client
                        .post(format!("{}/chat.postMessage", base_url))
                        .header("Authorization", format!("Bearer {}", bot_token))
                        .json(&body)
                        .send()
                        .await?;
                    resp.json::<serde_json::Value>().await.map_err(anyhow::Error::from)
                })
                .await?;
                Ok(ToolResult::json(result))
            }
            "editMessage" => {
                let channel = get_channel(&params)?;
                let ts = get_str(&params, "ts")?;
                let text = get_str(&params, "text")?;
                let text = strip_slack_reasoning_from_outbound(&text);

                let result = with_slack_write_lock(async {
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
                    resp.json::<serde_json::Value>().await.map_err(anyhow::Error::from)
                })
                .await?;
                Ok(ToolResult::json(result))
            }
            "deleteMessage" => {
                let channel = get_channel(&params)?;
                let ts = get_str(&params, "ts")?;

                let result = with_slack_write_lock(async {
                    let resp = client
                        .post(format!("{}/chat.delete", base_url))
                        .header("Authorization", format!("Bearer {}", bot_token))
                        .json(&serde_json::json!({
                            "channel": channel,
                            "ts": ts
                        }))
                        .send()
                        .await?;
                    resp.json::<serde_json::Value>().await.map_err(anyhow::Error::from)
                })
                .await?;
                Ok(ToolResult::json(result))
            }
            "react" => {
                let channel = get_channel(&params)?;
                let ts = get_str(&params, "ts")?;
                let emoji = get_str(&params, "emoji")?;

                let result = with_slack_write_lock(async {
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
                    resp.json::<serde_json::Value>().await.map_err(anyhow::Error::from)
                })
                .await?;
                Ok(ToolResult::json(result))
            }
            "readMessages" => {
                let channel = get_channel(&params)?;
                let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(20);
                let thread_ts = resolve_slack_thread_ts_value(
                    None,
                    params.get("threadTs").and_then(|v| v.as_str()),
                );

                if let Some(thread_ts) = thread_ts {
                    // v2026.7.1: unbounded thread pagination — follow the
                    // cursor chain until Slack ends it, no page cap.
                    let client_ref = &client;
                    let bot_token_ref = &bot_token;
                    let channel_ref = &channel;
                    let thread_ts_ref = &thread_ts;
                    let messages = collect_slack_cursor_items(move |cursor| async move {
                        let mut query: Vec<(String, String)> = vec![
                            ("channel".to_string(), channel_ref.clone()),
                            ("ts".to_string(), thread_ts_ref.clone()),
                            ("limit".to_string(), "200".to_string()),
                        ];
                        if let Some(cursor) = cursor {
                            query.push(("cursor".to_string(), cursor));
                        }
                        let resp = client_ref
                            .get(format!("{}/conversations.replies", base_url))
                            .header("Authorization", format!("Bearer {}", bot_token_ref))
                            .query(&query)
                            .send()
                            .await?;
                        let result: serde_json::Value = resp.json().await?;
                        let next_cursor = read_slack_next_cursor(&result);
                        let items = result
                            .get("messages")
                            .and_then(|m| m.as_array())
                            .cloned()
                            .unwrap_or_default();
                        Ok((items, next_cursor))
                    })
                    .await?;
                    return Ok(ToolResult::json(serde_json::json!({
                        "ok": true,
                        "messages": messages
                    })));
                }

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
                let channel = get_channel(&params)?;
                let ts = get_str(&params, "ts")?;

                let result = with_slack_write_lock(async {
                    let resp = client
                        .post(format!("{}/pins.add", base_url))
                        .header("Authorization", format!("Bearer {}", bot_token))
                        .json(&serde_json::json!({
                            "channel": channel,
                            "timestamp": ts
                        }))
                        .send()
                        .await?;
                    resp.json::<serde_json::Value>().await.map_err(anyhow::Error::from)
                })
                .await?;
                Ok(ToolResult::json(result))
            }
            "unpinMessage" => {
                let channel = get_channel(&params)?;
                let ts = get_str(&params, "ts")?;

                let result = with_slack_write_lock(async {
                    let resp = client
                        .post(format!("{}/pins.remove", base_url))
                        .header("Authorization", format!("Bearer {}", bot_token))
                        .json(&serde_json::json!({
                            "channel": channel,
                            "timestamp": ts
                        }))
                        .send()
                        .await?;
                    resp.json::<serde_json::Value>().await.map_err(anyhow::Error::from)
                })
                .await?;
                Ok(ToolResult::json(result))
            }
            "listPins" => {
                let channel = get_channel(&params)?;

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

/// Resolve the `channel` param through Slack target parsing so `channel:C…`,
/// `#C…`, and bare ids are all accepted (v2026.5.2 target syntax).
fn get_channel(params: &serde_json::Value) -> Result<String> {
    let raw = get_str(params, "channel")?;
    resolve_slack_channel_id(&raw)
}
