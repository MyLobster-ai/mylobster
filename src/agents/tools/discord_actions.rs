//! Discord channel actions tool.
//!
//! Supports multiple Discord actions beyond simple message sending:
//! react, sendMessage, editMessage, deleteMessage, threadCreate, threadReply,
//! searchMessages, memberInfo, roleInfo, channelList, channelCreate,
//! roleAdd, roleRemove, kick, ban, timeout, setPresence, pinMessage, poll.

use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use async_trait::async_trait;

pub struct DiscordActionsTool;

#[async_trait]
impl AgentTool for DiscordActionsTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "discord".to_string(),
            description: "Perform Discord actions: send/edit/delete messages, react, manage threads, members, roles, channels, pins, polls".to_string(),
            category: "discord".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": [
                            "sendMessage", "editMessage", "deleteMessage",
                            "react", "threadCreate", "threadReply",
                            "searchMessages", "memberInfo", "roleInfo",
                            "channelList", "channelCreate",
                            "roleAdd", "roleRemove",
                            "kick", "ban", "timeout",
                            "setPresence", "pinMessage", "poll"
                        ],
                        "description": "The Discord action to perform"
                    },
                    "channelId": { "type": "string", "description": "Discord channel ID" },
                    "messageId": { "type": "string", "description": "Discord message ID" },
                    "guildId": { "type": "string", "description": "Discord guild/server ID" },
                    "userId": { "type": "string", "description": "Discord user ID" },
                    "roleId": { "type": "string", "description": "Discord role ID" },
                    "content": { "type": "string", "description": "Message content" },
                    "emoji": { "type": "string", "description": "Emoji for reactions" },
                    "threadName": { "type": "string", "description": "Thread name" },
                    "reason": { "type": "string", "description": "Reason for moderation actions" },
                    "duration": { "type": "integer", "description": "Duration in seconds for timeout" },
                    "query": { "type": "string", "description": "Search query" },
                    "limit": { "type": "integer", "description": "Max results", "default": 25 },
                    "presenceStatus": { "type": "string", "enum": ["online", "idle", "dnd", "invisible"] },
                    "pollQuestion": { "type": "string" },
                    "pollOptions": { "type": "array", "items": { "type": "string" } },
                    "pollDuration": { "type": "integer", "description": "Poll duration in hours" }
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
            .discord
            .default_account
            .token
            .clone()
            .or_else(|| std::env::var("DISCORD_BOT_TOKEN").ok())
            .ok_or_else(|| anyhow::anyhow!("No Discord bot token configured"))?;

        let client = reqwest::Client::new();
        let base_url = "https://discord.com/api/v10";

        match action {
            "sendMessage" => {
                let channel_id = get_str(&params, "channelId")?;
                let content = get_str(&params, "content")?;

                let resp = client
                    .post(format!("{}/channels/{}/messages", base_url, channel_id))
                    .header("Authorization", format!("Bot {}", bot_token))
                    .json(&serde_json::json!({ "content": content }))
                    .send()
                    .await?;

                let body: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(body))
            }
            "editMessage" => {
                let channel_id = get_str(&params, "channelId")?;
                let message_id = get_str(&params, "messageId")?;
                let content = get_str(&params, "content")?;

                let resp = client
                    .patch(format!(
                        "{}/channels/{}/messages/{}",
                        base_url, channel_id, message_id
                    ))
                    .header("Authorization", format!("Bot {}", bot_token))
                    .json(&serde_json::json!({ "content": content }))
                    .send()
                    .await?;

                let body: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(body))
            }
            "deleteMessage" => {
                let channel_id = get_str(&params, "channelId")?;
                let message_id = get_str(&params, "messageId")?;

                client
                    .delete(format!(
                        "{}/channels/{}/messages/{}",
                        base_url, channel_id, message_id
                    ))
                    .header("Authorization", format!("Bot {}", bot_token))
                    .send()
                    .await?;

                Ok(ToolResult::text("Message deleted"))
            }
            "react" => {
                let channel_id = get_str(&params, "channelId")?;
                let message_id = get_str(&params, "messageId")?;
                let emoji = get_str(&params, "emoji")?;
                let encoded = url::form_urlencoded::byte_serialize(emoji.as_bytes())
                    .collect::<String>();

                client
                    .put(format!(
                        "{}/channels/{}/messages/{}/reactions/{}/@me",
                        base_url, channel_id, message_id, encoded
                    ))
                    .header("Authorization", format!("Bot {}", bot_token))
                    .send()
                    .await?;

                Ok(ToolResult::text(format!("Reacted with {}", emoji)))
            }
            "threadCreate" => {
                let channel_id = get_str(&params, "channelId")?;
                let thread_name = get_str(&params, "threadName")?;
                let content = params
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                // Start thread from message or create standalone
                let resp = client
                    .post(format!(
                        "{}/channels/{}/threads",
                        base_url, channel_id
                    ))
                    .header("Authorization", format!("Bot {}", bot_token))
                    .json(&serde_json::json!({
                        "name": thread_name,
                        "type": 11, // PUBLIC_THREAD
                        "auto_archive_duration": 1440
                    }))
                    .send()
                    .await?;

                let thread: serde_json::Value = resp.json().await?;

                // Send initial message if provided
                if !content.is_empty() {
                    if let Some(thread_id) = thread.get("id").and_then(|v| v.as_str()) {
                        client
                            .post(format!("{}/channels/{}/messages", base_url, thread_id))
                            .header("Authorization", format!("Bot {}", bot_token))
                            .json(&serde_json::json!({ "content": content }))
                            .send()
                            .await?;
                    }
                }

                Ok(ToolResult::json(thread))
            }
            "threadReply" => {
                let channel_id = get_str(&params, "channelId")?;
                let content = get_str(&params, "content")?;

                let resp = client
                    .post(format!("{}/channels/{}/messages", base_url, channel_id))
                    .header("Authorization", format!("Bot {}", bot_token))
                    .json(&serde_json::json!({ "content": content }))
                    .send()
                    .await?;

                let body: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(body))
            }
            "memberInfo" => {
                let guild_id = get_str(&params, "guildId")?;
                let user_id = get_str(&params, "userId")?;

                let resp = client
                    .get(format!(
                        "{}/guilds/{}/members/{}",
                        base_url, guild_id, user_id
                    ))
                    .header("Authorization", format!("Bot {}", bot_token))
                    .send()
                    .await?;

                let body: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(body))
            }
            "channelList" => {
                let guild_id = get_str(&params, "guildId")?;

                let resp = client
                    .get(format!("{}/guilds/{}/channels", base_url, guild_id))
                    .header("Authorization", format!("Bot {}", bot_token))
                    .send()
                    .await?;

                let body: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(body))
            }
            "roleAdd" => {
                let guild_id = get_str(&params, "guildId")?;
                let user_id = get_str(&params, "userId")?;
                let role_id = get_str(&params, "roleId")?;

                client
                    .put(format!(
                        "{}/guilds/{}/members/{}/roles/{}",
                        base_url, guild_id, user_id, role_id
                    ))
                    .header("Authorization", format!("Bot {}", bot_token))
                    .send()
                    .await?;

                Ok(ToolResult::text("Role added"))
            }
            "roleRemove" => {
                let guild_id = get_str(&params, "guildId")?;
                let user_id = get_str(&params, "userId")?;
                let role_id = get_str(&params, "roleId")?;

                client
                    .delete(format!(
                        "{}/guilds/{}/members/{}/roles/{}",
                        base_url, guild_id, user_id, role_id
                    ))
                    .header("Authorization", format!("Bot {}", bot_token))
                    .send()
                    .await?;

                Ok(ToolResult::text("Role removed"))
            }
            "kick" => {
                let guild_id = get_str(&params, "guildId")?;
                let user_id = get_str(&params, "userId")?;

                client
                    .delete(format!(
                        "{}/guilds/{}/members/{}",
                        base_url, guild_id, user_id
                    ))
                    .header("Authorization", format!("Bot {}", bot_token))
                    .send()
                    .await?;

                Ok(ToolResult::text("Member kicked"))
            }
            "ban" => {
                let guild_id = get_str(&params, "guildId")?;
                let user_id = get_str(&params, "userId")?;
                let reason = params
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("No reason provided");

                client
                    .put(format!(
                        "{}/guilds/{}/bans/{}",
                        base_url, guild_id, user_id
                    ))
                    .header("Authorization", format!("Bot {}", bot_token))
                    .json(&serde_json::json!({ "reason": reason }))
                    .send()
                    .await?;

                Ok(ToolResult::text("Member banned"))
            }
            "timeout" => {
                let guild_id = get_str(&params, "guildId")?;
                let user_id = get_str(&params, "userId")?;
                let duration = params
                    .get("duration")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(300);

                let until = chrono::Utc::now()
                    + chrono::Duration::seconds(duration as i64);

                client
                    .patch(format!(
                        "{}/guilds/{}/members/{}",
                        base_url, guild_id, user_id
                    ))
                    .header("Authorization", format!("Bot {}", bot_token))
                    .json(&serde_json::json!({
                        "communication_disabled_until": until.to_rfc3339()
                    }))
                    .send()
                    .await?;

                Ok(ToolResult::text(format!(
                    "Member timed out for {} seconds",
                    duration
                )))
            }
            "pinMessage" => {
                let channel_id = get_str(&params, "channelId")?;
                let message_id = get_str(&params, "messageId")?;

                client
                    .put(format!(
                        "{}/channels/{}/pins/{}",
                        base_url, channel_id, message_id
                    ))
                    .header("Authorization", format!("Bot {}", bot_token))
                    .send()
                    .await?;

                Ok(ToolResult::text("Message pinned"))
            }
            "searchMessages" => {
                let guild_id = get_str(&params, "guildId")?;
                let query = get_str(&params, "query")?;
                let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(25);

                let resp = client
                    .get(format!("{}/guilds/{}/messages/search", base_url, guild_id))
                    .header("Authorization", format!("Bot {}", bot_token))
                    .query(&[
                        ("content", query.as_str()),
                        ("limit", &limit.to_string()),
                    ])
                    .send()
                    .await?;

                let body: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(body))
            }
            "roleInfo" => {
                let guild_id = get_str(&params, "guildId")?;

                let resp = client
                    .get(format!("{}/guilds/{}/roles", base_url, guild_id))
                    .header("Authorization", format!("Bot {}", bot_token))
                    .send()
                    .await?;

                let body: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(body))
            }
            "channelCreate" => {
                let guild_id = get_str(&params, "guildId")?;
                let name = get_str(&params, "threadName")?; // reuse threadName for channel name

                let resp = client
                    .post(format!("{}/guilds/{}/channels", base_url, guild_id))
                    .header("Authorization", format!("Bot {}", bot_token))
                    .json(&serde_json::json!({
                        "name": name,
                        "type": 0 // GUILD_TEXT
                    }))
                    .send()
                    .await?;

                let body: serde_json::Value = resp.json().await?;
                Ok(ToolResult::json(body))
            }
            "setPresence" | "poll" => {
                Ok(ToolResult::text(format!(
                    "Action '{}' requires gateway connection (not available via REST)",
                    action
                )))
            }
            _ => Ok(ToolResult::error(format!("Unknown Discord action: {}", action))),
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

