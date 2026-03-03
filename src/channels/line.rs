use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use tracing::{info, warn};

// ============================================================================
// LINE Channel Implementation
// ============================================================================

/// LINE Messaging API channel integration.
///
/// Sends and receives messages via the LINE Messaging API.
/// Push messages use `POST https://api.line.me/v2/bot/message/push`.
/// Reply messages use `POST https://api.line.me/v2/bot/message/reply`.
///
/// Requires a LINE Messaging API channel access token and channel secret
/// (for webhook signature verification).
pub struct LineChannel {
    /// Long-lived channel access token from the LINE Developer Console.
    channel_access_token: Option<String>,
    /// Channel secret for webhook signature verification (HMAC-SHA256).
    channel_secret: Option<String>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// HTTP client for LINE API calls.
    client: Client,
}

/// LINE Messaging API base URL.
const LINE_API_BASE: &str = "https://api.line.me/v2/bot";

impl LineChannel {
    pub fn new() -> Self {
        Self {
            channel_access_token: None,
            channel_secret: None,
            enabled: None,
            client: Client::new(),
        }
    }

    /// Create a configured LINE channel.
    pub fn with_config(channel_access_token: String, channel_secret: String) -> Self {
        Self {
            channel_access_token: Some(channel_access_token),
            channel_secret: Some(channel_secret),
            enabled: Some(true),
            client: Client::new(),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }
}

#[async_trait]
impl ChannelPlugin for LineChannel {
    fn id(&self) -> &str {
        "line"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "LINE".to_string(),
            description: "LINE Messaging API channel".to_string(),
            enabled: self.is_enabled(),
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
            ChannelCapability::Stickers,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        if self.channel_access_token.is_none() {
            warn!("LINE channel enabled but no channel_access_token configured");
            return Ok(());
        }

        info!("LINE channel starting");

        // Verify the token by calling the bot info endpoint.
        if let Some(token) = &self.channel_access_token {
            let url = format!("{}/info", LINE_API_BASE);
            match self
                .client
                .get(&url)
                .header("Authorization", format!("Bearer {}", token))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    info!("LINE: bot info endpoint reachable, token valid");
                }
                Ok(resp) => {
                    warn!("LINE: bot info returned status {}", resp.status());
                }
                Err(e) => {
                    warn!("LINE: failed to verify token: {}", e);
                }
            }
        }

        // TODO: Register a webhook endpoint for incoming LINE messages.
        // LINE sends events to the webhook URL configured in the LINE Developer Console.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("LINE channel stopping");
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        let token = self
            .channel_access_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("LINE channel_access_token not configured"))?;

        // `to` is a LINE user ID, group ID, or room ID.
        let url = format!("{}/message/push", LINE_API_BASE);

        let body = serde_json::json!({
            "to": to,
            "messages": [
                {
                    "type": "text",
                    "text": message,
                }
            ],
        });

        info!(to = %to, "LINE: sending push message");

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("LINE push message failed ({}): {}", status, text);
        }

        Ok(())
    }
}
