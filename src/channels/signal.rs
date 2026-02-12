use crate::config::Config;
use crate::gateway::GatewayState;

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};

use anyhow::Result;
use async_trait::async_trait;
use tracing::info;

/// Signal channel stub.
///
/// Signal integration requires a signal-cli REST API instance.  This stub
/// registers the channel in the plugin system but does not yet implement the
/// full protocol.
pub struct SignalChannel {
    enabled: bool,
}

impl SignalChannel {
    pub fn new(config: &Config) -> Self {
        let enabled = config.channels.signal.enabled.unwrap_or(false);

        Self { enabled }
    }
}

#[async_trait]
impl ChannelPlugin for SignalChannel {
    fn id(&self) -> &str {
        "signal"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Signal".to_string(),
            description: "Signal Messenger channel (stub)".to_string(),
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
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        info!("Signal channel starting (stub)");
        // TODO: Connect to signal-cli REST API.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.enabled {
            info!("Signal channel stopping (stub)");
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, _message: &str) -> Result<()> {
        info!(to = to, "Signal: sending message (stub -- not implemented)");
        // TODO: Forward to signal-cli REST API /v2/send endpoint.
        Ok(())
    }
}

/// Convenience function called by the top-level `send_message` dispatcher.
pub(crate) async fn send_message(config: &Config, to: &str, message: &str) -> Result<()> {
    let channel = SignalChannel::new(config);
    channel.send_message(to, message).await
}
