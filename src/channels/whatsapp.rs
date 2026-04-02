use crate::config::Config;
use crate::gateway::GatewayState;

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};

use anyhow::Result;
use async_trait::async_trait;
use tracing::info;

/// WhatsApp channel implementation.
///
/// Connects to the WhatsApp Business API (or a compatible bridge) to send and
/// receive messages.
pub struct WhatsAppChannel {
    enabled: bool,
}

impl WhatsAppChannel {
    pub fn new(config: &Config) -> Self {
        let wa = &config.channels.whatsapp;
        let enabled = wa.default_account.enabled.unwrap_or(false);

        Self { enabled }
    }
}

#[async_trait]
impl ChannelPlugin for WhatsAppChannel {
    fn id(&self) -> &str {
        "whatsapp"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "WhatsApp".to_string(),
            description: "WhatsApp Business API channel".to_string(),
            enabled: self.enabled,
            multi_account: true,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![
            ChannelCapability::SendText,
            ChannelCapability::ReceiveText,
            ChannelCapability::SendMedia,
            ChannelCapability::ReceiveMedia,
            ChannelCapability::Reactions,
            ChannelCapability::Groups,
            ChannelCapability::ReadReceipts,
            ChannelCapability::TypingIndicators,
            ChannelCapability::Voice,
            ChannelCapability::Polls,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        info!("WhatsApp channel starting");

        // TODO: Connect to WhatsApp Business API / bridge.
        // The implementation will:
        // 1. Authenticate via the configured auth_dir / session.
        // 2. Set up a message listener to forward incoming messages.
        // 3. Optionally send read receipts.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.enabled {
            info!("WhatsApp channel stopping");
            // TODO: Disconnect from WhatsApp.
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, _message: &str) -> Result<()> {
        info!(to = to, "WhatsApp: sending message");

        // TODO: Use the WhatsApp API client to send _message.
        // `to` is expected to be a phone number in international format (e.g. "+1234567890").

        Ok(())
    }
}

/// Convenience function called by the top-level `send_message` dispatcher.
pub(crate) async fn send_message(config: &Config, to: &str, message: &str) -> Result<()> {
    let channel = WhatsAppChannel::new(config);
    channel.send_message(to, message).await
}

// v2026.4.1: Support HTML/XML/CSS MIME types + fallback for unknown types
fn resolve_mime_type(filename: &str, content_type: Option<&str>) -> String {
    if let Some(ct) = content_type {
        return ct.to_string();
    }
    let ext = filename.rsplit('.').next().unwrap_or("");
    match ext {
        "html" | "htm" => "text/html".to_string(),
        "xml" => "application/xml".to_string(),
        "css" => "text/css".to_string(),
        "js" => "application/javascript".to_string(),
        "json" => "application/json".to_string(),
        "pdf" => "application/pdf".to_string(),
        "png" => "image/png".to_string(),
        "jpg" | "jpeg" => "image/jpeg".to_string(),
        "gif" => "image/gif".to_string(),
        "mp4" => "video/mp4".to_string(),
        "mp3" => "audio/mpeg".to_string(),
        "ogg" => "audio/ogg".to_string(),
        "webp" => "image/webp".to_string(),
        _ => "application/octet-stream".to_string(),
    }
}
