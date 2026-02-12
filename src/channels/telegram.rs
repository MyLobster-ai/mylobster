use crate::config::Config;
use crate::gateway::GatewayState;

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};

use anyhow::Result;
use async_trait::async_trait;
use tracing::{info, warn};

/// Telegram channel implementation using the Bot API via teloxide.
pub struct TelegramChannel {
    enabled: bool,
    bot_token: Option<String>,
}

impl TelegramChannel {
    pub fn new(config: &Config) -> Self {
        let tg = &config.channels.telegram;
        let bot_token = tg.default_account.bot_token.clone();
        let enabled = tg.default_account.enabled.unwrap_or(bot_token.is_some());

        Self { enabled, bot_token }
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
        // The actual implementation will:
        // 1. Create a teloxide::Bot with the token.
        // 2. Set up a message handler that forwards incoming messages to the
        //    gateway session system.
        // 3. Start long-polling or register a webhook.

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
