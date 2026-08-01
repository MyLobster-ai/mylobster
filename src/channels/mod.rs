pub mod bluebubbles;
pub mod control_commands;
pub mod discord;
pub mod discord_chunk;
pub mod discord_routing;
pub mod discord_status;
pub mod discord_transport;
pub mod discord_voice;
pub mod feishu;
pub mod google_meet;
pub mod googlechat;
pub mod imessage;
pub mod irc;
pub mod line;
pub mod loop_guard;
pub mod matrix;
pub mod mattermost;
pub mod nextcloud;
pub mod normalize;
pub mod nostr;
mod plugin;
pub mod progress_draft;
pub mod qqbot;
pub mod signal;
pub mod slack;
pub mod sms;
pub mod status_reactions;
pub mod synology_chat;
pub mod teams;
pub mod telegram;
pub mod telegram_commands;
pub mod telegram_dispatcher;
pub mod telegram_format;
pub mod telegram_net;
pub mod telegram_pairing;
pub mod telegram_progress;
pub mod telegram_spool;
pub mod telegram_targets;
mod tlon;
pub mod twitch;
pub mod voice_call;
mod webchat;
pub mod whatsapp;
pub mod yuanbao;
pub mod zalo;
pub mod zalouser;

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
    /// Maximum duration before auto-stop (v2026.2.26 safety net).
    max_duration_ms: u64,
    running: Arc<AtomicBool>,
    /// Suppressed during tool execution (v2026.2.26).
    suppressed: Arc<AtomicBool>,
}

/// Default maximum typing indicator duration: 120 seconds.
const DEFAULT_MAX_TYPING_DURATION_MS: u64 = 120_000;

impl TypingKeepaliveLoop {
    /// Create a new keepalive loop with the given interval.
    pub fn new(interval_ms: u64) -> Self {
        Self {
            interval_ms,
            max_duration_ms: DEFAULT_MAX_TYPING_DURATION_MS,
            running: Arc::new(AtomicBool::new(false)),
            suppressed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Create with a custom max duration (v2026.2.26).
    pub fn with_max_duration(interval_ms: u64, max_duration_ms: u64) -> Self {
        Self {
            interval_ms,
            max_duration_ms,
            running: Arc::new(AtomicBool::new(false)),
            suppressed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Whether the loop is currently running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Suppress typing indicators during tool execution (v2026.2.26).
    pub fn suppress(&self) {
        self.suppressed.store(true, Ordering::Relaxed);
    }

    /// Resume typing indicators after tool execution (v2026.2.26).
    pub fn unsuppress(&self) {
        self.suppressed.store(false, Ordering::Relaxed);
    }

    /// Start the keepalive loop, invoking `on_tick` at each interval.
    /// Returns a handle that can be used to stop the loop.
    ///
    /// v2026.2.26: auto-stops after `max_duration_ms` to prevent stuck indicators.
    pub fn start<F>(&self, on_tick: F) -> tokio::task::JoinHandle<()>
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.running.store(true, Ordering::Relaxed);
        let running = self.running.clone();
        let suppressed = self.suppressed.clone();
        let interval = self.interval_ms;
        let max_duration = self.max_duration_ms;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(interval));
            let start = std::time::Instant::now();
            // Skip the first immediate tick.
            ticker.tick().await;
            while running.load(Ordering::Relaxed) {
                ticker.tick().await;
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                // v2026.2.26: TTL safety net — auto-stop stuck indicators.
                if start.elapsed().as_millis() as u64 > max_duration {
                    running.store(false, Ordering::Relaxed);
                    break;
                }
                // v2026.2.26: skip tick if suppressed (tool execution).
                if suppressed.load(Ordering::Relaxed) {
                    continue;
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

// ============================================================================
// Channel config hygiene (v2026.6.x, PARITY_v2026.7.1 Channels row 95)
// ============================================================================

/// Whether a raw channel config entry counts as *configured*.
///
/// Mirror of upstream `isConfiguredChannel` (v2026.6.x): an object entry is
/// configured unless `enabled === false` — so an `{"enabled": true}`-only
/// entry IS configured. Non-object entries are not.
pub fn is_configured_channel_entry(entry: Option<&serde_json::Value>) -> bool {
    match entry {
        Some(serde_json::Value::Object(map)) => {
            !matches!(map.get("enabled"), Some(serde_json::Value::Bool(false)))
        }
        _ => false,
    }
}

/// Error for malformed `channel[:account]` specs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelSpecError {
    Empty,
    EmptySegment,
    TooManySegments,
}

impl std::fmt::Display for ChannelSpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelSpecError::Empty => write!(f, "channel spec is empty"),
            ChannelSpecError::EmptySegment => write!(f, "channel spec has an empty segment"),
            ChannelSpecError::TooManySegments => {
                write!(f, "channel spec has too many segments (expected channel[:account])")
            }
        }
    }
}

/// Parse a `channel[:account]` spec, rejecting malformed forms like
/// `matrix:work:extra` (v2026.6.x "malformed account specs rejected").
pub fn parse_channel_account_spec(
    spec: &str,
) -> Result<(String, Option<String>), ChannelSpecError> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err(ChannelSpecError::Empty);
    }
    let segments: Vec<&str> = trimmed.split(':').collect();
    if segments.len() > 2 {
        return Err(ChannelSpecError::TooManySegments);
    }
    if segments.iter().any(|s| s.trim().is_empty()) {
        return Err(ChannelSpecError::EmptySegment);
    }
    let channel = segments[0].trim().to_lowercase();
    let account = segments.get(1).map(|s| s.trim().to_string());
    Ok((channel, account))
}

/// Clear timeout message for channel capability checks (v2026.6.x "channel
/// capability checks return clear timeout").
pub fn capability_check_timeout_message(channel: &str, timeout_ms: u64) -> String {
    format!(
        "Channel capability check for {channel} timed out after {timeout_ms} ms; \
         the channel may still be starting. Retry, or check `channels status --json`."
    )
}

// ============================================================================
// Channel-message lifecycle adapter (v2026.5.x, row 91)
// ============================================================================

/// Durable receipt for one delivered channel message.
///
/// Mirror of upstream `defineChannelMessageAdapter` receipts: adapters return
/// the platform message id so replies/edits/reactions can target it and
/// delivery recovery can prove the send happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageReceipt {
    /// Platform-assigned message id (empty when the platform returns none).
    pub message_id: String,
    /// Chat/conversation the message landed in.
    pub chat_id: String,
}

/// Channel-message lifecycle adapter: uniform prepare→send→receipt surface.
///
/// `prepare_send_payload` turns outbound text + attachments into an ordered
/// [`normalize::SendPlan`] honoring the channel's caption support and text
/// limits; `deliver_prepared` executes the plan through the channel's native
/// APIs and returns receipts. Rollout across the bundled channels mirrors
/// upstream's ~15-channel adoption wave (per-channel wiring tracked in the
/// parity files).
#[async_trait::async_trait]
pub trait ChannelMessageAdapter: Send + Sync {
    /// Whether the channel supports captions on media sends.
    fn supports_captions(&self) -> bool {
        false
    }

    /// Caption character limit (only meaningful when captions supported).
    fn caption_limit(&self) -> usize {
        1024
    }

    /// Build the ordered send plan for outbound content.
    fn prepare_send_payload(&self, out: &normalize::NormalizedOutbound) -> normalize::SendPlan {
        normalize::build_send_plan(
            &out.text,
            &out.attachments,
            self.supports_captions(),
            self.caption_limit(),
        )
    }

    /// Execute a prepared plan, returning receipts for delivered messages.
    async fn deliver_prepared(
        &self,
        chat_id: &str,
        plan: normalize::SendPlan,
    ) -> Result<Vec<MessageReceipt>>;
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
        // Install reusable `accessGroups` allowlist groups so channel ingress
        // can expand `accessGroup:<name>` entries (v2026.5.x).
        crate::routing::access_groups::install_access_groups(config);

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

        // New channel plugins (v2026.3.3).
        plugins.insert(
            "matrix".to_string(),
            Arc::new(matrix::MatrixChannel::new()),
        );
        plugins.insert("irc".to_string(), Arc::new(irc::IrcChannel::new()));
        plugins.insert(
            "googlechat".to_string(),
            Arc::new(googlechat::GoogleChatChannel::new()),
        );
        plugins.insert(
            "teams".to_string(),
            Arc::new(teams::TeamsChannel::new()),
        );
        plugins.insert(
            "bluebubbles".to_string(),
            Arc::new(bluebubbles::BlueBubblesChannel::new()),
        );
        plugins.insert(
            "line".to_string(),
            Arc::new(line::LineChannel::new()),
        );
        plugins.insert(
            "mattermost".to_string(),
            Arc::new(mattermost::MattermostChannel::new()),
        );
        plugins.insert(
            "twitch".to_string(),
            Arc::new(twitch::TwitchChannel::new()),
        );
        plugins.insert(
            "nostr".to_string(),
            Arc::new(nostr::NostrChannel::new()),
        );
        plugins.insert(
            "feishu".to_string(),
            Arc::new(feishu::FeishuChannel::new()),
        );
        plugins.insert(
            "nextcloud".to_string(),
            Arc::new(nextcloud::NextcloudChannel::new()),
        );
        plugins.insert(
            "tlon".to_string(),
            Arc::new(tlon::TlonChannel::new()),
        );
        plugins.insert(
            "zalo".to_string(),
            Arc::new(zalo::ZaloChannel::new()),
        );
        plugins.insert(
            "zalouser".to_string(),
            Arc::new(zalouser::ZaloUserChannel::new()),
        );
        plugins.insert(
            "webchat".to_string(),
            Arc::new(webchat::WebChatChannel::new()),
        );

        // New channel plugins (v2026.7.1 parity pass).
        plugins.insert(
            "qqbot".to_string(),
            Arc::new(qqbot::QqBotChannel::new(config)),
        );
        plugins.insert(
            "yuanbao".to_string(),
            Arc::new(yuanbao::YuanbaoChannel::new(config)),
        );
        plugins.insert(
            "voicecall".to_string(),
            Arc::new(voice_call::VoiceCallChannel::new(config)),
        );
        plugins.insert(
            "googlemeet".to_string(),
            Arc::new(google_meet::GoogleMeetChannel::new(config)),
        );
        plugins.insert("sms".to_string(), Arc::new(sms::SmsChannel::new(config)));

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

// ============================================================================
// Tests (channel kernel)
// ============================================================================

#[cfg(test)]
mod kernel_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn enabled_true_only_entry_is_configured() {
        assert!(is_configured_channel_entry(Some(&json!({"enabled": true}))));
        assert!(is_configured_channel_entry(Some(&json!({}))));
        assert!(is_configured_channel_entry(Some(
            &json!({"botToken": "x", "enabled": true})
        )));
        assert!(!is_configured_channel_entry(Some(
            &json!({"enabled": false})
        )));
        assert!(!is_configured_channel_entry(Some(&json!("string"))));
        assert!(!is_configured_channel_entry(Some(&json!(null))));
        assert!(!is_configured_channel_entry(None));
    }

    #[test]
    fn channel_account_spec_parsing() {
        assert_eq!(
            parse_channel_account_spec("matrix"),
            Ok(("matrix".to_string(), None))
        );
        assert_eq!(
            parse_channel_account_spec("Matrix:Work"),
            Ok(("matrix".to_string(), Some("Work".to_string())))
        );
        assert_eq!(
            parse_channel_account_spec("matrix:work:extra"),
            Err(ChannelSpecError::TooManySegments)
        );
        assert_eq!(
            parse_channel_account_spec("matrix:"),
            Err(ChannelSpecError::EmptySegment)
        );
        assert_eq!(
            parse_channel_account_spec(" : "),
            Err(ChannelSpecError::EmptySegment)
        );
        assert_eq!(parse_channel_account_spec(""), Err(ChannelSpecError::Empty));
    }

    #[test]
    fn capability_timeout_message_is_clear() {
        let msg = capability_check_timeout_message("slack", 5000);
        assert!(msg.contains("slack"));
        assert!(msg.contains("5000"));
        assert!(msg.contains("timed out"));
    }
}
