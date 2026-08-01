use crate::config::{
    Config, DiscordAccountConfig, DiscordGuildChannelConfig, DiscordGuildEntry, DiscordVoiceConfig,
    GroupPolicy, OutboundRetryConfig,
};
use crate::gateway::GatewayState;

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};

use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tracing::{info, warn};

// ============================================================================
// v2026.4.1: Media Download Idle-Timeout
// ============================================================================

/// Maximum time to wait for a media download before aborting (v2026.4.1).
const MEDIA_DOWNLOAD_TIMEOUT_SECS: u64 = 30;

// ============================================================================
// v2026.2.26: Slash Command Validation
// ============================================================================

/// A Discord slash command definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommand {
    /// Command name (1-32 chars, lowercase, no spaces).
    pub name: String,
    /// Command description (1-100 chars).
    pub description: String,
    /// Command options.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<SlashCommandOption>,
}

/// A slash command option.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommandOption {
    /// Option name (1-32 chars, lowercase, no spaces).
    pub name: String,
    /// Option description (1-100 chars).
    pub description: String,
    /// Option type (3=string, 4=integer, 5=boolean, etc.).
    #[serde(rename = "type")]
    pub option_type: u8,
    /// Whether this option is required.
    #[serde(default)]
    pub required: bool,
}

/// Validation error for slash command definitions.
#[derive(Debug, Clone)]
pub struct SlashCommandValidationError {
    pub field: String,
    pub message: String,
}

/// Validate a slash command definition before registration.
///
/// v2026.2.26: Validates all fields to prevent Discord API errors during
/// registration. Invalid commands are logged and skipped rather than
/// causing the entire registration to fail.
pub fn validate_slash_command(cmd: &SlashCommand) -> Vec<SlashCommandValidationError> {
    let mut errors = Vec::new();

    // Name: 1-32 chars, lowercase, no spaces, matches ^[\w-]{1,32}$
    if cmd.name.is_empty() || cmd.name.len() > 32 {
        errors.push(SlashCommandValidationError {
            field: "name".to_string(),
            message: format!(
                "Command name must be 1-32 characters, got {}",
                cmd.name.len()
            ),
        });
    }

    if cmd.name != cmd.name.to_lowercase() {
        errors.push(SlashCommandValidationError {
            field: "name".to_string(),
            message: "Command name must be lowercase".to_string(),
        });
    }

    if cmd.name.contains(' ') {
        errors.push(SlashCommandValidationError {
            field: "name".to_string(),
            message: "Command name must not contain spaces".to_string(),
        });
    }

    if !cmd
        .name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        errors.push(SlashCommandValidationError {
            field: "name".to_string(),
            message: "Command name must only contain alphanumeric characters, hyphens, or underscores".to_string(),
        });
    }

    // Description: 1-100 chars
    if cmd.description.is_empty() || cmd.description.len() > 100 {
        errors.push(SlashCommandValidationError {
            field: "description".to_string(),
            message: format!(
                "Command description must be 1-100 characters, got {}",
                cmd.description.len()
            ),
        });
    }

    // Validate options
    for (i, opt) in cmd.options.iter().enumerate() {
        if opt.name.is_empty() || opt.name.len() > 32 {
            errors.push(SlashCommandValidationError {
                field: format!("options[{}].name", i),
                message: format!(
                    "Option name must be 1-32 characters, got {}",
                    opt.name.len()
                ),
            });
        }

        if opt.name != opt.name.to_lowercase() {
            errors.push(SlashCommandValidationError {
                field: format!("options[{}].name", i),
                message: "Option name must be lowercase".to_string(),
            });
        }

        if opt.description.is_empty() || opt.description.len() > 100 {
            errors.push(SlashCommandValidationError {
                field: format!("options[{}].description", i),
                message: format!(
                    "Option description must be 1-100 characters, got {}",
                    opt.description.len()
                ),
            });
        }

        // Valid option types: 1-11
        if opt.option_type == 0 || opt.option_type > 11 {
            errors.push(SlashCommandValidationError {
                field: format!("options[{}].type", i),
                message: format!(
                    "Option type must be 1-11, got {}",
                    opt.option_type
                ),
            });
        }
    }

    errors
}

/// Filter and validate a list of slash commands, returning only valid ones.
///
/// Invalid commands are logged as warnings but do not prevent valid commands
/// from being registered.
pub fn filter_valid_commands(commands: Vec<SlashCommand>) -> Vec<SlashCommand> {
    commands
        .into_iter()
        .filter(|cmd| {
            let errors = validate_slash_command(cmd);
            if errors.is_empty() {
                true
            } else {
                for error in &errors {
                    warn!(
                        "Discord slash command '{}' validation error in {}: {}",
                        cmd.name, error.field, error.message
                    );
                }
                false
            }
        })
        .collect()
}

// ============================================================================
// Discord Channel Implementation
// ============================================================================

/// Discord channel implementation using serenity.
pub struct DiscordChannel {
    enabled: bool,
    bot_token: Option<String>,
    /// Configured outbound `@handle` → user-id rewrites (v2026.5.2).
    mention_aliases: Option<HashMap<String, String>>,
    /// Outbound retry policy (shared with the learned-cooldown REST client).
    retry: Option<OutboundRetryConfig>,
}

impl DiscordChannel {
    pub fn new(config: &Config) -> Self {
        let dc = &config.channels.discord;
        let bot_token = dc.default_account.token.clone();
        let enabled = dc.default_account.enabled.unwrap_or(bot_token.is_some());

        Self {
            enabled,
            bot_token,
            mention_aliases: dc.default_account.mention_aliases.clone(),
            retry: dc.default_account.retry.clone(),
        }
    }
}

#[async_trait]
impl ChannelPlugin for DiscordChannel {
    fn id(&self) -> &str {
        "discord"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Discord".to_string(),
            description: "Discord Bot channel via serenity gateway".to_string(),
            enabled: self.enabled,
            multi_account: true,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![
            ChannelCapability::SendText,
            ChannelCapability::ReceiveText,
            ChannelCapability::SendMedia,
            ChannelCapability::ReceiveMedia,
            ChannelCapability::Reactions,
            ChannelCapability::Groups,
            ChannelCapability::Threads,
            ChannelCapability::EditMessage,
            ChannelCapability::DeleteMessage,
            ChannelCapability::Stickers,
            ChannelCapability::Voice,
            ChannelCapability::Polls,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let token = match &self.bot_token {
            Some(t) => t,
            None => {
                warn!("Discord channel enabled but no bot token configured");
                return Ok(());
            }
        };

        info!(
            "Discord channel starting (token ends ...{})",
            &token[token.len().saturating_sub(4)..]
        );

        // TODO: Initialise a serenity::Client with a gateway handler.
        // v2026.4.1: Pass attachment and sticker downloads through shared idle-timeout
        // and worker-abort path to prevent hangs (MEDIA_DOWNLOAD_TIMEOUT_SECS).

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.enabled {
            info!("Discord channel stopping");
            // TODO: Shut down the serenity client.
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        let token = self
            .bot_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Discord bot token not configured"))?;

        let _channel_id: u64 = to
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid Discord channel_id: {to}"))?;

        // v2026.5.2: rewrite configured @Name aliases to real user mentions.
        let content = rewrite_discord_known_mentions(message, self.mention_aliases.as_ref());

        info!(channel_id = to, "Discord: sending message");

        // v2026.5.2: send through the REST client with learned bucket/global
        // cooldowns and queued 429 retries.
        let client = DiscordRestClient::new(token, self.retry.clone());
        client.send_channel_message(to, &content).await?;

        Ok(())
    }
}

/// Convenience function called by the top-level `send_message` dispatcher.
pub(crate) async fn send_message(config: &Config, to: &str, message: &str) -> Result<()> {
    let channel = DiscordChannel::new(config);
    channel.send_message(to, message).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_slash_command() {
        let cmd = SlashCommand {
            name: "help".to_string(),
            description: "Show help".to_string(),
            options: vec![],
        };
        let errors = validate_slash_command(&cmd);
        assert!(errors.is_empty());
    }

    #[test]
    fn slash_command_name_too_long() {
        let cmd = SlashCommand {
            name: "a".repeat(33),
            description: "Test".to_string(),
            options: vec![],
        };
        let errors = validate_slash_command(&cmd);
        assert!(!errors.is_empty());
        assert!(errors.iter().any(|e| e.field == "name"));
    }

    #[test]
    fn slash_command_name_uppercase_rejected() {
        let cmd = SlashCommand {
            name: "Help".to_string(),
            description: "Show help".to_string(),
            options: vec![],
        };
        let errors = validate_slash_command(&cmd);
        assert!(errors.iter().any(|e| e.message.contains("lowercase")));
    }

    #[test]
    fn slash_command_name_with_spaces_rejected() {
        let cmd = SlashCommand {
            name: "my command".to_string(),
            description: "Test".to_string(),
            options: vec![],
        };
        let errors = validate_slash_command(&cmd);
        assert!(errors.iter().any(|e| e.message.contains("spaces")));
    }

    #[test]
    fn slash_command_empty_description_rejected() {
        let cmd = SlashCommand {
            name: "test".to_string(),
            description: "".to_string(),
            options: vec![],
        };
        let errors = validate_slash_command(&cmd);
        assert!(errors.iter().any(|e| e.field == "description"));
    }

    #[test]
    fn slash_command_description_too_long_rejected() {
        let cmd = SlashCommand {
            name: "test".to_string(),
            description: "x".repeat(101),
            options: vec![],
        };
        let errors = validate_slash_command(&cmd);
        assert!(errors.iter().any(|e| e.field == "description"));
    }

    #[test]
    fn slash_command_option_validation() {
        let cmd = SlashCommand {
            name: "test".to_string(),
            description: "Test command".to_string(),
            options: vec![SlashCommandOption {
                name: "INVALID".to_string(),
                description: "An option".to_string(),
                option_type: 3,
                required: false,
            }],
        };
        let errors = validate_slash_command(&cmd);
        assert!(errors
            .iter()
            .any(|e| e.field.starts_with("options[0]")));
    }

    #[test]
    fn slash_command_option_invalid_type() {
        let cmd = SlashCommand {
            name: "test".to_string(),
            description: "Test command".to_string(),
            options: vec![SlashCommandOption {
                name: "opt".to_string(),
                description: "An option".to_string(),
                option_type: 99,
                required: false,
            }],
        };
        let errors = validate_slash_command(&cmd);
        assert!(errors.iter().any(|e| e.message.contains("1-11")));
    }

    #[test]
    fn filter_valid_commands_keeps_valid() {
        let commands = vec![
            SlashCommand {
                name: "help".to_string(),
                description: "Show help".to_string(),
                options: vec![],
            },
            SlashCommand {
                name: "INVALID".to_string(),
                description: "Bad command".to_string(),
                options: vec![],
            },
            SlashCommand {
                name: "ask".to_string(),
                description: "Ask a question".to_string(),
                options: vec![SlashCommandOption {
                    name: "query".to_string(),
                    description: "Your question".to_string(),
                    option_type: 3,
                    required: true,
                }],
            },
        ];
        let valid = filter_valid_commands(commands);
        assert_eq!(valid.len(), 2);
        assert_eq!(valid[0].name, "help");
        assert_eq!(valid[1].name, "ask");
    }

    #[test]
    fn valid_command_with_hyphen_and_underscore() {
        let cmd = SlashCommand {
            name: "my-cool_cmd".to_string(),
            description: "A cool command".to_string(),
            options: vec![],
        };
        let errors = validate_slash_command(&cmd);
        assert!(errors.is_empty());
    }
}

// ============================================================================
// v2026.5.2: Channel-audience DM authorization (`accessGroup:<name>`)
// ============================================================================
//
// Ported from OpenClaw `src/channels/allow-from.ts` and
// `src/channels/message-access/runtime-access-groups.ts`.

/// Prefix that marks an allowFrom entry as an access-group reference instead
/// of a direct sender id.
pub const ACCESS_GROUP_ALLOW_FROM_PREFIX: &str = "accessGroup:";

/// Parse an access-group allowFrom entry, returning the referenced group name.
pub fn parse_access_group_allow_from_entry(entry: &str) -> Option<&str> {
    let trimmed = entry.trim();
    let name = trimmed.strip_prefix(ACCESS_GROUP_ALLOW_FROM_PREFIX)?.trim();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn discord_allow_entry_matches(
    entry: &str,
    sender_id: &str,
    sender_name: Option<&str>,
    allow_name_matching: bool,
) -> bool {
    let entry = entry.trim();
    if entry.is_empty() {
        return false;
    }
    if entry == "*" {
        return true;
    }
    let bare = entry.strip_prefix('@').unwrap_or(entry);
    if bare.chars().all(|c| c.is_ascii_digit()) {
        return bare == sender_id;
    }
    // Mutable identity matching (names/tags) is opt-in, mirroring
    // `dangerouslyAllowNameMatching` (default: ID-only matching).
    if !allow_name_matching {
        return false;
    }
    match sender_name {
        Some(name) => bare.eq_ignore_ascii_case(name.trim().strip_prefix('@').unwrap_or(name.trim())),
        None => false,
    }
}

/// Check whether a DM sender is authorized by an allowFrom list that may
/// contain symbolic `accessGroup:<name>` entries, expanding groups through the
/// provided resolver. Unresolvable groups yield not-matched (never an implicit
/// allow), mirroring OpenClaw's runtime access-group membership facts.
pub fn is_dm_sender_authorized_with<F>(
    sender_id: &str,
    sender_name: Option<&str>,
    allow_from: &[String],
    allow_name_matching: bool,
    mut resolve: F,
) -> bool
where
    F: FnMut(&str) -> Option<Vec<String>>,
{
    for entry in allow_from {
        if let Some(group) = parse_access_group_allow_from_entry(entry) {
            if let Some(members) = resolve(group) {
                if members.iter().any(|member| {
                    discord_allow_entry_matches(member, sender_id, sender_name, allow_name_matching)
                }) {
                    return true;
                }
            }
            continue;
        }
        if discord_allow_entry_matches(entry, sender_id, sender_name, allow_name_matching) {
            return true;
        }
    }
    false
}

/// Production wrapper resolving access groups through
/// `crate::routing::access_groups::resolve`.
pub fn is_dm_sender_authorized(
    sender_id: &str,
    sender_name: Option<&str>,
    allow_from: &[String],
    allow_name_matching: bool,
) -> bool {
    is_dm_sender_authorized_with(
        sender_id,
        sender_name,
        allow_from,
        allow_name_matching,
        crate::routing::access_groups::resolve,
    )
}

/// Authorize a Discord DM against the account's DM policy + allowFrom lists
/// (including `accessGroup:<name>` entries).
pub fn authorize_discord_dm(
    cfg: &DiscordAccountConfig,
    sender_id: &str,
    sender_name: Option<&str>,
) -> bool {
    let dm = cfg.dm.as_ref().or(cfg.dms.as_ref());
    if let Some(dm) = dm {
        if dm.enabled == Some(false) {
            return false;
        }
    }
    let policy = dm.and_then(|d| d.policy).unwrap_or_default();
    match policy {
        crate::config::DmPolicy::Disabled => false,
        crate::config::DmPolicy::Open => true,
        crate::config::DmPolicy::Allowlist | crate::config::DmPolicy::Pairing => {
            let empty: Vec<String> = Vec::new();
            let allow_from = dm.and_then(|d| d.allow_from.as_ref()).unwrap_or(&empty);
            is_dm_sender_authorized(sender_id, sender_name, allow_from, false)
        }
    }
}

// ============================================================================
// v2026.5.2: Configurable gateway READY timeouts (startup + reconnect)
// ============================================================================
//
// Ported from OpenClaw `extensions/discord/src/monitor/provider.lifecycle.ts`.

/// Startup wait for the gateway READY event before restarting the socket.
pub const DEFAULT_GATEWAY_READY_TIMEOUT_MS: u64 = 15_000;
/// Runtime reconnect wait for the gateway READY event before force-stopping.
pub const DEFAULT_GATEWAY_RUNTIME_READY_TIMEOUT_MS: u64 = 30_000;
/// Poll interval while waiting for the gateway to reach READY.
pub const DISCORD_GATEWAY_READY_POLL_MS: u64 = 250;

/// Resolve `(startup_ready_timeout_ms, runtime_ready_timeout_ms)` for an
/// account, honoring `gatewayReadyTimeoutMs` / `gatewayRuntimeReadyTimeoutMs`.
pub fn resolve_gateway_ready_timeouts(cfg: &DiscordAccountConfig) -> (u64, u64) {
    (
        cfg.gateway_ready_timeout_ms
            .unwrap_or(DEFAULT_GATEWAY_READY_TIMEOUT_MS),
        cfg.gateway_runtime_ready_timeout_ms
            .unwrap_or(DEFAULT_GATEWAY_RUNTIME_READY_TIMEOUT_MS),
    )
}

/// Action produced by polling a [`GatewayReadyWatch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyWatchState {
    /// No socket open — nothing to watch.
    Idle,
    /// Socket opened, still waiting for READY within the deadline.
    Waiting,
    /// Gateway reached READY.
    Connected,
    /// The READY deadline elapsed; the socket must be restarted (startup) or
    /// the lifecycle force-stopped (runtime reconnect).
    TimedOut,
}

/// Deadline state machine for the gateway READY watch. The caller polls it
/// (every [`DISCORD_GATEWAY_READY_POLL_MS`]) with the current connected flag.
#[derive(Debug, Clone)]
pub struct GatewayReadyWatch {
    timeout_ms: u64,
    deadline_at_ms: Option<u64>,
    connected: bool,
}

impl GatewayReadyWatch {
    pub fn new(timeout_ms: u64) -> Self {
        Self {
            timeout_ms,
            deadline_at_ms: None,
            connected: false,
        }
    }

    /// The gateway websocket opened; start (or restart) the READY deadline.
    pub fn on_socket_open(&mut self, now_ms: u64) {
        self.connected = false;
        self.deadline_at_ms = Some(now_ms + self.timeout_ms);
    }

    /// The gateway websocket closed or a reconnect was scheduled; clear the watch.
    pub fn on_socket_closed(&mut self) {
        self.connected = false;
        self.deadline_at_ms = None;
    }

    /// Poll the watch with the current connection state.
    pub fn poll(&mut self, is_connected: bool, now_ms: u64) -> ReadyWatchState {
        if is_connected {
            self.connected = true;
            self.deadline_at_ms = None;
            return ReadyWatchState::Connected;
        }
        match self.deadline_at_ms {
            None => {
                if self.connected {
                    ReadyWatchState::Connected
                } else {
                    ReadyWatchState::Idle
                }
            }
            Some(deadline) if now_ms >= deadline => {
                self.deadline_at_ms = None;
                ReadyWatchState::TimedOut
            }
            Some(_) => ReadyWatchState::Waiting,
        }
    }

    /// Error message used when the READY deadline elapses.
    pub fn timeout_error(&self) -> String {
        format!(
            "discord gateway did not reach READY within {}ms",
            self.timeout_ms
        )
    }
}

// ============================================================================
// v2026.5.2: Components v2 Text Display + forwarded snapshot text
// ============================================================================
//
// Ported from OpenClaw `extensions/discord/src/monitor/message-text.ts` and
// `message-forwarded.ts`. Operates on raw Discord API message JSON.

/// Discord Components v2 `TextDisplay` component type.
const COMPONENT_TYPE_TEXT_DISPLAY: u64 = 10;
/// `message_reference.type` value marking a forward.
const FORWARD_MESSAGE_REFERENCE_TYPE: u64 = 1;

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    let s = value?.as_str()?.trim();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Extract `title\ndescription` text from a Discord embed object.
pub fn resolve_discord_embed_text(embed: Option<&Value>) -> String {
    let title = non_empty_str(embed.and_then(|e| e.get("title"))).unwrap_or("");
    let description = non_empty_str(embed.and_then(|e| e.get("description"))).unwrap_or("");
    if !title.is_empty() && !description.is_empty() {
        format!("{}\n{}", title, description)
    } else if !title.is_empty() {
        title.to_string()
    } else {
        description.to_string()
    }
}

fn collect_text_display_content(value: &Value, parts: &mut Vec<String>) {
    match value {
        Value::Array(entries) => {
            for entry in entries {
                collect_text_display_content(entry, parts);
            }
        }
        Value::Object(map) => {
            if map.get("type").and_then(|t| t.as_u64()) == Some(COMPONENT_TYPE_TEXT_DISPLAY) {
                if let Some(content) = non_empty_str(map.get("content")) {
                    parts.push(content.to_string());
                }
            }
            if let Some(components) = map.get("components") {
                collect_text_display_content(components, parts);
            }
            if let Some(component) = map.get("component") {
                collect_text_display_content(component, parts);
            }
        }
        _ => {}
    }
}

/// Extract Components v2 Text Display content from a `components` tree.
pub fn extract_components_v2_text(components: Option<&Value>) -> String {
    let mut parts = Vec::new();
    if let Some(components) = components {
        collect_text_display_content(components, &mut parts);
    }
    parts.join("\n")
}

/// Format a snapshot/forward author label (`global_name` > `name` > `username`).
pub fn format_discord_snapshot_author(author: Option<&Value>) -> String {
    let Some(author) = author else {
        return String::new();
    };
    for key in ["global_name", "name", "username"] {
        if let Some(label) = non_empty_str(author.get(key)) {
            return label.to_string();
        }
    }
    String::new()
}

fn build_media_placeholder(message: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(attachments) = message.get("attachments").and_then(|a| a.as_array()) {
        for attachment in attachments {
            let name = non_empty_str(attachment.get("filename")).unwrap_or("attachment");
            parts.push(format!("[attachment: {}]", name));
        }
    }
    let stickers = message
        .get("stickers")
        .or_else(|| message.get("sticker_items"))
        .and_then(|s| s.as_array());
    if let Some(stickers) = stickers {
        for sticker in stickers {
            let name = non_empty_str(sticker.get("name")).unwrap_or("sticker");
            parts.push(format!("[sticker: {}]", name));
        }
    }
    parts.join("\n")
}

fn resolve_snapshot_message_text(snapshot_message: &Value) -> String {
    if let Some(content) = non_empty_str(snapshot_message.get("content")) {
        return content.to_string();
    }
    let media = build_media_placeholder(snapshot_message);
    if !media.is_empty() {
        return media;
    }
    let embed_text = resolve_discord_embed_text(
        snapshot_message
            .get("embeds")
            .and_then(|e| e.as_array())
            .and_then(|e| e.first()),
    );
    if !embed_text.is_empty() {
        return embed_text;
    }
    extract_components_v2_text(snapshot_message.get("components"))
}

fn build_forwarded_message_block(snapshot_message: &Value) -> Option<String> {
    let text = resolve_snapshot_message_text(snapshot_message);
    if text.is_empty() {
        return None;
    }
    let author = format_discord_snapshot_author(snapshot_message.get("author"));
    let heading = if author.is_empty() {
        "[Forwarded message]".to_string()
    } else {
        format!("[Forwarded message from {}]", author)
    };
    Some(format!("{}\n{}", heading, text))
}

/// Build forwarded-message text blocks from `message_snapshots`.
pub fn resolve_forwarded_snapshots_text(snapshots: Option<&Value>) -> String {
    let Some(snapshots) = snapshots.and_then(|s| s.as_array()) else {
        return String::new();
    };
    snapshots
        .iter()
        .filter_map(|snapshot| snapshot.get("message"))
        .filter_map(build_forwarded_message_block)
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn is_forward_reference(message: &Value) -> bool {
    message
        .get("message_reference")
        .and_then(|r| r.get("type"))
        .and_then(|t| t.as_u64())
        == Some(FORWARD_MESSAGE_REFERENCE_TYPE)
}

fn resolve_forwarded_messages_text(message: &Value) -> String {
    let snapshot_text = resolve_forwarded_snapshots_text(message.get("message_snapshots"));
    if !snapshot_text.is_empty() {
        return snapshot_text;
    }
    // Recover forwarded referenced message text when snapshots are missing.
    if !is_forward_reference(message) {
        return String::new();
    }
    let Some(referenced) = message.get("referenced_message") else {
        return String::new();
    };
    let referenced_text = resolve_discord_message_text(referenced, false);
    if referenced_text.is_empty() {
        return String::new();
    }
    let author = format_discord_snapshot_author(referenced.get("author"));
    let heading = if author.is_empty() {
        "[Forwarded message]".to_string()
    } else {
        format!("[Forwarded message from {}]", author)
    };
    format!("{}\n{}", heading, referenced_text)
}

fn resolve_inline_mentions(text: &str, message: &Value) -> String {
    if !text.contains('<') {
        return text.to_string();
    }
    let Some(mentions) = message.get("mentions").and_then(|m| m.as_array()) else {
        return text.to_string();
    };
    let mut out = text.to_string();
    for user in mentions {
        let Some(id) = non_empty_str(user.get("id")) else {
            continue;
        };
        let label = non_empty_str(user.get("global_name"))
            .or_else(|| non_empty_str(user.get("username")))
            .unwrap_or("user");
        out = out
            .replace(&format!("<@!{}>", id), &format!("@{}", label))
            .replace(&format!("<@{}>", id), &format!("@{}", label));
    }
    out
}

/// Resolve the effective text of a Discord API message JSON object, including
/// Components v2 Text Display content and (optionally) forwarded snapshots /
/// forwarded referenced replies.
pub fn resolve_discord_message_text(message: &Value, include_forwarded: bool) -> String {
    let embed_text = resolve_discord_embed_text(
        message
            .get("embeds")
            .and_then(|e| e.as_array())
            .and_then(|e| e.first()),
    );
    let component_text = extract_components_v2_text(message.get("components"));
    let raw = if let Some(content) = non_empty_str(message.get("content")) {
        content.to_string()
    } else {
        let media = build_media_placeholder(message);
        if !media.is_empty() {
            media
        } else if !embed_text.is_empty() {
            embed_text
        } else {
            component_text
        }
    };
    let base = resolve_inline_mentions(&raw, message);
    if !include_forwarded {
        return base;
    }
    let forwarded = resolve_forwarded_messages_text(message);
    if forwarded.is_empty() {
        base
    } else if base.is_empty() {
        forwarded
    } else {
        format!("{}\n{}", base, forwarded)
    }
}

// ============================================================================
// v2026.5.2: REST 429 retry against learned bucket/global cooldowns
// ============================================================================
//
// Ported from OpenClaw `extensions/discord/src/internal/rest-scheduler.ts`,
// `retry-after.ts`, and `retry.ts`.

/// Default Discord outbound retry policy (attempts/min/max/jitter).
pub const DISCORD_RETRY_DEFAULT_ATTEMPTS: u32 = 3;
pub const DISCORD_RETRY_DEFAULT_MIN_DELAY_MS: u64 = 500;
pub const DISCORD_RETRY_DEFAULT_MAX_DELAY_MS: u64 = 30_000;

const MAX_SAFE_RETRY_AFTER_SECONDS: f64 = 9_007_199_254_740_991.0 / 1000.0;

/// Whether a Discord REST status code is retryable (408, 429, or 5xx).
pub fn is_retryable_discord_status(status: u16) -> bool {
    status == 408 || status == 429 || status >= 500
}

/// Parse a `Retry-After` header value into seconds. Accepts a delta-seconds
/// integer or an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`).
pub fn parse_retry_after_header_seconds(value: &str, now_ms: u64) -> Option<f64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().all(|c| c.is_ascii_digit()) {
        let secs = trimmed.parse::<f64>().ok()?;
        if secs.is_finite() && (0.0..=MAX_SAFE_RETRY_AFTER_SECONDS).contains(&secs) {
            return Some(secs);
        }
        return None;
    }
    // IMF-fixdate form.
    let parsed =
        chrono::NaiveDateTime::parse_from_str(trimmed, "%a, %d %b %Y %H:%M:%S GMT").ok()?;
    let retry_at_ms = parsed.and_utc().timestamp_millis();
    Some(((retry_at_ms - now_ms as i64).max(0)) as f64 / 1000.0)
}

/// Parse the `retry_after` field of a Discord 429 body into seconds.
pub fn parse_retry_after_body_seconds(value: &Value) -> Option<f64> {
    let secs = match value {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() || !s.chars().all(|c| c.is_ascii_digit() || c == '.') {
                return None;
            }
            s.parse::<f64>().ok()?
        }
        _ => return None,
    };
    if secs.is_finite() && (0.0..=MAX_SAFE_RETRY_AFTER_SECONDS).contains(&secs) {
        Some(secs)
    } else {
        None
    }
}

/// Build a rate-limit route key from method + path: major parameters
/// (`channels/:id`, `guilds/:id`, `webhooks/:id`) keep their literal snowflake;
/// all other numeric segments collapse to `:id`.
pub fn create_discord_route_key(method: &str, path: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut prev = "";
    for segment in path.trim_matches('/').split('/') {
        let is_numeric = !segment.is_empty() && segment.chars().all(|c| c.is_ascii_digit());
        if is_numeric && !matches!(prev, "channels" | "guilds" | "webhooks") {
            out.push(":id".to_string());
        } else {
            out.push(segment.to_string());
        }
        prev = segment;
    }
    format!("{} /{}", method.to_uppercase(), out.join("/"))
}

/// A single rate-limit observation extracted from a REST response.
#[derive(Debug, Clone, Default)]
pub struct RateLimitObservation {
    pub status: u16,
    /// `X-RateLimit-Bucket` header (learned bucket hash).
    pub bucket: Option<String>,
    /// `X-RateLimit-Limit`.
    pub limit: Option<u64>,
    /// `X-RateLimit-Remaining`.
    pub remaining: Option<u64>,
    /// `X-RateLimit-Reset-After` in seconds.
    pub reset_after_secs: Option<f64>,
    /// `Retry-After` header / body `retry_after` in seconds (429 only).
    pub retry_after_secs: Option<f64>,
    /// `X-RateLimit-Global: true` or 429 body `global: true`.
    pub global: bool,
}

/// Learned per-bucket state.
#[derive(Debug, Clone, Default)]
pub struct BucketCooldown {
    pub limit: Option<u64>,
    pub remaining: Option<u64>,
    pub reset_at_ms: u64,
    pub rate_limit_hits: u64,
}

/// Bookkeeping of learned Discord REST rate-limit buckets plus the global
/// cooldown. Queued sends consult [`RateLimitBook::wait_ms`] before dispatch
/// and retry 429s against the learned cooldown rather than blind backoff.
#[derive(Debug, Default)]
pub struct RateLimitBook {
    buckets: HashMap<String, BucketCooldown>,
    route_to_bucket: HashMap<String, String>,
    global_until_ms: u64,
}

impl RateLimitBook {
    pub fn new() -> Self {
        Self::default()
    }

    fn bucket_key_for_route(&self, route_key: &str) -> String {
        self.route_to_bucket
            .get(route_key)
            .cloned()
            .unwrap_or_else(|| route_key.to_string())
    }

    /// Record a response's rate-limit headers (and 429 retry-after) for a route.
    pub fn record(&mut self, route_key: &str, obs: &RateLimitObservation, now_ms: u64) {
        let bucket_key = match &obs.bucket {
            Some(bucket_header) => {
                // Bind the route to the shared bucket learned from the header so
                // sibling routes on the same bucket honor the same cooldown.
                self.route_to_bucket
                    .insert(route_key.to_string(), bucket_header.clone());
                bucket_header.clone()
            }
            None => self.bucket_key_for_route(route_key),
        };
        let bucket = self.buckets.entry(bucket_key).or_default();
        if let Some(limit) = obs.limit {
            bucket.limit = Some(limit);
        }
        if let Some(remaining) = obs.remaining {
            bucket.remaining = Some(remaining);
        }
        if let Some(reset_after) = obs.reset_after_secs {
            bucket.reset_at_ms = now_ms + (reset_after * 1000.0) as u64;
        }
        if obs.status != 429 {
            return;
        }
        bucket.rate_limit_hits += 1;
        let retry_after_ms = (obs.retry_after_secs.unwrap_or(1.0).max(0.0) * 1000.0) as u64;
        let retry_at = now_ms + retry_after_ms;
        if obs.global {
            self.global_until_ms = self.global_until_ms.max(retry_at);
            return;
        }
        bucket.remaining = Some(0);
        bucket.reset_at_ms = bucket.reset_at_ms.max(retry_at);
    }

    /// Remaining global cooldown in ms.
    pub fn global_wait_ms(&self, now_ms: u64) -> u64 {
        self.global_until_ms.saturating_sub(now_ms)
    }

    /// How long a request on `route_key` must wait before dispatch
    /// (global cooldown first, then the learned bucket cooldown).
    pub fn wait_ms(&self, route_key: &str, now_ms: u64) -> u64 {
        let global = self.global_wait_ms(now_ms);
        if global > 0 {
            return global;
        }
        let bucket_key = self.bucket_key_for_route(route_key);
        match self.buckets.get(&bucket_key) {
            Some(bucket) if bucket.remaining == Some(0) && bucket.reset_at_ms > now_ms => {
                bucket.reset_at_ms - now_ms
            }
            _ => 0,
        }
    }

    /// Learned state for diagnostics.
    pub fn bucket_for_route(&self, route_key: &str) -> Option<&BucketCooldown> {
        self.buckets.get(&self.bucket_key_for_route(route_key))
    }
}

/// Compute the delay before retrying a queued request: exponential backoff
/// (capped) raised to at least the learned bucket/global cooldown.
pub fn compute_queued_retry_delay_ms(
    attempt: u32,
    retry: &OutboundRetryConfig,
    learned_wait_ms: u64,
) -> u64 {
    let exp = attempt.saturating_sub(1).min(16);
    let backoff = retry
        .min_delay_ms
        .saturating_mul(1u64 << exp)
        .min(retry.max_delay_ms);
    backoff.max(learned_wait_ms)
}

// ============================================================================
// v2026.5.2: Outbound mention aliases + canonical mention formatting
// ============================================================================
//
// Ported from OpenClaw `extensions/discord/src/mentions.ts`.

const DISCORD_RESERVED_MENTIONS: [&str; 2] = ["everyone", "here"];

fn mention_candidate_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)(^|[\s(\[{"'.,;:!?])@([a-z0-9_.\-]{2,32}(?:#[0-9]{4})?)"#)
            .expect("valid mention pattern")
    })
}

fn targeted_mention_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<@!?\d+>|<@&\d+>").expect("valid targeted mention pattern"))
}

fn broadcast_mention_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"@(everyone|here)\b").expect("valid broadcast mention pattern"))
}

fn normalize_snowflake(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit()) {
        Some(trimmed.to_string())
    } else {
        None
    }
}

/// Format a canonical user mention (`<@USER_ID>`); `None` for invalid ids.
pub fn format_user_mention(user_id: &str) -> Option<String> {
    normalize_snowflake(user_id).map(|id| format!("<@{}>", id))
}

/// Format a canonical channel mention (`<#CHANNEL_ID>`).
pub fn format_channel_mention(channel_id: &str) -> Option<String> {
    normalize_snowflake(channel_id).map(|id| format!("<#{}>", id))
}

/// Format a canonical role mention (`<@&ROLE_ID>`).
pub fn format_role_mention(role_id: &str) -> Option<String> {
    normalize_snowflake(role_id).map(|id| format!("<@&{}>", id))
}

fn normalize_handle_key(raw: &str) -> Option<String> {
    let mut handle = raw.trim();
    if let Some(stripped) = handle.strip_prefix('@') {
        handle = stripped.trim();
    }
    if handle.is_empty() || handle.chars().any(|c| c.is_whitespace()) {
        return None;
    }
    Some(handle.to_lowercase())
}

fn strip_discriminator(handle: &str) -> &str {
    if let Some(idx) = handle.rfind('#') {
        let suffix = &handle[idx + 1..];
        if suffix.len() == 4 && suffix.chars().all(|c| c.is_ascii_digit()) {
            return &handle[..idx];
        }
    }
    handle
}

fn resolve_configured_mention_alias(
    handle: &str,
    aliases: Option<&HashMap<String, String>>,
) -> Option<String> {
    let key = normalize_handle_key(handle)?;
    let aliases = aliases?;
    let without_discriminator = strip_discriminator(&key);
    for (raw_alias, raw_user_id) in aliases {
        let Some(alias) = normalize_handle_key(raw_alias) else {
            continue;
        };
        let alias_without_discriminator = strip_discriminator(&alias);
        let matches = alias == key
            || (without_discriminator != key && alias == without_discriminator)
            || (alias_without_discriminator != alias && alias_without_discriminator == key);
        if matches {
            if let Some(user_id) = normalize_snowflake(raw_user_id) {
                return Some(user_id);
            }
        }
    }
    None
}

fn rewrite_plain_text_mentions(text: &str, aliases: Option<&HashMap<String, String>>) -> String {
    if !text.contains('@') {
        return text.to_string();
    }
    mention_candidate_pattern()
        .replace_all(text, |caps: &regex::Captures| {
            let prefix = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let handle = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let lookup = handle.to_lowercase();
            if DISCORD_RESERVED_MENTIONS.contains(&lookup.as_str()) {
                return caps[0].to_string();
            }
            match resolve_configured_mention_alias(handle, aliases) {
                Some(user_id) => format!("{}<@{}>", prefix, user_id),
                None => caps[0].to_string(),
            }
        })
        .to_string()
}

fn count_backtick_run(bytes: &[u8], index: usize) -> usize {
    let mut cursor = index;
    while cursor < bytes.len() && bytes[cursor] == b'`' {
        cursor += 1;
    }
    cursor - index
}

fn find_same_line_backtick_run(text: &str, start_index: usize, run_length: usize) -> Option<usize> {
    let delimiter = "`".repeat(run_length);
    let line_end = text[start_index..]
        .find('\n')
        .map(|i| start_index + i)
        .unwrap_or(text.len());
    let close_index = text[start_index..].find(&delimiter).map(|i| start_index + i)?;
    if close_index < line_end {
        Some(close_index + run_length)
    } else {
        None
    }
}

fn find_fence_end(text: &str, start_index: usize, run_length: usize) -> usize {
    let bytes = text.as_bytes();
    let mut search_index = start_index + run_length;
    while search_index < text.len() {
        let Some(newline_index) = text[search_index..].find('\n').map(|i| search_index + i) else {
            return text.len();
        };
        let mut line_cursor = newline_index + 1;
        while line_cursor < bytes.len()
            && bytes[line_cursor] == b' '
            && line_cursor - newline_index <= 3
        {
            line_cursor += 1;
        }
        let closing_run_length = count_backtick_run(bytes, line_cursor);
        if closing_run_length >= run_length {
            return line_cursor + closing_run_length;
        }
        search_index = line_cursor + closing_run_length.max(1);
    }
    text.len()
}

fn find_next_markdown_code_segment(text: &str, start_index: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut search_index = start_index;
    while search_index < text.len() {
        let segment_start = text[search_index..].find('`').map(|i| search_index + i)?;
        let run_length = count_backtick_run(bytes, segment_start);
        if let Some(inline_end) =
            find_same_line_backtick_run(text, segment_start + run_length, run_length)
        {
            return Some((segment_start, inline_end));
        }
        if run_length >= 3 {
            return Some((segment_start, find_fence_end(text, segment_start, run_length)));
        }
        search_index = segment_start + run_length;
    }
    None
}

/// Rewrite known `@Name` handles in outbound text into real Discord user
/// mentions using the configured `mentionAliases` map, skipping inline code
/// spans and fenced code blocks. Reserved broadcast handles
/// (`@everyone`/`@here`) are never rewritten.
pub fn rewrite_discord_known_mentions(
    text: &str,
    aliases: Option<&HashMap<String, String>>,
) -> String {
    if !text.contains('@') {
        return text.to_string();
    }
    let mut rewritten = String::with_capacity(text.len());
    let mut offset = 0usize;
    while let Some((start, end)) = find_next_markdown_code_segment(text, offset) {
        rewritten.push_str(&rewrite_plain_text_mentions(&text[offset..start], aliases));
        rewritten.push_str(&text[start..end]);
        offset = end;
    }
    rewritten.push_str(&rewrite_plain_text_mentions(&text[offset..], aliases));
    rewritten
}

/// Whether text carries a Discord user/role mention that pings when sent fresh.
pub fn discord_text_has_targeted_mention(text: &str) -> bool {
    targeted_mention_pattern().is_match(text)
}

/// Whether text carries an `@everyone`/`@here` broadcast mention.
pub fn discord_text_has_broadcast_mention(text: &str) -> bool {
    broadcast_mention_pattern().is_match(text)
}

// ============================================================================
// v2026.5.2: Canonical mention prompt hints
// ============================================================================
//
// Ported from OpenClaw `extensions/discord/src/channel.ts` (agentPrompt
// messageToolHints).

/// Canonical outbound mention syntax hint injected into the agent prompt.
pub const DISCORD_MENTION_PROMPT_HINT: &str = "- Discord mentions: use canonical outbound syntax: users `<@USER_ID>`, channels `<#CHANNEL_ID>`, and roles `<@&ROLE_ID>`. Plain `@name` text only pings when a configured `mentionAliases` entry rewrites it; do not use the legacy `<@!USER_ID>` nickname form.";

/// Message-tool prompt hints for the Discord channel.
pub fn discord_message_tool_hints() -> Vec<&'static str> {
    vec![
        DISCORD_MENTION_PROMPT_HINT,
        "- Discord components: set `components` when sending messages to include buttons, selects, or v2 containers.",
        "- Forms: add `components.modal` (title, fields). OpenClaw adds a trigger button and routes submissions as new messages.",
    ]
}

// ============================================================================
// v2026.5.2: Reaction listener registration gate
// ============================================================================
//
// Ported from OpenClaw `extensions/discord/src/monitor/provider.startup.ts`
// (`shouldRegisterDiscordReactionListeners`).

/// Skip reaction listener registration when DMs (and group DMs) are disabled
/// and no guild has reaction notifications enabled.
pub fn should_register_reaction_listeners(
    dm_enabled: bool,
    group_dm_enabled: bool,
    group_policy: GroupPolicy,
    guild_entries: Option<&HashMap<String, DiscordGuildEntry>>,
) -> bool {
    if dm_enabled || group_dm_enabled {
        return true;
    }
    if group_policy == GroupPolicy::Disabled {
        return false;
    }
    let Some(entries) = guild_entries.filter(|entries| !entries.is_empty()) else {
        return true;
    };
    entries
        .values()
        .any(|entry| entry.reaction_notifications.as_deref() != Some("off"))
}

// ============================================================================
// v2026.5.2: Typing indicators alive during long tool runs / auto-compaction
// ============================================================================
//
// Ported from OpenClaw `extensions/discord/src/monitor/reply-typing-feedback.ts`
// and `message-handler.reply-typing-policy.ts`. Discord owns the typing
// heartbeat for the whole reply lifecycle (core typing keepalive is disabled),
// so the indicator stays alive through long tool runs and auto-compaction.

/// Discord can keep long tool-heavy replies alive, but not forever.
/// The dispatch restart path refreshes this TTL after queue wait time.
pub const DISCORD_REPLY_TYPING_MAX_DURATION_MS: u64 = 20 * 60_000;
/// Discord typing indicators expire after ~10s; refresh comfortably sooner.
pub const DISCORD_TYPING_KEEPALIVE_INTERVAL_MS: u64 = 8_000;

/// One stable typing owner for a Discord reply. The inner keepalive loop can
/// rotate between prequeue feedback and the actual dispatch lifecycle
/// (`restart_for_dispatch`) without changing the owner.
pub struct DiscordReplyTypingFeedback {
    channel_id: Arc<Mutex<String>>,
    on_typing: Arc<dyn Fn(String) + Send + Sync>,
    interval_ms: u64,
    max_duration_ms: u64,
    current: Mutex<Option<Arc<AtomicBool>>>,
}

impl DiscordReplyTypingFeedback {
    pub fn new<F>(channel_id: impl Into<String>, on_typing: F) -> Self
    where
        F: Fn(String) + Send + Sync + 'static,
    {
        Self {
            channel_id: Arc::new(Mutex::new(channel_id.into())),
            on_typing: Arc::new(on_typing),
            interval_ms: DISCORD_TYPING_KEEPALIVE_INTERVAL_MS,
            max_duration_ms: DISCORD_REPLY_TYPING_MAX_DURATION_MS,
            current: Mutex::new(None),
        }
    }

    /// Override interval/TTL (tests, config).
    pub fn with_timing(mut self, interval_ms: u64, max_duration_ms: u64) -> Self {
        self.interval_ms = interval_ms;
        self.max_duration_ms = max_duration_ms;
        self
    }

    /// Current typing target channel.
    pub fn channel_id(&self) -> String {
        self.channel_id.lock().unwrap().clone()
    }

    /// The typing owner follows the final target before reply dispatch starts.
    pub fn update_channel_id(&self, next: &str) {
        let trimmed = next.trim();
        if !trimmed.is_empty() {
            *self.channel_id.lock().unwrap() = trimmed.to_string();
        }
    }

    /// Whether the keepalive loop is currently running.
    pub fn is_running(&self) -> bool {
        self.current
            .lock()
            .unwrap()
            .as_ref()
            .map(|running| running.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    /// Start the typing heartbeat (idempotent). Fires immediately, then every
    /// interval until stopped or the TTL elapses. The loop deliberately keeps
    /// ticking while tools run and during auto-compaction.
    pub fn on_reply_start(&self) {
        let mut guard = self.current.lock().unwrap();
        if guard
            .as_ref()
            .map(|running| running.load(Ordering::Relaxed))
            .unwrap_or(false)
        {
            return;
        }
        let running = Arc::new(AtomicBool::new(true));
        *guard = Some(running.clone());
        drop(guard);

        let channel_id = self.channel_id.clone();
        let on_typing = self.on_typing.clone();
        let interval_ms = self.interval_ms;
        let max_duration_ms = self.max_duration_ms;
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            loop {
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                if started.elapsed().as_millis() as u64 > max_duration_ms {
                    running.store(false, Ordering::Relaxed);
                    break;
                }
                let target = channel_id.lock().unwrap().clone();
                on_typing(target);
                tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
            }
        });
    }

    /// Auto-compaction started: keep the indicator alive (no-op by design —
    /// the heartbeat continues; upstream additionally flips a status reaction).
    pub fn on_compaction_start(&self) {}

    /// Auto-compaction finished: heartbeat is still running; nothing to do.
    pub fn on_compaction_end(&self) {}

    /// Stop the heartbeat (reply became idle).
    pub fn on_idle(&self) {
        self.stop();
    }

    /// Stop the heartbeat and release resources.
    pub fn on_cleanup(&self) {
        self.stop();
    }

    fn stop(&self) {
        if let Some(running) = self.current.lock().unwrap().take() {
            running.store(false, Ordering::Relaxed);
        }
    }

    /// Prequeue typing may have hit its TTL before the job starts. Rotate the
    /// inner loop so dispatch always owns a live heartbeat (fresh TTL).
    pub fn restart_for_dispatch(&self, next_channel_id: &str) {
        self.update_channel_id(next_channel_id);
        self.stop();
        self.on_reply_start();
    }
}

impl Drop for DiscordReplyTypingFeedback {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Why a typing prestart decision was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypingPrestartReason {
    Aborted,
    Empty,
    RoomEvent,
    ConfiguredInstant,
    ConfiguredNotInstant,
    ToolOnly,
    Direct,
    MentionedGroup,
    DeferToMessage,
}

/// Inputs for [`resolve_accepted_typing_prestart`].
#[derive(Debug, Clone, Default)]
pub struct TypingPrestartParams<'a> {
    pub aborted: bool,
    pub message_text: &'a str,
    pub is_room_event: bool,
    /// `session.typingMode` / `agents.defaults.typingMode` when configured.
    pub configured_typing_mode: Option<&'a str>,
    /// Source replies are delivered via the message tool only.
    pub source_reply_tool_only: bool,
    pub is_guild_message: bool,
    pub is_group_dm: bool,
    pub was_mentioned: bool,
}

/// Decide whether to prestart the typing indicator for an accepted message.
pub fn resolve_accepted_typing_prestart(
    params: &TypingPrestartParams,
) -> (bool, TypingPrestartReason) {
    if params.aborted {
        return (false, TypingPrestartReason::Aborted);
    }
    if params.message_text.trim().is_empty() {
        return (false, TypingPrestartReason::Empty);
    }
    if params.is_room_event {
        return (false, TypingPrestartReason::RoomEvent);
    }
    if let Some(mode) = params.configured_typing_mode {
        // Explicit operator config wins over Discord heuristics.
        return if mode == "instant" {
            (true, TypingPrestartReason::ConfiguredInstant)
        } else {
            (false, TypingPrestartReason::ConfiguredNotInstant)
        };
    }
    if params.source_reply_tool_only {
        // Message-tool-only replies have no visible default response path;
        // prestart preserves user feedback while the tool-delivered reply waits.
        return (true, TypingPrestartReason::ToolOnly);
    }
    if !params.is_guild_message && !params.is_group_dm {
        return (true, TypingPrestartReason::Direct);
    }
    if params.was_mentioned {
        return (true, TypingPrestartReason::MentionedGroup);
    }
    (false, TypingPrestartReason::DeferToMessage)
}

// ============================================================================
// v2026.5.2: PluralKit dedupe + thread starter context on first turn only
// ============================================================================
//
// Ported from OpenClaw `extensions/discord/src/pluralkit.ts`,
// `monitor/inbound-dedupe.ts`, and `monitor/message-handler.context.ts`.

/// PluralKit API base.
const PLURALKIT_API_BASE: &str = "https://api.pluralkit.me/v2";

/// PluralKit proxied message info (subset).
#[derive(Debug, Clone, Deserialize)]
pub struct PluralKitMessageInfo {
    pub id: String,
    /// The original (pre-proxy) message id — used as the canonical dedupe id.
    pub original: Option<String>,
    pub sender: Option<String>,
    #[serde(default)]
    pub system: Option<Value>,
    #[serde(default)]
    pub member: Option<Value>,
}

/// Fetch PluralKit message info for a proxied message. Returns `Ok(None)` when
/// PluralKit is disabled or the message is unknown (404).
pub async fn fetch_pluralkit_message_info(
    enabled: bool,
    message_id: &str,
) -> Result<Option<PluralKitMessageInfo>> {
    if !enabled {
        return Ok(None);
    }
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/messages/{}", PLURALKIT_API_BASE, message_id))
        .send()
        .await?;
    if resp.status().as_u16() == 404 {
        return Ok(None);
    }
    if !resp.status().is_success() {
        anyhow::bail!("PluralKit API failed ({})", resp.status().as_u16());
    }
    Ok(Some(resp.json::<PluralKitMessageInfo>().await?))
}

/// Prefer the PluralKit original message id as the canonical message id so a
/// proxied re-post and its original dedupe to the same inbound event.
pub fn resolve_canonical_message_id<'a>(
    message_id: &'a str,
    pluralkit_original: Option<&'a str>,
) -> &'a str {
    match pluralkit_original.map(str::trim) {
        Some(original) if !original.is_empty() => original,
        _ => message_id,
    }
}

/// Build the inbound replay-dedupe key: `{account}:{channel}:{canonical_id}`.
pub fn build_inbound_replay_key(
    account_id: &str,
    channel_id: &str,
    canonical_message_id: &str,
) -> Option<String> {
    let message_id = canonical_message_id.trim();
    let channel_id = channel_id.trim();
    if message_id.is_empty() || channel_id.is_empty() {
        return None;
    }
    Some(format!("{}:{}:{}", account_id, channel_id, message_id))
}

const REPLAY_GUARD_TTL_MS: u64 = 5 * 60_000;
const REPLAY_GUARD_MAX: usize = 5000;

/// Claimable TTL dedupe guard for inbound Discord messages (5 min / 5000 keys).
#[derive(Debug, Default)]
pub struct InboundReplayGuard {
    entries: Mutex<HashMap<String, (u64, bool)>>,
}

impl InboundReplayGuard {
    pub fn new() -> Self {
        Self::default()
    }

    fn prune(entries: &mut HashMap<String, (u64, bool)>, now_ms: u64) {
        entries.retain(|_, (at, _)| now_ms.saturating_sub(*at) < REPLAY_GUARD_TTL_MS);
        while entries.len() >= REPLAY_GUARD_MAX {
            let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            entries.remove(&oldest);
        }
    }

    /// Claim a replay key. Returns `true` when newly claimed (process the
    /// message) and `false` for a duplicate within the TTL window.
    pub fn claim(&self, replay_key: &str, now_ms: u64) -> bool {
        let key = replay_key.trim();
        if key.is_empty() {
            return true;
        }
        let mut entries = self.entries.lock().unwrap();
        Self::prune(&mut entries, now_ms);
        if entries.contains_key(key) {
            return false;
        }
        entries.insert(key.to_string(), (now_ms, false));
        true
    }

    /// Commit a processed replay key so late duplicates stay deduped.
    pub fn commit(&self, replay_key: &str, now_ms: u64) {
        let key = replay_key.trim();
        if key.is_empty() {
            return;
        }
        self.entries
            .lock()
            .unwrap()
            .insert(key.to_string(), (now_ms, true));
    }

    /// Release an uncommitted claim (processing failed → allow retry).
    pub fn release(&self, replay_key: &str) {
        let key = replay_key.trim();
        if key.is_empty() {
            return;
        }
        let mut entries = self.entries.lock().unwrap();
        if let Some((_, committed)) = entries.get(key) {
            if !committed {
                entries.remove(key);
            }
        }
    }
}

/// Thread starter context is included only on the first turn of a thread
/// session (no previous timestamp) and only when the channel config does not
/// disable it (`includeThreadStarter !== false`).
pub fn should_include_thread_starter(
    channel_include_config: Option<bool>,
    has_previous_turn: bool,
) -> bool {
    channel_include_config != Some(false) && !has_previous_turn
}

// ============================================================================
// v2026.5.2 / v2026.4.25: Voice — text-only default, hidden tts tool,
// per-channel systemPrompt overrides, voice LLM model override
// ============================================================================
//
// Ported from OpenClaw `extensions/discord/src/voice/prompt.ts`,
// `src/config/types.discord.ts` (DiscordVoiceConfig), and
// `monitor/inbound-context.ts`.

/// Spoken-output contract for Discord voice turns. Voice is text-only by
/// default: the agent returns plain text and Discord voice synthesizes it.
pub const DISCORD_VOICE_SPOKEN_OUTPUT_CONTRACT: &str = "You are the Discord voice interface in a live voice channel.\nDiscord voice reply requirements:\n- Return only the concise text that should be spoken aloud in the voice channel.\n- Treat the transcript as speech-to-text from a live conversation; repair obvious transcription artifacts and ignore repeated partial fragments caused by voice buffering.\n- If the transcript is garbled, incomplete, or missing the user's intent, ask one brief clarifying question instead of guessing.\n- If the request needs deeper reasoning, current information, or tools, use the available tools before answering.\n- Do not call the tts tool; Discord voice will synthesize and play the returned text.\n- Do not reply with NO_REPLY unless no spoken response is appropriate.\n- Keep the response brief, natural, and conversational. Prefer one to three short sentences.\n- Avoid markdown tables, code fences, citations, and visual formatting unless the user explicitly asks for something that cannot be spoken naturally.";

/// Tools hidden from the agent on Discord voice turns: the voice-output policy
/// owns synthesis, so the agent-side `tts` tool is removed.
pub const DISCORD_VOICE_HIDDEN_TOOLS: &[&str] = &["tts"];

/// Whether a tool must be hidden from the agent for a Discord voice turn.
pub fn is_tool_hidden_for_voice(tool_name: &str) -> bool {
    DISCORD_VOICE_HIDDEN_TOOLS
        .iter()
        .any(|hidden| hidden.eq_ignore_ascii_case(tool_name))
}

/// Whether Discord voice conversations are enabled (default: true).
pub fn voice_enabled(voice: Option<&DiscordVoiceConfig>) -> bool {
    voice.and_then(|v| v.enabled).unwrap_or(true)
}

/// Voice conversation mode (default: "agent-proxy").
pub fn resolve_voice_mode(voice: Option<&DiscordVoiceConfig>) -> &str {
    voice
        .and_then(|v| v.mode.as_deref())
        .filter(|mode| !mode.trim().is_empty())
        .unwrap_or("agent-proxy")
}

/// Optional LLM model override for Discord voice channel responses
/// (`channels.discord.voice.model`, v2026.4.25).
pub fn resolve_voice_model_override(voice: Option<&DiscordVoiceConfig>) -> Option<&str> {
    voice
        .and_then(|v| v.model.as_deref())
        .map(str::trim)
        .filter(|model| !model.is_empty())
}

/// Format the ingress prompt for a voice transcript turn.
pub fn format_voice_ingress_prompt(transcript: &str, speaker_label: Option<&str>) -> String {
    let cleaned = transcript.trim();
    let voice_input = match speaker_label.map(str::trim).filter(|l| !l.is_empty()) {
        Some(label) => format!("Voice transcript from speaker \"{}\":\n{}", label, cleaned),
        None => cleaned.to_string(),
    };
    format!("{}\n\n{}", DISCORD_VOICE_SPOKEN_OUTPUT_CONTRACT, voice_input)
}

/// Resolve the per-channel `systemPrompt` override for a guild channel.
pub fn resolve_channel_system_prompt(
    channel_config: Option<&DiscordGuildChannelConfig>,
) -> Option<String> {
    let prompt = channel_config?.system_prompt.as_deref()?.trim();
    if prompt.is_empty() {
        None
    } else {
        Some(prompt.to_string())
    }
}

// ============================================================================
// v2026.4.8: Text command parsing + interactive arg dialogs
// ============================================================================
//
// Ported from OpenClaw `extensions/discord/src/monitor/native-command-arg-ui.ts`
// and `src/auto-reply/commands-registry.ts` (parse/menu/title helpers).

/// A slash-style command parsed from plain message text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTextCommand {
    /// Command name without the leading `/`.
    pub name: String,
    /// Raw argument text (trimmed), if any.
    pub args_raw: Option<String>,
}

/// Parse a plain-text command (`/name args…`) from an inbound Discord message,
/// tolerating a leading bot mention (`<@id> /name`). Returns `None` for
/// non-command text (including path-like text such as `/usr/bin`).
pub fn parse_discord_text_command(text: &str) -> Option<ParsedTextCommand> {
    let mut t = text.trim();
    // Strip leading bot mentions.
    loop {
        if t.starts_with("<@") {
            if let Some(end) = t.find('>') {
                let inner = &t[2..end];
                let inner = inner.strip_prefix('!').unwrap_or(inner);
                if !inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit()) {
                    t = t[end + 1..].trim_start();
                    continue;
                }
            }
        }
        break;
    }
    let rest = t.strip_prefix('/')?;
    let name_end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .unwrap_or(rest.len());
    if name_end == 0 {
        return None;
    }
    let name = &rest[..name_end];
    let after = &rest[name_end..];
    // Only whitespace (or end) may follow the command name — `/usr/bin` is not
    // a command.
    if !after.is_empty() && !after.starts_with(char::is_whitespace) {
        return None;
    }
    let args_raw = after.trim();
    Some(ParsedTextCommand {
        name: name.to_string(),
        args_raw: if args_raw.is_empty() {
            None
        } else {
            Some(args_raw.to_string())
        },
    })
}

/// One selectable choice in a command-arg dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandArgChoice {
    pub value: String,
    pub label: String,
}

/// A button of a command-arg dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandArgButton {
    pub label: String,
    pub custom_id: String,
}

/// An interactive dialog prompting for a missing command argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandArgMenu {
    pub content: String,
    /// Button rows (max 4 buttons per row, mirroring upstream chunking).
    pub rows: Vec<Vec<CommandArgButton>>,
}

const COMMAND_ARG_CUSTOM_ID_KEY: &str = "cmdarg";
const COMMAND_ARG_BUTTONS_PER_ROW: usize = 4;

fn encode_command_arg_value(value: &str) -> String {
    // encodeURIComponent-compatible percent encoding.
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')');
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

fn decode_command_arg_value(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).and_then(|b| (*b as char).to_digit(16)),
                bytes.get(i + 2).and_then(|b| (*b as char).to_digit(16)),
            ) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Build the component custom id for a command-arg dialog button:
/// `cmdarg:command=<c>;arg=<a>;value=<v>;user=<u>` (percent-encoded fields).
pub fn build_command_arg_custom_id(command: &str, arg: &str, value: &str, user_id: &str) -> String {
    format!(
        "{}:command={};arg={};value={};user={}",
        COMMAND_ARG_CUSTOM_ID_KEY,
        encode_command_arg_value(command),
        encode_command_arg_value(arg),
        encode_command_arg_value(value),
        encode_command_arg_value(user_id),
    )
}

/// Parse a command-arg custom id back into `(command, arg, value, user_id)`.
pub fn parse_command_arg_custom_id(custom_id: &str) -> Option<(String, String, String, String)> {
    let rest = custom_id.strip_prefix(&format!("{}:", COMMAND_ARG_CUSTOM_ID_KEY))?;
    let mut command = None;
    let mut arg = None;
    let mut value = None;
    let mut user = None;
    for part in rest.split(';') {
        let (key, raw) = part.split_once('=')?;
        let decoded = decode_command_arg_value(raw);
        match key {
            "command" => command = Some(decoded),
            "arg" => arg = Some(decoded),
            "value" => value = Some(decoded),
            "user" => user = Some(decoded),
            _ => {}
        }
    }
    Some((command?, arg?, value?, user?))
}

/// Build an interactive dialog for a missing command argument. `title`
/// overrides the generated `Choose <arg> for /<command>.` content.
pub fn build_command_arg_menu(
    command_label: &str,
    arg_name: &str,
    arg_description: Option<&str>,
    title: Option<&str>,
    choices: &[CommandArgChoice],
    user_id: &str,
) -> CommandArgMenu {
    let content = match title.map(str::trim).filter(|t| !t.is_empty()) {
        Some(title) => title.to_string(),
        None => {
            let arg_label = arg_description
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .unwrap_or(arg_name);
            format!("Choose {} for /{}.", arg_label, command_label)
        }
    };
    let rows = choices
        .chunks(COMMAND_ARG_BUTTONS_PER_ROW)
        .map(|chunk| {
            chunk
                .iter()
                .map(|choice| CommandArgButton {
                    label: choice.label.clone(),
                    custom_id: build_command_arg_custom_id(
                        command_label,
                        arg_name,
                        &choice.value,
                        user_id,
                    ),
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    CommandArgMenu { content, rows }
}

// ============================================================================
// v2026.5.2: REST client with learned cooldowns + retry
// ============================================================================

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn read_header_u64(headers: &reqwest::header::HeaderMap, name: &str) -> Option<u64> {
    headers
        .get(name)?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| v as u64)
}

fn read_header_f64(headers: &reqwest::header::HeaderMap, name: &str) -> Option<f64> {
    headers
        .get(name)?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

/// Extract a [`RateLimitObservation`] from response status/headers/body.
pub fn observe_rate_limit_response(
    status: u16,
    headers: &reqwest::header::HeaderMap,
    body: Option<&Value>,
    now_ms: u64,
) -> RateLimitObservation {
    let body_retry_after = body
        .and_then(|b| b.get("retry_after"))
        .and_then(parse_retry_after_body_seconds);
    let header_retry_after = headers
        .get("Retry-After")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| parse_retry_after_header_seconds(v, now_ms));
    let body_global = body
        .and_then(|b| b.get("global"))
        .and_then(|g| g.as_bool())
        .unwrap_or(false);
    let header_global = headers
        .get("X-RateLimit-Global")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    RateLimitObservation {
        status,
        bucket: headers
            .get("X-RateLimit-Bucket")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_string()),
        limit: read_header_u64(headers, "X-RateLimit-Limit"),
        remaining: read_header_u64(headers, "X-RateLimit-Remaining"),
        reset_after_secs: read_header_f64(headers, "X-RateLimit-Reset-After"),
        retry_after_secs: body_retry_after.or(header_retry_after),
        global: header_global || body_global,
    }
}

/// Discord REST client that retries queued requests against learned
/// bucket/global cooldowns (v2026.5.2) instead of blind backoff.
pub struct DiscordRestClient {
    http: reqwest::Client,
    token: String,
    retry: OutboundRetryConfig,
    book: Mutex<RateLimitBook>,
}

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

impl DiscordRestClient {
    pub fn new(token: impl Into<String>, retry: Option<OutboundRetryConfig>) -> Self {
        Self {
            http: reqwest::Client::new(),
            token: token.into(),
            retry: retry.unwrap_or_default(),
            book: Mutex::new(RateLimitBook::new()),
        }
    }

    /// Perform a JSON request with learned-cooldown waits and 429/transient
    /// retries. `path` is relative to the API base (e.g. `channels/123/messages`).
    pub async fn request_json(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        let route_key = create_discord_route_key(method.as_str(), path);
        let url = format!("{}/{}", DISCORD_API_BASE, path.trim_start_matches('/'));
        let attempts = self.retry.attempts.max(1);
        let mut last_error: Option<anyhow::Error> = None;

        for attempt in 1..=attempts {
            // Honor learned bucket/global cooldowns before dispatch.
            let wait = self.book.lock().unwrap().wait_ms(&route_key, now_unix_ms());
            if wait > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
            }

            let mut request = self
                .http
                .request(method.clone(), &url)
                .header("Authorization", format!("Bot {}", self.token));
            if let Some(body) = body {
                request = request.json(body);
            }

            match request.send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let headers = resp.headers().clone();
                    let text = resp.text().await.unwrap_or_default();
                    let parsed: Option<Value> = serde_json::from_str(&text).ok();
                    let now = now_unix_ms();
                    let obs =
                        observe_rate_limit_response(status, &headers, parsed.as_ref(), now);
                    self.book.lock().unwrap().record(&route_key, &obs, now);

                    if (200..300).contains(&status) {
                        return Ok(parsed.unwrap_or(Value::Null));
                    }
                    let err = anyhow::anyhow!(
                        "discord REST {} {} failed ({}): {}",
                        route_key,
                        url,
                        status,
                        text.chars().take(300).collect::<String>()
                    );
                    if !is_retryable_discord_status(status) || attempt == attempts {
                        return Err(err);
                    }
                    last_error = Some(err);
                    let learned = self.book.lock().unwrap().wait_ms(&route_key, now_unix_ms());
                    let delay = compute_queued_retry_delay_ms(attempt, &self.retry, learned);
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                Err(err) => {
                    // Network-level errors are transient-retryable.
                    if attempt == attempts {
                        return Err(err.into());
                    }
                    last_error = Some(err.into());
                    let delay = compute_queued_retry_delay_ms(attempt, &self.retry, 0);
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("discord REST retries exhausted")))
    }

    /// Send a text message to a channel.
    pub async fn send_channel_message(&self, channel_id: &str, content: &str) -> Result<Value> {
        self.request_json(
            reqwest::Method::POST,
            &format!("channels/{}/messages", channel_id),
            Some(&serde_json::json!({ "content": content })),
        )
        .await
    }

    /// Trigger the typing indicator for a channel.
    pub async fn trigger_typing(&self, channel_id: &str) -> Result<()> {
        self.request_json(
            reqwest::Method::POST,
            &format!("channels/{}/typing", channel_id),
            None,
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod parity_tests {
    use super::*;
    use serde_json::json;

    // ---- access-group DM authorization -------------------------------------

    #[test]
    fn parse_access_group_entry() {
        assert_eq!(
            parse_access_group_allow_from_entry("accessGroup:family"),
            Some("family")
        );
        assert_eq!(
            parse_access_group_allow_from_entry("  accessGroup: ops "),
            Some("ops")
        );
        assert_eq!(parse_access_group_allow_from_entry("accessGroup:"), None);
        assert_eq!(parse_access_group_allow_from_entry("12345"), None);
    }

    #[test]
    fn dm_auth_expands_access_groups() {
        let allow = vec!["accessGroup:family".to_string()];
        let resolver = |name: &str| {
            if name == "family" {
                Some(vec!["111".to_string(), "222".to_string()])
            } else {
                None
            }
        };
        assert!(is_dm_sender_authorized_with("111", None, &allow, false, resolver));
        assert!(!is_dm_sender_authorized_with("333", None, &allow, false, resolver));
    }

    #[test]
    fn dm_auth_unresolvable_group_is_not_matched() {
        let allow = vec!["accessGroup:ghost".to_string()];
        assert!(!is_dm_sender_authorized_with("111", None, &allow, false, |_| None));
    }

    #[test]
    fn dm_auth_direct_and_wildcard() {
        let allow = vec!["999".to_string()];
        assert!(is_dm_sender_authorized_with("999", None, &allow, false, |_| None));
        let wildcard = vec!["*".to_string()];
        assert!(is_dm_sender_authorized_with("1", None, &wildcard, false, |_| None));
        // Name matching is opt-in (ID-only by default).
        let names = vec!["alice".to_string()];
        assert!(!is_dm_sender_authorized_with("1", Some("alice"), &names, false, |_| None));
        assert!(is_dm_sender_authorized_with("1", Some("Alice"), &names, true, |_| None));
    }

    // ---- gateway READY timeouts --------------------------------------------

    #[test]
    fn ready_timeout_defaults_and_overrides() {
        let cfg = DiscordAccountConfig::default();
        assert_eq!(resolve_gateway_ready_timeouts(&cfg), (15_000, 30_000));
        let cfg = DiscordAccountConfig {
            gateway_ready_timeout_ms: Some(5_000),
            gateway_runtime_ready_timeout_ms: Some(60_000),
            ..Default::default()
        };
        assert_eq!(resolve_gateway_ready_timeouts(&cfg), (5_000, 60_000));
    }

    #[test]
    fn ready_watch_times_out_and_connects() {
        let mut watch = GatewayReadyWatch::new(1_000);
        assert_eq!(watch.poll(false, 0), ReadyWatchState::Idle);
        watch.on_socket_open(100);
        assert_eq!(watch.poll(false, 500), ReadyWatchState::Waiting);
        assert_eq!(watch.poll(false, 1_100), ReadyWatchState::TimedOut);
        watch.on_socket_open(2_000);
        assert_eq!(watch.poll(true, 2_100), ReadyWatchState::Connected);
        assert!(watch.timeout_error().contains("1000ms"));
    }

    // ---- Components v2 text + forwarded snapshots --------------------------

    #[test]
    fn components_v2_text_display_extraction() {
        let components = json!([
            { "type": 17, "components": [
                { "type": 10, "content": "hello from v2" },
                { "type": 1, "components": [{ "type": 2, "label": "btn" }] },
                { "type": 10, "content": "second line" }
            ]}
        ]);
        assert_eq!(
            extract_components_v2_text(Some(&components)),
            "hello from v2\nsecond line"
        );
    }

    #[test]
    fn message_text_falls_back_to_component_text() {
        let message = json!({
            "content": "",
            "components": [{ "type": 10, "content": "component body" }]
        });
        assert_eq!(resolve_discord_message_text(&message, false), "component body");
    }

    #[test]
    fn forwarded_snapshot_blocks() {
        let message = json!({
            "content": "look at this",
            "message_snapshots": [
                { "message": {
                    "content": "original text",
                    "author": { "username": "alice" }
                }}
            ]
        });
        let text = resolve_discord_message_text(&message, true);
        assert_eq!(text, "look at this\n[Forwarded message from alice]\noriginal text");
    }

    #[test]
    fn forwarded_referenced_reply_when_snapshots_missing() {
        let message = json!({
            "content": "",
            "message_reference": { "type": 1, "message_id": "9" },
            "referenced_message": {
                "content": "",
                "components": [{ "type": 10, "content": "v2 referenced body" }],
                "author": { "global_name": "Bob" }
            }
        });
        let text = resolve_discord_message_text(&message, true);
        assert_eq!(text, "[Forwarded message from Bob]\nv2 referenced body");
    }

    #[test]
    fn snapshot_embed_fallback() {
        let snapshots = json!([
            { "message": { "embeds": [{ "title": "T", "description": "D" }] } }
        ]);
        assert_eq!(
            resolve_forwarded_snapshots_text(Some(&snapshots)),
            "[Forwarded message]\nT\nD"
        );
    }

    // ---- 429 cooldown bookkeeping ------------------------------------------

    #[test]
    fn rate_limit_book_learns_bucket_cooldowns() {
        let mut book = RateLimitBook::new();
        let obs = RateLimitObservation {
            status: 429,
            bucket: Some("abc".to_string()),
            retry_after_secs: Some(2.0),
            ..Default::default()
        };
        book.record("POST /channels/1/messages", &obs, 1_000);
        // Same route waits out the learned cooldown.
        assert_eq!(book.wait_ms("POST /channels/1/messages", 1_500), 1_500);
        // Sibling route bound to the same bucket shares the cooldown.
        let obs2 = RateLimitObservation {
            status: 200,
            bucket: Some("abc".to_string()),
            ..Default::default()
        };
        book.record("POST /channels/2/messages", &obs2, 1_600);
        assert!(book.wait_ms("POST /channels/2/messages", 1_700) > 0);
        // After reset, no wait.
        assert_eq!(book.wait_ms("POST /channels/1/messages", 3_100), 0);
    }

    #[test]
    fn rate_limit_book_global_cooldown() {
        let mut book = RateLimitBook::new();
        let obs = RateLimitObservation {
            status: 429,
            global: true,
            retry_after_secs: Some(5.0),
            ..Default::default()
        };
        book.record("GET /users/@me", &obs, 0);
        // Global cooldown gates every route.
        assert_eq!(book.wait_ms("POST /channels/9/messages", 1_000), 4_000);
        assert_eq!(book.global_wait_ms(6_000), 0);
    }

    #[test]
    fn rate_limit_headers_track_remaining() {
        let mut book = RateLimitBook::new();
        let obs = RateLimitObservation {
            status: 200,
            bucket: Some("b1".to_string()),
            limit: Some(5),
            remaining: Some(0),
            reset_after_secs: Some(1.0),
            ..Default::default()
        };
        book.record("PUT /channels/1/pins/2", &obs, 10_000);
        assert_eq!(book.wait_ms("PUT /channels/1/pins/2", 10_500), 500);
        assert_eq!(book.wait_ms("PUT /channels/1/pins/2", 11_100), 0);
    }

    #[test]
    fn retry_after_parsing() {
        assert_eq!(parse_retry_after_header_seconds("3", 0), Some(3.0));
        assert_eq!(parse_retry_after_header_seconds("  ", 0), None);
        assert_eq!(parse_retry_after_header_seconds("soon", 0), None);
        // HTTP-date form.
        let secs =
            parse_retry_after_header_seconds("Sun, 06 Nov 1994 08:49:37 GMT", 784_111_777_000)
                .unwrap();
        assert!((secs - 0.0).abs() < 120.0);
        // Body forms.
        assert_eq!(parse_retry_after_body_seconds(&json!(1.5)), Some(1.5));
        assert_eq!(parse_retry_after_body_seconds(&json!("2.25")), Some(2.25));
        assert_eq!(parse_retry_after_body_seconds(&json!("nope")), None);
        assert_eq!(parse_retry_after_body_seconds(&json!(-1)), None);
    }

    #[test]
    fn retryable_statuses() {
        assert!(is_retryable_discord_status(429));
        assert!(is_retryable_discord_status(408));
        assert!(is_retryable_discord_status(502));
        assert!(!is_retryable_discord_status(400));
        assert!(!is_retryable_discord_status(404));
    }

    #[test]
    fn queued_retry_delay_uses_learned_cooldown() {
        let retry = OutboundRetryConfig::default();
        let base = compute_queued_retry_delay_ms(1, &retry, 0);
        assert_eq!(base, retry.min_delay_ms);
        // Learned cooldown dominates short backoff.
        assert_eq!(compute_queued_retry_delay_ms(1, &retry, 9_000), 9_000);
        // Backoff is capped at max_delay_ms.
        assert_eq!(
            compute_queued_retry_delay_ms(30, &retry, 0),
            retry.max_delay_ms
        );
    }

    #[test]
    fn route_key_major_parameters() {
        assert_eq!(
            create_discord_route_key("post", "channels/123/messages/456"),
            "POST /channels/123/messages/:id"
        );
        assert_eq!(
            create_discord_route_key("GET", "guilds/9/members/8"),
            "GET /guilds/9/members/:id"
        );
    }

    // ---- mention aliases ----------------------------------------------------

    fn aliases(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn rewrites_known_alias() {
        let aliases = aliases(&[("dendi", "123456789")]);
        assert_eq!(
            rewrite_discord_known_mentions("hey @dendi hello", Some(&aliases)),
            "hey <@123456789> hello"
        );
    }

    #[test]
    fn unknown_alias_untouched() {
        let aliases = aliases(&[("dendi", "123456789")]);
        assert_eq!(
            rewrite_discord_known_mentions("hey @stranger", Some(&aliases)),
            "hey @stranger"
        );
    }

    #[test]
    fn reserved_mentions_never_rewritten() {
        let aliases = aliases(&[("everyone", "1"), ("here", "2")]);
        assert_eq!(
            rewrite_discord_known_mentions("@everyone and @here", Some(&aliases)),
            "@everyone and @here"
        );
    }

    #[test]
    fn alias_with_discriminator() {
        let aliases = aliases(&[("dendi#1234", "42")]);
        assert_eq!(
            rewrite_discord_known_mentions("cc @dendi", Some(&aliases)),
            "cc <@42>"
        );
        let aliases2 = aliases2_helper();
        assert_eq!(
            rewrite_discord_known_mentions("cc @dendi#1234", Some(&aliases2)),
            "cc <@42>"
        );
    }

    fn aliases2_helper() -> HashMap<String, String> {
        aliases(&[("dendi", "42")])
    }

    #[test]
    fn code_segments_skipped() {
        let aliases = aliases(&[("dendi", "42")]);
        let text = "ping @dendi then `@dendi` and\n```\n@dendi in fence\n```\n@dendi again";
        let out = rewrite_discord_known_mentions(text, Some(&aliases));
        assert_eq!(
            out,
            "ping <@42> then `@dendi` and\n```\n@dendi in fence\n```\n<@42> again"
        );
    }

    #[test]
    fn non_snowflake_alias_value_ignored() {
        let aliases = aliases(&[("dendi", "not-an-id")]);
        assert_eq!(
            rewrite_discord_known_mentions("hi @dendi", Some(&aliases)),
            "hi @dendi"
        );
    }

    #[test]
    fn targeted_and_broadcast_detection() {
        assert!(discord_text_has_targeted_mention("hello <@123>"));
        assert!(discord_text_has_targeted_mention("role <@&55>"));
        assert!(!discord_text_has_targeted_mention("channel <#55>"));
        assert!(discord_text_has_broadcast_mention("hi @everyone"));
        // `\b` boundary: `@hereafter` is not a broadcast mention.
        assert!(!discord_text_has_broadcast_mention("see you @hereafter"));
    }

    // ---- mention formatting + prompt hints ----------------------------------

    #[test]
    fn canonical_mention_formatting() {
        assert_eq!(format_user_mention("123").as_deref(), Some("<@123>"));
        assert_eq!(format_channel_mention("45").as_deref(), Some("<#45>"));
        assert_eq!(format_role_mention("6").as_deref(), Some("<@&6>"));
        assert_eq!(format_user_mention("abc"), None);
    }

    #[test]
    fn mention_prompt_hints_present() {
        let hints = discord_message_tool_hints();
        assert!(hints[0].contains("<@USER_ID>"));
        assert!(hints[0].contains("<#CHANNEL_ID>"));
        assert!(hints[0].contains("<@&ROLE_ID>"));
        assert!(hints[0].contains("mentionAliases"));
        assert_eq!(hints.len(), 3);
    }

    // ---- reaction listener gate ---------------------------------------------

    fn guild_entries(modes: &[Option<&str>]) -> HashMap<String, DiscordGuildEntry> {
        modes
            .iter()
            .enumerate()
            .map(|(i, mode)| {
                (
                    format!("g{}", i),
                    DiscordGuildEntry {
                        reaction_notifications: mode.map(|m| m.to_string()),
                        ..Default::default()
                    },
                )
            })
            .collect()
    }

    #[test]
    fn reaction_listeners_skipped_when_dms_off_and_all_guilds_off() {
        let entries = guild_entries(&[Some("off"), Some("off")]);
        assert!(!should_register_reaction_listeners(
            false,
            false,
            GroupPolicy::Open,
            Some(&entries)
        ));
    }

    #[test]
    fn reaction_listeners_registered_when_any_guild_on() {
        let entries = guild_entries(&[Some("off"), Some("own")]);
        assert!(should_register_reaction_listeners(
            false,
            false,
            GroupPolicy::Open,
            Some(&entries)
        ));
        // Unset mode defaults to "own" (on).
        let entries = guild_entries(&[None]);
        assert!(should_register_reaction_listeners(
            false,
            false,
            GroupPolicy::Open,
            Some(&entries)
        ));
    }

    #[test]
    fn reaction_listeners_dm_enabled_wins() {
        let entries = guild_entries(&[Some("off")]);
        assert!(should_register_reaction_listeners(
            true,
            false,
            GroupPolicy::Disabled,
            Some(&entries)
        ));
    }

    #[test]
    fn reaction_listeners_group_policy_disabled() {
        assert!(!should_register_reaction_listeners(
            false,
            false,
            GroupPolicy::Disabled,
            None
        ));
        // No guild entries + open policy → register.
        assert!(should_register_reaction_listeners(
            false,
            false,
            GroupPolicy::Open,
            None
        ));
    }

    // ---- typing prestart policy ---------------------------------------------

    #[test]
    fn typing_prestart_decisions() {
        let base = TypingPrestartParams {
            message_text: "hello",
            ..Default::default()
        };
        assert_eq!(
            resolve_accepted_typing_prestart(&base),
            (true, TypingPrestartReason::Direct)
        );
        let aborted = TypingPrestartParams { aborted: true, ..base.clone() };
        assert_eq!(
            resolve_accepted_typing_prestart(&aborted),
            (false, TypingPrestartReason::Aborted)
        );
        let empty = TypingPrestartParams { message_text: "  ", ..base.clone() };
        assert_eq!(
            resolve_accepted_typing_prestart(&empty),
            (false, TypingPrestartReason::Empty)
        );
        let configured = TypingPrestartParams {
            configured_typing_mode: Some("message"),
            ..base.clone()
        };
        assert_eq!(
            resolve_accepted_typing_prestart(&configured),
            (false, TypingPrestartReason::ConfiguredNotInstant)
        );
        let tool_only = TypingPrestartParams {
            is_guild_message: true,
            source_reply_tool_only: true,
            ..base.clone()
        };
        assert_eq!(
            resolve_accepted_typing_prestart(&tool_only),
            (true, TypingPrestartReason::ToolOnly)
        );
        let group_unmentioned = TypingPrestartParams {
            is_guild_message: true,
            ..base.clone()
        };
        assert_eq!(
            resolve_accepted_typing_prestart(&group_unmentioned),
            (false, TypingPrestartReason::DeferToMessage)
        );
        let group_mentioned = TypingPrestartParams {
            is_guild_message: true,
            was_mentioned: true,
            ..base
        };
        assert_eq!(
            resolve_accepted_typing_prestart(&group_mentioned),
            (true, TypingPrestartReason::MentionedGroup)
        );
    }

    // ---- PluralKit dedupe + thread starter ----------------------------------

    #[test]
    fn canonical_message_id_prefers_pluralkit_original() {
        assert_eq!(resolve_canonical_message_id("proxy", Some("orig")), "orig");
        assert_eq!(resolve_canonical_message_id("proxy", Some("  ")), "proxy");
        assert_eq!(resolve_canonical_message_id("proxy", None), "proxy");
    }

    #[test]
    fn replay_key_shape() {
        assert_eq!(
            build_inbound_replay_key("acct", "chan", "msg").as_deref(),
            Some("acct:chan:msg")
        );
        assert_eq!(build_inbound_replay_key("acct", "", "msg"), None);
        assert_eq!(build_inbound_replay_key("acct", "chan", " "), None);
    }

    #[test]
    fn replay_guard_dedupes_by_canonical_id() {
        let guard = InboundReplayGuard::new();
        let original_key =
            build_inbound_replay_key("a", "c", resolve_canonical_message_id("proxy1", Some("orig")))
                .unwrap();
        assert!(guard.claim(&original_key, 0));
        // The PluralKit proxied copy resolves to the same canonical key.
        let proxied_key =
            build_inbound_replay_key("a", "c", resolve_canonical_message_id("proxy2", Some("orig")))
                .unwrap();
        assert!(!guard.claim(&proxied_key, 100));
        // Release (processing failed) allows a retry; commit keeps it deduped.
        guard.release(&original_key);
        assert!(guard.claim(&original_key, 200));
        guard.commit(&original_key, 200);
        guard.release(&original_key);
        assert!(!guard.claim(&original_key, 300));
        // TTL expiry frees the key.
        assert!(guard.claim("other", REPLAY_GUARD_TTL_MS * 2));
    }

    #[test]
    fn thread_starter_only_on_first_turn() {
        assert!(should_include_thread_starter(None, false));
        assert!(!should_include_thread_starter(None, true));
        assert!(!should_include_thread_starter(Some(false), false));
        assert!(should_include_thread_starter(Some(true), false));
        assert!(!should_include_thread_starter(Some(true), true));
    }

    // ---- voice ---------------------------------------------------------------

    #[test]
    fn voice_defaults() {
        assert!(voice_enabled(None));
        assert_eq!(resolve_voice_mode(None), "agent-proxy");
        assert_eq!(resolve_voice_model_override(None), None);
        let voice = DiscordVoiceConfig {
            enabled: Some(false),
            mode: Some("stt-tts".to_string()),
            model: Some("anthropic/claude-opus-4-5".to_string()),
            ..Default::default()
        };
        assert!(!voice_enabled(Some(&voice)));
        assert_eq!(resolve_voice_mode(Some(&voice)), "stt-tts");
        assert_eq!(
            resolve_voice_model_override(Some(&voice)),
            Some("anthropic/claude-opus-4-5")
        );
    }

    #[test]
    fn voice_hides_tts_tool() {
        assert!(is_tool_hidden_for_voice("tts"));
        assert!(is_tool_hidden_for_voice("TTS"));
        assert!(!is_tool_hidden_for_voice("web_search"));
    }

    #[test]
    fn voice_ingress_prompt_format() {
        let prompt = format_voice_ingress_prompt(" hello there ", Some("Dendi"));
        assert!(prompt.starts_with(DISCORD_VOICE_SPOKEN_OUTPUT_CONTRACT));
        assert!(prompt.ends_with("Voice transcript from speaker \"Dendi\":\nhello there"));
        assert!(prompt.contains("Do not call the tts tool"));
        let unlabeled = format_voice_ingress_prompt("hi", None);
        assert!(unlabeled.ends_with("\n\nhi"));
    }

    #[test]
    fn channel_system_prompt_override() {
        assert_eq!(resolve_channel_system_prompt(None), None);
        let cfg = DiscordGuildChannelConfig {
            system_prompt: Some("  be terse  ".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve_channel_system_prompt(Some(&cfg)).as_deref(),
            Some("be terse")
        );
        let empty = DiscordGuildChannelConfig {
            system_prompt: Some("   ".to_string()),
            ..Default::default()
        };
        assert_eq!(resolve_channel_system_prompt(Some(&empty)), None);
    }

    // ---- text command parsing + arg dialogs ----------------------------------

    #[test]
    fn parses_text_commands() {
        assert_eq!(
            parse_discord_text_command("/help"),
            Some(ParsedTextCommand {
                name: "help".to_string(),
                args_raw: None
            })
        );
        assert_eq!(
            parse_discord_text_command("  /model gpt-4.1  "),
            Some(ParsedTextCommand {
                name: "model".to_string(),
                args_raw: Some("gpt-4.1".to_string())
            })
        );
        // Leading bot mention tolerated.
        assert_eq!(
            parse_discord_text_command("<@12345> /steer main"),
            Some(ParsedTextCommand {
                name: "steer".to_string(),
                args_raw: Some("main".to_string())
            })
        );
        // Not commands.
        assert_eq!(parse_discord_text_command("/usr/bin/env"), None);
        assert_eq!(parse_discord_text_command("hello /help"), None);
        assert_eq!(parse_discord_text_command("//nope"), None);
    }

    #[test]
    fn command_arg_custom_id_round_trip() {
        let id = build_command_arg_custom_id("model", "name", "openai/gpt-4.1", "77");
        assert!(id.starts_with("cmdarg:command=model;arg=name;value=openai%2Fgpt-4.1;user=77"));
        let (command, arg, value, user) = parse_command_arg_custom_id(&id).unwrap();
        assert_eq!(command, "model");
        assert_eq!(arg, "name");
        assert_eq!(value, "openai/gpt-4.1");
        assert_eq!(user, "77");
    }

    #[test]
    fn command_arg_menu_chunks_and_titles() {
        let choices: Vec<CommandArgChoice> = (0..6)
            .map(|i| CommandArgChoice {
                value: format!("v{}", i),
                label: format!("L{}", i),
            })
            .collect();
        let menu = build_command_arg_menu("model", "name", None, None, &choices, "9");
        assert_eq!(menu.content, "Choose name for /model.");
        assert_eq!(menu.rows.len(), 2);
        assert_eq!(menu.rows[0].len(), 4);
        assert_eq!(menu.rows[1].len(), 2);
        assert_eq!(menu.rows[0][0].label, "L0");
        // Description and explicit title variants.
        let with_desc =
            build_command_arg_menu("model", "name", Some("the model"), None, &choices, "9");
        assert_eq!(with_desc.content, "Choose the model for /model.");
        let with_title =
            build_command_arg_menu("model", "name", None, Some("Pick one"), &choices, "9");
        assert_eq!(with_title.content, "Pick one");
    }

    // ---- rate limit observation from headers ---------------------------------

    #[test]
    fn observes_rate_limit_headers_and_body() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("X-RateLimit-Bucket", "hash1".parse().unwrap());
        headers.insert("X-RateLimit-Limit", "5".parse().unwrap());
        headers.insert("X-RateLimit-Remaining", "0".parse().unwrap());
        headers.insert("X-RateLimit-Reset-After", "1.5".parse().unwrap());
        headers.insert("X-RateLimit-Global", "true".parse().unwrap());
        let body = json!({ "retry_after": 2.5, "global": false });
        let obs = observe_rate_limit_response(429, &headers, Some(&body), 0);
        assert_eq!(obs.status, 429);
        assert_eq!(obs.bucket.as_deref(), Some("hash1"));
        assert_eq!(obs.limit, Some(5));
        assert_eq!(obs.remaining, Some(0));
        assert_eq!(obs.reset_after_secs, Some(1.5));
        assert_eq!(obs.retry_after_secs, Some(2.5));
        assert!(obs.global);
    }
}
