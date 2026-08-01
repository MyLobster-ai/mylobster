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
                            "setPresence", "pinMessage", "poll",
                            "uploadFile"
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
                    "filePath": { "type": "string", "description": "Local file path (agent workspace scoped) or http(s) URL for uploadFile" },
                    "path": { "type": "string", "description": "Alias for filePath" },
                    "media": { "type": "string", "description": "Alias for filePath" },
                    "filename": { "type": "string", "description": "Override upload filename" },
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

        // v2026.7.1: cross-provider guild admin action block — guild-admin
        // actions need a Discord sender identity for permission checks, so
        // requests originating from another provider's conversation are
        // rejected.
        if crate::channels::discord_routing::is_trusted_requester_guild_admin_action(action)
            && !crate::channels::discord_routing::is_discord_session_key(&context.session_key)
        {
            return Ok(ToolResult::error(format!(
                "Action '{}' is a Discord guild-admin action and requires a trusted \
                 Discord requester; cross-provider requests are blocked",
                action
            )));
        }

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
            "uploadFile" => {
                // v2026.5.2: upload-file message action with agent-scoped media
                // reads. Local paths must resolve inside the agent workspace;
                // http(s) URLs are fetched directly.
                let channel_id = get_str(&params, "channelId")?;
                let media = params
                    .get("filePath")
                    .or_else(|| params.get("path"))
                    .or_else(|| params.get("media"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .ok_or_else(|| {
                        anyhow::anyhow!("uploadFile requires filePath, path, or media")
                    })?;
                let content = params
                    .get("content")
                    .or_else(|| params.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                let (bytes, default_name) = if media.starts_with("http://")
                    || media.starts_with("https://")
                {
                    let resp = client.get(&media).send().await?;
                    if !resp.status().is_success() {
                        anyhow::bail!(
                            "uploadFile: failed to fetch media URL ({})",
                            resp.status()
                        );
                    }
                    let name = media
                        .rsplit('/')
                        .next()
                        .filter(|n| !n.is_empty())
                        .unwrap_or("upload.bin")
                        .split('?')
                        .next()
                        .unwrap_or("upload.bin")
                        .to_string();
                    (resp.bytes().await?.to_vec(), name)
                } else {
                    let workspace = context
                        .config
                        .agents
                        .defaults
                        .as_ref()
                        .and_then(|d| d.workspace.as_deref());
                    let resolved = resolve_agent_scoped_media_path(workspace, &media)
                        .map_err(|e| anyhow::anyhow!("uploadFile: {}", e))?;
                    let name = resolved
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("upload.bin")
                        .to_string();
                    (tokio::fs::read(&resolved).await?, name)
                };

                let filename = params
                    .get("filename")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or(default_name);

                let payload = serde_json::json!({
                    "content": content,
                    "attachments": [{ "id": 0, "filename": filename }]
                });
                let form = reqwest::multipart::Form::new()
                    .text("payload_json", payload.to_string())
                    .part(
                        "files[0]",
                        reqwest::multipart::Part::bytes(bytes).file_name(filename.clone()),
                    );

                let resp = client
                    .post(format!("{}/channels/{}/messages", base_url, channel_id))
                    .header("Authorization", format!("Bot {}", bot_token))
                    .multipart(form)
                    .send()
                    .await?;

                let status = resp.status();
                let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
                if !status.is_success() {
                    anyhow::bail!("uploadFile failed ({}): {}", status, body);
                }
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

/// Resolve a local media path for `uploadFile`, enforcing agent-scoped reads
/// (v2026.5.2): the path must canonicalize to a location inside the configured
/// agent workspace, with symlink-alias escapes rejected.
pub fn resolve_agent_scoped_media_path(
    workspace: Option<&str>,
    path: &str,
) -> Result<std::path::PathBuf, String> {
    let workspace = workspace
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .ok_or_else(|| {
            "agent workspace not configured; local media reads are agent-scoped".to_string()
        })?;
    let workspace = if let Some(rest) = workspace.strip_prefix("~/") {
        dirs::home_dir()
            .ok_or_else(|| "cannot resolve home directory".to_string())?
            .join(rest)
    } else {
        std::path::PathBuf::from(workspace)
    };
    let candidate = std::path::Path::new(path);
    let candidate = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        workspace.join(candidate)
    };
    let canonical = candidate
        .canonicalize()
        .map_err(|e| format!("cannot read '{}': {}", candidate.display(), e))?;
    let ws_canonical = workspace
        .canonicalize()
        .map_err(|e| format!("cannot resolve workspace '{}': {}", workspace.display(), e))?;
    if !canonical.starts_with(&ws_canonical) {
        return Err(format!(
            "path '{}' resolves outside the agent workspace '{}'",
            path,
            ws_canonical.display()
        ));
    }
    // Reject symlink-alias escapes (hardlink/symlink identity mismatch).
    crate::infra::hardlink_guards::assert_no_path_alias_escape(
        &candidate,
        &ws_canonical,
        crate::infra::hardlink_guards::PathAliasPolicy::RejectAliases,
    )?;
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mylobster-discord-test-{}", name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn agent_scoped_media_requires_workspace() {
        let err = resolve_agent_scoped_media_path(None, "/etc/passwd").unwrap_err();
        assert!(err.contains("agent workspace not configured"));
    }

    #[test]
    fn agent_scoped_media_allows_workspace_files() {
        let ws = temp_workspace("allow");
        let file = ws.join("upload.txt");
        std::fs::write(&file, b"hello").unwrap();
        let resolved =
            resolve_agent_scoped_media_path(ws.to_str(), file.to_str().unwrap()).unwrap();
        assert!(resolved.ends_with("upload.txt"));
        // Relative paths resolve against the workspace.
        let rel = resolve_agent_scoped_media_path(ws.to_str(), "upload.txt").unwrap();
        assert_eq!(rel, resolved);
    }

    #[test]
    fn agent_scoped_media_rejects_escapes() {
        let ws = temp_workspace("escape");
        let err = resolve_agent_scoped_media_path(ws.to_str(), "/etc/hosts").unwrap_err();
        assert!(err.contains("outside the agent workspace"));
        let err = resolve_agent_scoped_media_path(ws.to_str(), "../outside.txt").unwrap_err();
        // Either it doesn't exist or resolves outside — both are rejections.
        assert!(err.contains("outside") || err.contains("cannot read"));
    }
}

