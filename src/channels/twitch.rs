use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

// ============================================================================
// Twitch Channel Implementation
// ============================================================================

/// Twitch chat channel integration via IRC (TMI).
///
/// Twitch chat uses an IRC-compatible protocol at `irc.chat.twitch.tv:6697`
/// (TLS). Authentication is via OAuth token. Messages are standard
/// PRIVMSG commands to Twitch channels (prefixed with `#`).
///
/// This is a non-REST channel — it requires a persistent IRC/TMI connection.
/// `send_message` will return an error if the connection is not active.
pub struct TwitchChannel {
    /// OAuth token for Twitch IRC (format: `oauth:xxxxx`).
    oauth_token: Option<String>,
    /// Bot nickname (Twitch username, lowercase).
    nick: Option<String>,
    /// List of Twitch channels to join (without `#` prefix).
    channels: Option<Vec<String>>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// Whether the Twitch IRC connection is currently active.
    connected: Arc<AtomicBool>,
}

/// Twitch IRC (TMI) server address.
const TWITCH_IRC_HOST: &str = "irc.chat.twitch.tv";
/// Twitch IRC TLS port.
const TWITCH_IRC_PORT: u16 = 6697;

impl TwitchChannel {
    pub fn new() -> Self {
        Self {
            oauth_token: None,
            nick: None,
            channels: None,
            enabled: None,
            connected: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Create a configured Twitch channel.
    pub fn with_config(
        oauth_token: String,
        nick: String,
        channels: Vec<String>,
    ) -> Self {
        Self {
            oauth_token: Some(oauth_token),
            nick: Some(nick),
            channels: Some(channels),
            enabled: Some(true),
            connected: Arc::new(AtomicBool::new(false)),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }
}

#[async_trait]
impl ChannelPlugin for TwitchChannel {
    fn id(&self) -> &str {
        "twitch"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Twitch".to_string(),
            description: "Twitch chat channel via IRC/TMI protocol".to_string(),
            enabled: self.is_enabled(),
            multi_account: false,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![
            ChannelCapability::SendText,
            ChannelCapability::ReceiveText,
            ChannelCapability::Groups,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        let oauth_token = match &self.oauth_token {
            Some(t) => t,
            None => {
                warn!("Twitch channel enabled but no oauth_token configured");
                return Ok(());
            }
        };

        let nick = self.nick.as_deref().unwrap_or("mylobster");
        let channels = self.channels.as_deref().unwrap_or(&[]);

        info!(
            host = %TWITCH_IRC_HOST,
            port = %TWITCH_IRC_PORT,
            nick = %nick,
            channels = ?channels,
            token_suffix = %&oauth_token[oauth_token.len().saturating_sub(4)..],
            "Twitch channel starting — would connect to TMI"
        );

        // TODO: Establish a TLS connection to irc.chat.twitch.tv:6697.
        // 1. Send: PASS oauth:<token>
        // 2. Send: NICK <nick>
        // 3. Send: CAP REQ :twitch.tv/membership twitch.tv/tags twitch.tv/commands
        // 4. JOIN each configured channel (prefixed with #).
        // 5. Start a read loop to parse incoming IRC messages and PING/PONG.
        //
        // The connection lifecycle would be managed in a spawned task,
        // setting `self.connected` to true once the welcome message (001) is received.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("Twitch channel stopping");
            self.connected.store(false, Ordering::Relaxed);
            // TODO: Send QUIT and close the TLS connection.
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        if !self.connected.load(Ordering::Relaxed) {
            anyhow::bail!(
                "Twitch: not connected — cannot send message to #{}",
                to
            );
        }

        info!(channel = %to, "Twitch: sending PRIVMSG");

        // `to` is a Twitch channel name (without `#` prefix).
        // The message would be sent as: PRIVMSG #<to> :<message>
        //
        // TODO: Write the PRIVMSG line to the active TLS stream.
        // Twitch IRC has a 500-char message limit; split if needed.
        let _ = message;

        Ok(())
    }
}
