use crate::config::Config;
use crate::gateway::GatewayState;

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};

use anyhow::Result;
use async_trait::async_trait;
use tracing::{info, warn};

/// Discord channel implementation using serenity.
pub struct DiscordChannel {
    enabled: bool,
    bot_token: Option<String>,
}

impl DiscordChannel {
    pub fn new(config: &Config) -> Self {
        let dc = &config.channels.discord;
        let bot_token = dc.default_account.token.clone();
        let enabled = dc.default_account.enabled.unwrap_or(bot_token.is_some());

        Self { enabled, bot_token }
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
        // The handler should forward incoming messages to the gateway session
        // system, respecting guild / channel / DM configuration.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.enabled {
            info!("Discord channel stopping");
            // TODO: Shut down the serenity client.
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, _message: &str) -> Result<()> {
        let _token = self
            .bot_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Discord bot token not configured"))?;

        let _channel_id: u64 = to
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid Discord channel_id: {to}"))?;

        info!(channel_id = to, "Discord: sending message");

        // TODO: Use serenity HTTP client to send a message to _channel_id.

        Ok(())
    }
}

/// Convenience function called by the top-level `send_message` dispatcher.
pub(crate) async fn send_message(config: &Config, to: &str, message: &str) -> Result<()> {
    let channel = DiscordChannel::new(config);
    channel.send_message(to, message).await
}
