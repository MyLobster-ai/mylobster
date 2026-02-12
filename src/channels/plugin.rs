use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Describes the capabilities a channel plugin supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelCapability {
    /// The channel can send text messages.
    SendText,
    /// The channel can receive text messages.
    ReceiveText,
    /// The channel supports sending media (images, files, etc.).
    SendMedia,
    /// The channel supports receiving media.
    ReceiveMedia,
    /// The channel supports emoji reactions.
    Reactions,
    /// The channel supports group / multi-user conversations.
    Groups,
    /// The channel supports threaded replies.
    Threads,
    /// The channel supports read receipts.
    ReadReceipts,
    /// The channel supports typing indicators.
    TypingIndicators,
    /// The channel supports message editing.
    EditMessage,
    /// The channel supports message deletion.
    DeleteMessage,
    /// The channel supports voice messages / audio.
    Voice,
    /// The channel supports stickers.
    Stickers,
    /// The channel supports polls.
    Polls,
}

/// Static metadata about a channel plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelMeta {
    /// Human-readable channel name (e.g. "Telegram", "Discord").
    pub name: String,
    /// Short description of the channel.
    pub description: String,
    /// Whether the channel is currently enabled in configuration.
    pub enabled: bool,
    /// Whether the channel supports multi-account mode.
    pub multi_account: bool,
}

/// Trait that every channel implementation must satisfy.
///
/// Implementations provide the bridge between the gateway and an external
/// messaging platform (Telegram, Discord, Slack, etc.).
#[async_trait]
pub trait ChannelPlugin: Send + Sync + 'static {
    /// Unique identifier for this channel (e.g. "telegram").
    fn id(&self) -> &str;

    /// Return static metadata about this channel.
    fn meta(&self) -> ChannelMeta;

    /// Return the capabilities this channel supports.
    fn capabilities(&self) -> Vec<ChannelCapability>;

    /// Start the channel account / connection.
    ///
    /// This is where bots log in, webhooks are registered, WebSocket
    /// connections are opened, etc.
    async fn start_account(&self, state: &GatewayState) -> Result<()>;

    /// Stop the channel account / connection gracefully.
    async fn stop_account(&self) -> Result<()>;

    /// Send a text message to the given recipient on this channel.
    ///
    /// The meaning of `to` is channel-specific: it may be a chat ID, a
    /// channel name, a phone number, etc.
    async fn send_message(&self, to: &str, message: &str) -> Result<()>;
}
