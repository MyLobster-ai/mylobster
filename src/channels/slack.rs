use crate::config::Config;
use crate::gateway::GatewayState;

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};

use anyhow::Result;
use async_trait::async_trait;
use tracing::{debug, info, warn};

// ============================================================================
// v2026.2.26: NO_REPLY Suppression
// ============================================================================

/// Sentinel value indicating the agent chose not to reply.
///
/// When the agent returns this exact string as a response, the Slack channel
/// suppresses the API call — no message is sent and no error is raised.
/// This prevents empty or unwanted messages from being posted to Slack.
pub const NO_REPLY_SENTINEL: &str = "NO_REPLY";

/// Check if a message should be suppressed (not sent to Slack).
pub fn should_suppress_message(message: &str) -> bool {
    let trimmed = message.trim();
    trimmed == NO_REPLY_SENTINEL || trimmed.is_empty()
}

// ============================================================================
// Slack Channel Implementation
// ============================================================================

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
        let enabled = sl.default_account.enabled.unwrap_or(bot_token.is_some());

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

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        // v2026.2.26: Suppress NO_REPLY before making API call.
        if should_suppress_message(message) {
            debug!(channel = to, "Slack: suppressing NO_REPLY message");
            return Ok(());
        }

        let _bot_token = self
            .bot_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Slack bot token not configured"))?;

        info!(channel = to, "Slack: sending message");

        // TODO: Use slack-morphism to call chat.postMessage with (to, message).

        Ok(())
    }
}

/// Convenience function called by the top-level `send_message` dispatcher.
pub(crate) async fn send_message(config: &Config, to: &str, message: &str) -> Result<()> {
    let channel = SlackChannel::new(config);
    channel.send_message(to, message).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_reply_sentinel_suppressed() {
        assert!(should_suppress_message("NO_REPLY"));
        assert!(should_suppress_message("  NO_REPLY  "));
        assert!(should_suppress_message("NO_REPLY\n"));
    }

    #[test]
    fn empty_message_suppressed() {
        assert!(should_suppress_message(""));
        assert!(should_suppress_message("   "));
        assert!(should_suppress_message("\n\t"));
    }

    #[test]
    fn normal_message_not_suppressed() {
        assert!(!should_suppress_message("Hello world"));
        assert!(!should_suppress_message("NO_REPLY extra text"));
        assert!(!should_suppress_message("no_reply")); // case-sensitive
    }
}
