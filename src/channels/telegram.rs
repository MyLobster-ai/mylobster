use crate::config::Config;
use crate::gateway::GatewayState;
use crate::infra::dm_policy;

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

// ============================================================================
// v2026.2.26: Inline Keyboard Support
// ============================================================================

/// Represents an inline keyboard button for Telegram groups.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineKeyboardButton {
    /// Button label text.
    pub text: String,
    /// Callback data sent when button is pressed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_data: Option<String>,
    /// URL to open when button is pressed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Represents an inline keyboard markup for Telegram messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineKeyboardMarkup {
    /// Rows of inline keyboard buttons.
    pub inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

impl InlineKeyboardMarkup {
    /// Create a single-row keyboard with the given buttons.
    pub fn single_row(buttons: Vec<InlineKeyboardButton>) -> Self {
        Self {
            inline_keyboard: vec![buttons],
        }
    }

    /// Create a keyboard with one button per row.
    pub fn column(buttons: Vec<InlineKeyboardButton>) -> Self {
        Self {
            inline_keyboard: buttons.into_iter().map(|b| vec![b]).collect(),
        }
    }
}

// ============================================================================
// v2026.2.26: Native Command Registration
// ============================================================================

/// A Telegram bot command that can be registered with BotFather.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotCommand {
    /// Command name (without leading /).
    pub command: String,
    /// Command description shown in the command menu.
    pub description: String,
}

/// Result of a command registration attempt.
#[derive(Debug)]
pub struct CommandRegistrationResult {
    pub success: bool,
    pub registered_count: usize,
    pub error: Option<String>,
}

// ============================================================================
// v2026.2.26: Streaming Preview Finalization
// ============================================================================

/// Tracks the state of a streaming preview message for finalization.
#[derive(Debug)]
pub struct StreamingPreview {
    /// Telegram chat_id where the preview message was sent.
    pub chat_id: i64,
    /// message_id of the preview message being updated.
    pub message_id: i64,
    /// Whether the final edit has been applied.
    pub finalized: bool,
    /// Accumulated text content.
    pub content: String,
}

impl StreamingPreview {
    pub fn new(chat_id: i64, message_id: i64) -> Self {
        Self {
            chat_id,
            message_id,
            finalized: false,
            content: String::new(),
        }
    }

    /// Mark this preview as finalized. After this, no more edits will be sent.
    pub fn finalize(&mut self) {
        self.finalized = true;
    }

    /// Whether more edits should be sent.
    pub fn should_edit(&self) -> bool {
        !self.finalized
    }
}

// ============================================================================
// Telegram Channel Implementation
// ============================================================================

/// Telegram channel implementation using the Bot API via teloxide.
pub struct TelegramChannel {
    enabled: bool,
    bot_token: Option<String>,
    /// v2026.2.26: DM allowlist from config.
    dm_allow_from: Vec<String>,
}

impl TelegramChannel {
    pub fn new(config: &Config) -> Self {
        let tg = &config.channels.telegram;
        let bot_token = tg.default_account.bot_token.clone();
        let enabled = tg.default_account.enabled.unwrap_or(bot_token.is_some());

        // v2026.2.26: Load DM allowlist (does NOT inherit from parent)
        let dm_allow_from = tg
            .default_account
            .allow_from
            .clone()
            .unwrap_or_default();

        Self {
            enabled,
            bot_token,
            dm_allow_from,
        }
    }

    /// v2026.2.26: Check if a Telegram user is allowed to DM the bot.
    pub fn is_dm_allowed(&self, user_id: &str) -> bool {
        if self.dm_allow_from.is_empty() {
            // No allowlist = allow all DMs
            return true;
        }
        dm_policy::is_source_allowed(&self.dm_allow_from, user_id)
    }

    /// v2026.2.26: Register bot commands with Telegram API.
    ///
    /// Gracefully degrades if the API call fails (e.g., due to rate limits
    /// or permissions). Commands are still usable even without registration.
    pub async fn register_commands(
        &self,
        commands: &[BotCommand],
    ) -> CommandRegistrationResult {
        let token = match &self.bot_token {
            Some(t) => t,
            None => {
                return CommandRegistrationResult {
                    success: false,
                    registered_count: 0,
                    error: Some("No bot token configured".to_string()),
                };
            }
        };

        let url = format!(
            "https://api.telegram.org/bot{}/setMyCommands",
            token
        );

        let body = serde_json::json!({
            "commands": commands
        });

        let client = reqwest::Client::new();
        match client.post(&url).json(&body).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    info!("Registered {} Telegram bot commands", commands.len());
                    CommandRegistrationResult {
                        success: true,
                        registered_count: commands.len(),
                        error: None,
                    }
                } else {
                    // v2026.2.26: Graceful degradation — log warning but don't fail
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    warn!(
                        "Telegram command registration failed ({}): {}",
                        status, text
                    );
                    CommandRegistrationResult {
                        success: false,
                        registered_count: 0,
                        error: Some(format!("API error {}: {}", status, text)),
                    }
                }
            }
            Err(e) => {
                // v2026.2.26: Graceful degradation — network error
                warn!("Telegram command registration error: {}", e);
                CommandRegistrationResult {
                    success: false,
                    registered_count: 0,
                    error: Some(format!("Network error: {}", e)),
                }
            }
        }
    }

    /// v2026.2.26: Send a message with inline keyboard buttons (for groups).
    pub async fn send_message_with_buttons(
        &self,
        chat_id: &str,
        message: &str,
        keyboard: &InlineKeyboardMarkup,
    ) -> Result<()> {
        let token = self
            .bot_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Telegram bot token not configured"))?;

        let url = format!("https://api.telegram.org/bot{}/sendMessage", token);

        let body = serde_json::json!({
            "chat_id": chat_id,
            "text": message,
            "reply_markup": keyboard,
        });

        let client = reqwest::Client::new();
        let resp = client.post(&url).json(&body).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Telegram sendMessage failed ({}): {}", status, text);
        }

        Ok(())
    }
}

#[async_trait]
impl ChannelPlugin for TelegramChannel {
    fn id(&self) -> &str {
        "telegram"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Telegram".to_string(),
            description: "Telegram Bot API channel".to_string(),
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
                warn!("Telegram channel enabled but no bot token configured");
                return Ok(());
            }
        };

        info!(
            "Telegram channel starting (token ends ...{})",
            &token[token.len().saturating_sub(4)..]
        );

        // TODO: Initialise teloxide bot dispatcher and start polling / webhook.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.enabled {
            info!("Telegram channel stopping");
            // TODO: Signal the teloxide dispatcher to shut down.
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, _message: &str) -> Result<()> {
        let _token = self
            .bot_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Telegram bot token not configured"))?;

        let _chat_id: i64 = to
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid Telegram chat_id: {to}"))?;

        info!(chat_id = to, "Telegram: sending message");

        // TODO: Use teloxide to call sendMessage with (_token, _chat_id, _message).

        Ok(())
    }
}

/// Convenience function called by the top-level `send_message` dispatcher.
pub(crate) async fn send_message(config: &Config, to: &str, message: &str) -> Result<()> {
    let channel = TelegramChannel::new(config);
    channel.send_message(to, message).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_keyboard_single_row() {
        let buttons = vec![
            InlineKeyboardButton {
                text: "Yes".to_string(),
                callback_data: Some("yes".to_string()),
                url: None,
            },
            InlineKeyboardButton {
                text: "No".to_string(),
                callback_data: Some("no".to_string()),
                url: None,
            },
        ];
        let kb = InlineKeyboardMarkup::single_row(buttons);
        assert_eq!(kb.inline_keyboard.len(), 1);
        assert_eq!(kb.inline_keyboard[0].len(), 2);
    }

    #[test]
    fn inline_keyboard_column() {
        let buttons = vec![
            InlineKeyboardButton {
                text: "Option A".to_string(),
                callback_data: Some("a".to_string()),
                url: None,
            },
            InlineKeyboardButton {
                text: "Option B".to_string(),
                callback_data: Some("b".to_string()),
                url: None,
            },
        ];
        let kb = InlineKeyboardMarkup::column(buttons);
        assert_eq!(kb.inline_keyboard.len(), 2);
        assert_eq!(kb.inline_keyboard[0].len(), 1);
        assert_eq!(kb.inline_keyboard[1].len(), 1);
    }

    #[test]
    fn inline_keyboard_serialization() {
        let kb = InlineKeyboardMarkup::single_row(vec![InlineKeyboardButton {
            text: "Click".to_string(),
            callback_data: Some("data".to_string()),
            url: None,
        }]);

        let json = serde_json::to_value(&kb).unwrap();
        assert!(json["inline_keyboard"].is_array());
        assert_eq!(json["inline_keyboard"][0][0]["text"], "Click");
    }

    #[test]
    fn streaming_preview_lifecycle() {
        let mut preview = StreamingPreview::new(12345, 67890);
        assert!(preview.should_edit());
        assert!(!preview.finalized);

        preview.content.push_str("Hello ");
        preview.content.push_str("World");
        assert_eq!(preview.content, "Hello World");

        preview.finalize();
        assert!(!preview.should_edit());
        assert!(preview.finalized);
    }

    #[test]
    fn dm_allowlist_empty_allows_all() {
        let config = Config::default();
        let channel = TelegramChannel::new(&config);
        assert!(channel.is_dm_allowed("any_user"));
    }

    #[test]
    fn bot_command_serialization() {
        let cmd = BotCommand {
            command: "help".to_string(),
            description: "Show help message".to_string(),
        };
        let json = serde_json::to_value(&cmd).unwrap();
        assert_eq!(json["command"], "help");
        assert_eq!(json["description"], "Show help message");
    }
}
