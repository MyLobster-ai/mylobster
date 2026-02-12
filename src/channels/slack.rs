use crate::config::Config;
use crate::gateway::GatewayState;

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};

use anyhow::Result;
use async_trait::async_trait;
use tracing::{info, warn};

/// Slack channel implementation using slack-morphism.
pub struct SlackChannel {
    enabled: bool,
    bot_token: Option<String>,
    app_token: Option<String>,
}

impl SlackChannel {
    pub fn new(config: &Config) -> Self {
        let sl = &config.channels.slack;
        let bot_token = sl.default_account.bot_token.clone();
        let app_token = sl.default_account.app_token.clone();
        let enabled = sl
            .default_account
            .enabled
            .unwrap_or(bot_token.is_some());

        Self {
            enabled,
            bot_token,
            app_token,
        }
    }
}

#[async_trait]
impl ChannelPlugin for SlackChannel {
    fn id(&self) -> &str {
        "slack"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Slack".to_string(),
            description: "Slack Bot channel via Socket Mode or Events API".to_string(),
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
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let _bot_token = match &self.bot_token {
            Some(t) => t,
            None => {
                warn!("Slack channel enabled but no bot token configured");
                return Ok(());
            }
        };

        info!("Slack channel starting");

        // TODO: Initialise slack-morphism client.
        // If app_token is present, use Socket Mode for real-time events.
        // Otherwise, register an Events API webhook endpoint on the gateway.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.enabled {
            info!("Slack channel stopping");
            // TODO: Close Socket Mode connection or deregister webhook.
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, _message: &str) -> Result<()> {
        let _bot_token = self
            .bot_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Slack bot token not configured"))?;

        info!(channel = to, "Slack: sending message");

        // TODO: Use slack-morphism to call chat.postMessage with (to, _message).

        Ok(())
    }
}

/// Convenience function called by the top-level `send_message` dispatcher.
pub(crate) async fn send_message(config: &Config, to: &str, message: &str) -> Result<()> {
    let channel = SlackChannel::new(config);
    channel.send_message(to, message).await
}
