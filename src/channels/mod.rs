mod discord;
mod imessage;
mod normalize;
mod plugin;
mod signal;
mod slack;
mod synology_chat;
mod telegram;
mod whatsapp;

pub use plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};

use crate::config::Config;
use crate::gateway::GatewayState;

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

// Re-export the send_message convenience function.
pub use self::send::send_message;

mod send {
    use crate::config::Config;
    use anyhow::{bail, Result};

    /// Send a message through a specific channel.
    ///
    /// This is a convenience wrapper that dispatches to the appropriate channel
    /// implementation based on the `channel` argument (e.g. "telegram", "discord").
    pub async fn send_message(
        config: &Config,
        channel: &str,
        to: &str,
        message: &str,
    ) -> Result<()> {
        match channel {
            "telegram" => super::telegram::send_message(config, to, message).await,
            "discord" => super::discord::send_message(config, to, message).await,
            "slack" => super::slack::send_message(config, to, message).await,
            "whatsapp" => super::whatsapp::send_message(config, to, message).await,
            "signal" => super::signal::send_message(config, to, message).await,
            "imessage" => super::imessage::send_message(config, to, message).await,
            "synology_chat" => super::synology_chat::send_message(config, to, message).await,
            other => bail!("unknown channel: {other}"),
        }
    }
}

/// Manages all channel instances and their lifecycle.
pub struct ChannelManager {
    /// Registered channel plugins keyed by channel id (e.g. "telegram", "discord").
    plugins: RwLock<HashMap<String, Arc<dyn ChannelPlugin>>>,
    /// Snapshot of channel configuration at construction time.
    config: Config,
}

impl ChannelManager {
    /// Create a new `ChannelManager` from the provided configuration.
    ///
    /// Channel plugins are registered but not started until [`start_all`] is called.
    pub fn new(config: &Config) -> Self {
        let mut plugins: HashMap<String, Arc<dyn ChannelPlugin>> = HashMap::new();

        // Register built-in channel plugins.
        plugins.insert(
            "telegram".to_string(),
            Arc::new(telegram::TelegramChannel::new(config)),
        );
        plugins.insert(
            "discord".to_string(),
            Arc::new(discord::DiscordChannel::new(config)),
        );
        plugins.insert(
            "slack".to_string(),
            Arc::new(slack::SlackChannel::new(config)),
        );
        plugins.insert(
            "whatsapp".to_string(),
            Arc::new(whatsapp::WhatsAppChannel::new(config)),
        );
        plugins.insert(
            "signal".to_string(),
            Arc::new(signal::SignalChannel::new(config)),
        );
        plugins.insert(
            "imessage".to_string(),
            Arc::new(imessage::IMessageChannel::new(config)),
        );
        plugins.insert(
            "synology_chat".to_string(),
            Arc::new(synology_chat::SynologyChatChannel::new(config)),
        );

        Self {
            plugins: RwLock::new(plugins),
            config: config.clone(),
        }
    }

    /// Start all registered channel plugins that are enabled.
    ///
    /// Each plugin's `start_account` method is invoked. Plugins that fail to
    /// start are logged but do not prevent other channels from starting.
    pub async fn start_all(&self, state: &GatewayState) -> Result<()> {
        let plugins = self.plugins.read().await;
        for (id, plugin) in plugins.iter() {
            let meta = plugin.meta();
            if !meta.enabled {
                info!(channel = %id, "Channel disabled, skipping");
                continue;
            }
            info!(channel = %id, "Starting channel");
            if let Err(e) = plugin.start_account(state).await {
                warn!(channel = %id, error = %e, "Failed to start channel");
            }
        }
        Ok(())
    }

    /// Stop all running channel plugins.
    pub async fn stop_all(&self) -> Result<()> {
        let plugins = self.plugins.read().await;
        for (id, plugin) in plugins.iter() {
            info!(channel = %id, "Stopping channel");
            if let Err(e) = plugin.stop_account().await {
                warn!(channel = %id, error = %e, "Failed to stop channel");
            }
        }
        Ok(())
    }

    /// Return a JSON status summary of all channels.
    pub async fn get_status(&self) -> serde_json::Value {
        let plugins = self.plugins.read().await;
        let mut status = serde_json::Map::new();

        for (id, plugin) in plugins.iter() {
            let meta = plugin.meta();
            let capabilities: Vec<String> = plugin
                .capabilities()
                .iter()
                .map(|c| format!("{c:?}"))
                .collect();

            status.insert(
                id.clone(),
                serde_json::json!({
                    "name": meta.name,
                    "enabled": meta.enabled,
                    "capabilities": capabilities,
                }),
            );
        }

        serde_json::Value::Object(status)
    }

    /// Look up a channel plugin by id.
    pub async fn get_plugin(&self, id: &str) -> Option<Arc<dyn ChannelPlugin>> {
        self.plugins.read().await.get(id).cloned()
    }
}
