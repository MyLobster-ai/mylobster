use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

// ============================================================================
// IRC Channel Implementation
// ============================================================================

/// IRC channel integration.
///
/// Connects to an IRC server via raw TCP (optionally TLS) and joins the
/// configured channels. Uses a simple line-based IRC protocol implementation.
///
/// This is a non-REST channel — it requires a persistent TCP connection.
/// `send_message` will return an error if the connection is not active.
pub struct IrcChannel {
    /// IRC server hostname (e.g. `irc.libera.chat`).
    server: Option<String>,
    /// IRC server port (default: 6667, or 6697 for TLS).
    port: Option<u16>,
    /// Bot nickname.
    nick: Option<String>,
    /// List of IRC channels to join (e.g. `["#mylobster", "#general"]`).
    channels: Option<Vec<String>>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// Whether the IRC connection is currently active.
    connected: Arc<AtomicBool>,
}

impl IrcChannel {
    pub fn new() -> Self {
        Self {
            server: None,
            port: None,
            nick: None,
            channels: None,
            enabled: None,
            connected: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Create a configured IRC channel.
    pub fn with_config(
        server: String,
        port: u16,
        nick: String,
        channels: Vec<String>,
    ) -> Self {
        Self {
            server: Some(server),
            port: Some(port),
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
impl ChannelPlugin for IrcChannel {
    fn id(&self) -> &str {
        "irc"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "IRC".to_string(),
            description: "Internet Relay Chat channel via raw TCP connection".to_string(),
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

        let server = match &self.server {
            Some(s) => s,
            None => {
                warn!("IRC channel enabled but no server configured");
                return Ok(());
            }
        };

        let port = self.port.unwrap_or(6667);
        let nick = self.nick.as_deref().unwrap_or("mylobster");
        let channels = self.channels.as_deref().unwrap_or(&[]);

        info!(
            server = %server,
            port = %port,
            nick = %nick,
            channels = ?channels,
            "IRC channel starting — would connect to server"
        );

        // TODO: Establish a TCP (or TLS) connection to the IRC server.
        // 1. Send NICK and USER commands.
        // 2. Join configured channels.
        // 3. Start a read loop to parse incoming IRC messages.
        //
        // The connection lifecycle would be managed in a spawned task,
        // setting `self.connected` to true once registered.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("IRC channel stopping");
            self.connected.store(false, Ordering::Relaxed);
            // TODO: Send QUIT command and close the TCP connection.
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        if !self.connected.load(Ordering::Relaxed) {
            anyhow::bail!("IRC: not connected — cannot send message to {}", to);
        }

        info!(target_channel = %to, "IRC: sending PRIVMSG");

        // `to` is an IRC channel name (e.g. "#mylobster") or a nick for DMs.
        // The message would be sent as: PRIVMSG <to> :<message>
        //
        // TODO: Write the PRIVMSG line to the active TCP stream.
        // For long messages, split into multiple lines (IRC has a ~512 byte limit).
        let _ = message;

        Ok(())
    }
}
