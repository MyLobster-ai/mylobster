use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use tracing::{info, warn};

// ============================================================================
// Nextcloud Talk Channel Implementation
// ============================================================================

/// Nextcloud Talk channel integration via the OCS API.
///
/// Communicates with a Nextcloud instance using the Talk API (OCS format).
/// Messages are sent via
/// `POST /ocs/v2.php/apps/spreed/api/v1/chat/{token}`.
///
/// Authentication uses either a Nextcloud app password or a bot-specific
/// token. All API calls require the `OCS-APIRequest: true` header.
///
/// API docs: <https://nextcloud-talk.readthedocs.io/en/latest/>
pub struct NextcloudChannel {
    /// Nextcloud server URL (e.g. `https://cloud.example.com`).
    server_url: Option<String>,
    /// Authentication token (app password or bot token).
    token: Option<String>,
    /// Nextcloud username for basic auth.
    username: Option<String>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// HTTP client for API calls.
    client: Client,
}

impl NextcloudChannel {
    pub fn new() -> Self {
        Self {
            server_url: None,
            token: None,
            username: None,
            enabled: None,
            client: Client::new(),
        }
    }

    /// Create a configured Nextcloud Talk channel.
    pub fn with_config(server_url: String, username: String, token: String) -> Self {
        Self {
            server_url: Some(server_url),
            token: Some(token),
            username: Some(username),
            enabled: Some(true),
            client: Client::new(),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }
}

#[async_trait]
impl ChannelPlugin for NextcloudChannel {
    fn id(&self) -> &str {
        "nextcloud"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Nextcloud Talk".to_string(),
            description: "Nextcloud Talk channel via OCS API".to_string(),
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
            ChannelCapability::Reactions,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        let server_url = match &self.server_url {
            Some(url) => url,
            None => {
                warn!("Nextcloud Talk channel enabled but no server_url configured");
                return Ok(());
            }
        };

        if self.token.is_none() {
            warn!("Nextcloud Talk channel enabled but no token configured");
            return Ok(());
        }

        info!(server_url = %server_url, "Nextcloud Talk channel starting");

        // Verify connectivity by calling the capabilities endpoint.
        let caps_url = format!(
            "{}/ocs/v2.php/cloud/capabilities",
            server_url.trim_end_matches('/'),
        );

        let username = self.username.as_deref().unwrap_or("bot");
        let token = self.token.as_deref().unwrap_or_default();

        match self
            .client
            .get(&caps_url)
            .basic_auth(username, Some(token))
            .header("OCS-APIRequest", "true")
            .header("Accept", "application/json")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                info!("Nextcloud Talk: server capabilities endpoint reachable");
            }
            Ok(resp) => {
                warn!(
                    "Nextcloud Talk: capabilities returned status {}",
                    resp.status()
                );
            }
            Err(e) => {
                warn!("Nextcloud Talk: failed to reach server: {}", e);
            }
        }

        // TODO: Set up long-polling or SSE endpoint to receive incoming messages.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("Nextcloud Talk channel stopping");
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        let server_url = self
            .server_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Nextcloud Talk server_url not configured"))?;

        let username = self
            .username
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Nextcloud Talk username not configured"))?;

        let token = self
            .token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Nextcloud Talk token not configured"))?;

        // `to` is a Nextcloud Talk conversation token (e.g. "abc123xy").
        let url = format!(
            "{}/ocs/v2.php/apps/spreed/api/v1/chat/{}",
            server_url.trim_end_matches('/'),
            to,
        );

        let body = serde_json::json!({
            "message": message,
        });

        info!(conversation = %to, "Nextcloud Talk: sending message");

        let resp = self
            .client
            .post(&url)
            .basic_auth(username, Some(token))
            .header("OCS-APIRequest", "true")
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Nextcloud Talk send message failed ({}): {}",
                status,
                text
            );
        }

        Ok(())
    }
}
