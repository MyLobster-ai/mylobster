//! Outbound delivery types and chat-type classification (v2026.2.24).
//!
//! These are type declarations and stub functions for the delivery pipeline.
//! Full delivery implementation is out of scope — this module provides the
//! type foundation that the rest of the codebase can reference.

use serde::{Deserialize, Serialize};

/// A channel that can receive delivered messages.
///
/// In OpenClaw this is `DeliverableMessageChannel` — one of the known
/// platform channel identifiers.
pub type DeliverableMessageChannel = String;

/// An outbound channel target: either a deliverable channel name or `"none"`.
pub type OutboundChannel = String;

/// Identifies a specific delivery target for outbound messages.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OutboundTarget {
    /// Channel identifier (e.g. "telegram", "discord", "slack", "none").
    pub channel: String,
    /// Recipient address within the channel (chat ID, user ID, etc.).
    pub to: Option<String>,
    /// Why this target was selected (e.g. "heartbeat", "reply", "explicit").
    pub reason: Option<String>,
    /// Account ID within the channel.
    pub account_id: Option<String>,
    /// Thread/topic ID for threaded channels.
    pub thread_id: Option<String>,
}

/// A resolved delivery target for a session, including routing metadata.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionDeliveryTarget {
    /// Channel identifier.
    pub channel: String,
    /// Recipient address.
    pub to: Option<String>,
    /// Account ID within the channel.
    pub account_id: Option<String>,
    /// Thread/topic ID.
    pub thread_id: Option<String>,
    /// Whether the thread ID was explicitly set (vs inferred).
    pub thread_id_explicit: Option<bool>,
    /// Delivery mode hint.
    pub mode: Option<String>,
}

/// Classification of a chat interaction type for delivery routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatType {
    /// One-on-one direct message.
    Direct,
    /// Public or semi-public channel message.
    Channel,
    /// Group chat (multiple participants, but not a public channel).
    Group,
}

/// Resolve the chat type for heartbeat delivery based on channel-specific
/// target parsing.
///
/// This is a stub — full implementation requires per-channel target format
/// knowledge (e.g. Telegram chat IDs are negative for groups, Discord has
/// guild channels vs DMs, etc.).
pub fn resolve_heartbeat_delivery_chat_type(
    channel: &str,
    _to: Option<&str>,
) -> ChatType {
    // Default heuristic: heartbeats are typically direct messages.
    // Per-channel refinement would go here.
    match channel {
        "discord" | "slack" | "msteams" | "irc" => ChatType::Channel,
        _ => ChatType::Direct,
    }
}
