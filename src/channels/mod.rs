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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

// ============================================================================
// Typing Keepalive Loop (v2026.2.24)
// ============================================================================

/// Manages a periodic "typing…" indicator for a channel during long-running
/// operations. The loop fires a callback at a fixed interval until stopped.
///
/// Reference: OC `src/channels/typing-lifecycle.ts`.
pub struct TypingKeepaliveLoop {
    interval_ms: u64,
    running: Arc<AtomicBool>,
}

impl TypingKeepaliveLoop {
    /// Create a new keepalive loop with the given interval.
    pub fn new(interval_ms: u64) -> Self {
        Self {
            interval_ms,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Whether the loop is currently running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Start the keepalive loop, invoking `on_tick` at each interval.
    /// Returns a handle that can be used to stop the loop.
    pub fn start<F>(&self, on_tick: F) -> tokio::task::JoinHandle<()>
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.running.store(true, Ordering::Relaxed);
        let running = self.running.clone();
        let interval = self.interval_ms;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(interval));
            // Skip the first immediate tick.
            ticker.tick().await;
            while running.load(Ordering::Relaxed) {
                ticker.tick().await;
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                on_tick();
            }
        })
    }

    /// Stop the keepalive loop.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

// ============================================================================
// Active Run Queue Policy (v2026.2.24)
// ============================================================================

/// Action to take when a new message arrives while a run is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveRunQueueAction {
    /// Execute immediately (no active run, or queue mode allows it).
    RunNow,
    /// Queue a follow-up run after the current one finishes.
    EnqueueFollowup,
    /// Drop the message (heartbeats during active runs).
    Drop,
}

/// Determine what to do with an incoming message when a run is already active.
///
/// Heartbeats are always dropped during active runs.
///
/// Reference: OC `src/auto-reply/reply/queue-policy.ts`.
pub fn resolve_active_run_queue_action(
    is_active: bool,
    is_heartbeat: bool,
    should_followup: bool,
    _queue_mode: &str,
) -> ActiveRunQueueAction {
    if !is_active {
        return ActiveRunQueueAction::RunNow;
    }

    // Heartbeats always drop during active runs.
    if is_heartbeat {
        return ActiveRunQueueAction::Drop;
    }

    if should_followup {
        ActiveRunQueueAction::EnqueueFollowup
    } else {
        ActiveRunQueueAction::Drop
    }
}

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
