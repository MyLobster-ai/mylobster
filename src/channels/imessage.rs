use crate::config::Config;
use crate::gateway::GatewayState;

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};

use anyhow::Result;
use async_trait::async_trait;
use tracing::info;

/// iMessage channel stub.
///
/// iMessage integration requires a macOS host with an iMessage bridge
/// (e.g. BlueBubbles or Beeper).  This stub registers the channel in the
/// plugin system but does not yet implement the full protocol.
pub struct IMessageChannel {
    enabled: bool,
}

impl IMessageChannel {
    pub fn new(config: &Config) -> Self {
        let enabled = config.channels.imessage.enabled.unwrap_or(false);

        Self { enabled }
    }
}

#[async_trait]
impl ChannelPlugin for IMessageChannel {
    fn id(&self) -> &str {
        "imessage"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "iMessage".to_string(),
            description: "Apple iMessage channel (stub)".to_string(),
            enabled: self.enabled,
            multi_account: false,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![
            ChannelCapability::SendText,
            ChannelCapability::ReceiveText,
            ChannelCapability::SendMedia,
            ChannelCapability::ReceiveMedia,
            ChannelCapability::Groups,
            ChannelCapability::ReadReceipts,
            ChannelCapability::TypingIndicators,
            ChannelCapability::Reactions,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        info!("iMessage channel starting (stub)");
        // TODO: Connect to BlueBubbles / Beeper REST API.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.enabled {
            info!("iMessage channel stopping (stub)");
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, _message: &str) -> Result<()> {
        info!(
            to = to,
            "iMessage: sending message (stub -- not implemented)"
        );
        // TODO: Forward to iMessage bridge API.
        Ok(())
    }
}

/// Convenience function called by the top-level `send_message` dispatcher.
pub(crate) async fn send_message(config: &Config, to: &str, message: &str) -> Result<()> {
    let channel = IMessageChannel::new(config);
    channel.send_message(to, message).await
}
