use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use tracing::{info, warn};

// ============================================================================
// Mattermost Channel Implementation
// ============================================================================

/// Mattermost channel integration via the Mattermost REST API v4.
///
/// Communicates with a Mattermost server using a bot access token or
/// personal access token. Messages are sent via
/// `POST /api/v4/posts`.
///
/// Mattermost API docs: <https://api.mattermost.com/>
pub struct MattermostChannel {
    /// Mattermost server URL (e.g. `https://mattermost.example.com`).
    server_url: Option<String>,
    /// Bot access token or personal access token.
    token: Option<String>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// HTTP client for API calls.
    client: Client,
}

impl MattermostChannel {
    pub fn new() -> Self {
        Self {
            server_url: None,
            token: None,
            enabled: None,
            client: Client::new(),
        }
    }

    /// Create a configured Mattermost channel.
    pub fn with_config(server_url: String, token: String) -> Self {
        Self {
            server_url: Some(server_url),
            token: Some(token),
            enabled: Some(true),
            client: Client::new(),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }
}

#[async_trait]
impl ChannelPlugin for MattermostChannel {
    fn id(&self) -> &str {
        "mattermost"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Mattermost".to_string(),
            description: "Mattermost channel via REST API v4".to_string(),
            enabled: self.is_enabled(),
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
            ChannelCapability::Threads,
            ChannelCapability::EditMessage,
            ChannelCapability::DeleteMessage,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        let server_url = match &self.server_url {
            Some(url) => url,
            None => {
                warn!("Mattermost channel enabled but no server_url configured");
                return Ok(());
            }
        };

        let token = match &self.token {
            Some(t) => t,
            None => {
                warn!("Mattermost channel enabled but no token configured");
                return Ok(());
            }
        };

        info!(server_url = %server_url, "Mattermost channel starting");

        // Verify credentials by calling the /users/me endpoint.
        let me_url = format!(
            "{}/api/v4/users/me",
            server_url.trim_end_matches('/'),
        );

        match self
            .client
            .get(&me_url)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = resp.json().await.unwrap_or_default();
                let username = body["username"].as_str().unwrap_or("unknown");
                info!(username = %username, "Mattermost: authenticated successfully");
            }
            Ok(resp) => {
                warn!("Mattermost: auth check returned status {}", resp.status());
            }
            Err(e) => {
                warn!("Mattermost: failed to verify credentials: {}", e);
            }
        }

        // TODO: Set up a WebSocket connection to the Mattermost event stream
        // at `wss://<server>/api/v4/websocket` to receive incoming messages.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("Mattermost channel stopping");
            // TODO: Close the WebSocket event stream connection.
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        let server_url = self
            .server_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Mattermost server_url not configured"))?;

        let token = self
            .token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Mattermost token not configured"))?;

        // `to` is a Mattermost channel ID (26-char alphanumeric string).
        let url = format!(
            "{}/api/v4/posts",
            server_url.trim_end_matches('/'),
        );

        let body = serde_json::json!({
            "channel_id": to,
            "message": message,
        });

        info!(channel_id = %to, "Mattermost: creating post");

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
            anyhow::bail!("Mattermost post creation failed ({}): {}", status, text);
        }

        Ok(())
    }
}
