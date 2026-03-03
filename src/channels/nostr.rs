use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

// ============================================================================
// Nostr Channel Implementation
// ============================================================================

/// Nostr relay channel integration.
///
/// Connects to one or more Nostr relays via WebSocket and publishes/receives
/// NIP-01 text note events (kind 1) and NIP-04 encrypted DMs (kind 4).
///
/// This is a non-REST channel — it requires persistent WebSocket connections
/// to relays. `send_message` will return an error if not connected.
///
/// Protocol reference: <https://github.com/nostr-protocol/nips>
pub struct NostrChannel {
    /// Nostr private key (hex-encoded, 32 bytes / 64 hex chars).
    /// Used to sign events and derive the public key (npub).
    private_key: Option<String>,
    /// List of relay WebSocket URLs (e.g. `["wss://relay.damus.io"]`).
    relays: Option<Vec<String>>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// Whether relay connections are currently active.
    connected: Arc<AtomicBool>,
}

impl NostrChannel {
    pub fn new() -> Self {
        Self {
            private_key: None,
            relays: None,
            enabled: None,
            connected: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Create a configured Nostr channel.
    pub fn with_config(private_key: String, relays: Vec<String>) -> Self {
        Self {
            private_key: Some(private_key),
            relays: Some(relays),
            enabled: Some(true),
            connected: Arc::new(AtomicBool::new(false)),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }
}

#[async_trait]
impl ChannelPlugin for NostrChannel {
    fn id(&self) -> &str {
        "nostr"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Nostr".to_string(),
            description: "Nostr protocol channel via relay WebSocket connections".to_string(),
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

        if self.private_key.is_none() {
            warn!("Nostr channel enabled but no private_key configured");
            return Ok(());
        }

        let relays = self.relays.as_deref().unwrap_or(&[]);
        if relays.is_empty() {
            warn!("Nostr channel enabled but no relays configured");
            return Ok(());
        }

        info!(
            relay_count = relays.len(),
            relays = ?relays,
            "Nostr channel starting — would connect to relays"
        );

        // TODO: For each relay:
        // 1. Open a WebSocket connection (tokio-tungstenite).
        // 2. Subscribe to events mentioning our pubkey (REQ filter).
        // 3. Handle incoming EVENT, EOSE, NOTICE messages.
        // 4. Implement NIP-04 decryption for encrypted DMs.
        //
        // Event signing uses secp256k1 Schnorr signatures (BIP-340).
        // The private key is used to derive the public key and sign events.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("Nostr channel stopping");
            self.connected.store(false, Ordering::Relaxed);
            // TODO: Send CLOSE to all subscriptions and disconnect from relays.
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        if !self.connected.load(Ordering::Relaxed) {
            anyhow::bail!("Nostr: not connected to any relay — cannot send message");
        }

        info!(to = %to, "Nostr: publishing event");

        // `to` interpretation depends on the event kind:
        // - For kind 1 (text note): `to` is ignored (broadcast to relays)
        // - For kind 4 (encrypted DM): `to` is the recipient's pubkey (hex)
        //
        // TODO: Construct a NIP-01 event, sign it with the private key,
        // and publish via ["EVENT", <event>] to all connected relays.
        let _ = message;

        Ok(())
    }
}
