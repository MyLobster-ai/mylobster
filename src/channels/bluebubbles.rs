use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use tracing::{info, warn};

// ============================================================================
// BlueBubbles Channel Implementation
// ============================================================================

/// BlueBubbles iMessage bridge channel.
///
/// Connects to a BlueBubbles server (running on a Mac with iMessage) to
/// send and receive iMessage/SMS messages through its REST API.
///
/// BlueBubbles API docs: <https://documenter.getpostman.com/view/765844/UV5RnfwM>
///
/// The server typically runs on `http://<mac-ip>:1234` and requires a
/// password for authentication.
pub struct BlueBubblesChannel {
    /// BlueBubbles server API URL (e.g. `http://192.168.1.100:1234`).
    api_url: Option<String>,
    /// BlueBubbles server password.
    password: Option<String>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// HTTP client for API calls.
    client: Client,
}

impl BlueBubblesChannel {
    pub fn new() -> Self {
        Self {
            api_url: None,
            password: None,
            enabled: None,
            client: Client::new(),
        }
    }

    /// Create a configured BlueBubbles channel.
    pub fn with_config(api_url: String, password: String) -> Self {
        Self {
            api_url: Some(api_url),
            password: Some(password),
            enabled: Some(true),
            client: Client::new(),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }
}

#[async_trait]
impl ChannelPlugin for BlueBubblesChannel {
    fn id(&self) -> &str {
        "bluebubbles"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "BlueBubbles".to_string(),
            description: "BlueBubbles iMessage bridge for sending/receiving iMessages".to_string(),
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
            ChannelCapability::ReadReceipts,
            ChannelCapability::TypingIndicators,
            ChannelCapability::Reactions,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        let api_url = match &self.api_url {
            Some(url) => url,
            None => {
                warn!("BlueBubbles channel enabled but no api_url configured");
                return Ok(());
            }
        };

        if self.password.is_none() {
            warn!("BlueBubbles channel enabled but no password configured");
            return Ok(());
        }

        info!(api_url = %api_url, "BlueBubbles channel starting");

        // Verify server connectivity by calling the server info endpoint.
        let password = self.password.as_deref().unwrap_or_default();
        let info_url = format!(
            "{}/api/v1/server/info?password={}",
            api_url.trim_end_matches('/'),
            password,
        );

        match self.client.get(&info_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                info!("BlueBubbles: server connectivity verified");
            }
            Ok(resp) => {
                warn!(
                    "BlueBubbles: server returned status {}",
                    resp.status()
                );
            }
            Err(e) => {
                warn!("BlueBubbles: failed to reach server: {}", e);
            }
        }

        // TODO: Register a webhook endpoint for incoming messages, or start
        // polling the messages endpoint for new messages.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("BlueBubbles channel stopping");
            // TODO: Deregister webhook if registered.
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        let api_url = self
            .api_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("BlueBubbles api_url not configured"))?;

        let password = self
            .password
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("BlueBubbles password not configured"))?;

        // `to` is a phone number or iMessage email address.
        let url = format!(
            "{}/api/v1/message/text?password={}",
            api_url.trim_end_matches('/'),
            password,
        );

        let body = serde_json::json!({
            "chatGuid": format!("iMessage;-;{}", to),
            "tempGuid": uuid::Uuid::new_v4().to_string(),
            "message": message,
            "method": "private-api",
        });

        info!(to = %to, "BlueBubbles: sending iMessage");

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("BlueBubbles send failed ({}): {}", status, text);
        }

        Ok(())
    }
}
