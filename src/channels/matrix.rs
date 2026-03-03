use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use tracing::{info, warn};

// ============================================================================
// Matrix Channel Implementation
// ============================================================================

/// Matrix channel integration using the Client-Server API.
///
/// Communicates with a Matrix homeserver via the Matrix Client-Server API
/// (`/_matrix/client/v3/`). Sends messages using the `/rooms/{roomId}/send`
/// endpoint with `m.room.message` events.
pub struct MatrixChannel {
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// Matrix homeserver URL (e.g. `https://matrix.org`).
    homeserver_url: Option<String>,
    /// Access token for the Matrix account.
    access_token: Option<String>,
    /// Matrix user ID (e.g. `@bot:matrix.org`).
    user_id: Option<String>,
    /// HTTP client for API calls.
    client: Client,
}

impl MatrixChannel {
    pub fn new() -> Self {
        Self {
            enabled: None,
            homeserver_url: None,
            access_token: None,
            user_id: None,
            client: Client::new(),
        }
    }

    /// Create a configured Matrix channel.
    pub fn with_config(
        homeserver_url: String,
        access_token: String,
        user_id: String,
    ) -> Self {
        Self {
            enabled: Some(true),
            homeserver_url: Some(homeserver_url),
            access_token: Some(access_token),
            user_id: Some(user_id),
            client: Client::new(),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }
}

#[async_trait]
impl ChannelPlugin for MatrixChannel {
    fn id(&self) -> &str {
        "matrix"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Matrix".to_string(),
            description: "Matrix protocol channel via Client-Server API".to_string(),
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
            ChannelCapability::ReadReceipts,
            ChannelCapability::TypingIndicators,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        let homeserver = match &self.homeserver_url {
            Some(url) => url,
            None => {
                warn!("Matrix channel enabled but no homeserver_url configured");
                return Ok(());
            }
        };

        if self.access_token.is_none() {
            warn!("Matrix channel enabled but no access_token configured");
            return Ok(());
        }

        let user_id = self.user_id.as_deref().unwrap_or("(unknown)");
        info!(
            homeserver = %homeserver,
            user_id = %user_id,
            "Matrix channel starting"
        );

        // TODO: Start a /sync long-poll loop to receive incoming messages.
        // The sync loop would call `/_matrix/client/v3/sync` with a `since`
        // token and dispatch incoming `m.room.message` events.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("Matrix channel stopping");
            // TODO: Cancel the sync loop task.
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        let homeserver = self
            .homeserver_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Matrix homeserver_url not configured"))?;

        let access_token = self
            .access_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Matrix access_token not configured"))?;

        // `to` is a Matrix room ID (e.g. "!abc123:matrix.org").
        let room_id = to;
        let txn_id = uuid::Uuid::new_v4().to_string();

        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            homeserver.trim_end_matches('/'),
            urlencoded(room_id),
            txn_id,
        );

        let body = serde_json::json!({
            "msgtype": "m.text",
            "body": message,
        });

        info!(room_id = %room_id, "Matrix: sending message");

        let resp = self
            .client
            .put(&url)
            .bearer_token(access_token)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Matrix send failed ({}): {}", status, text);
        }

        Ok(())
    }
}

/// Percent-encode a Matrix room ID for use in URL paths.
fn urlencoded(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Helper trait to add bearer_token to reqwest::RequestBuilder.
trait BearerToken {
    fn bearer_token(self, token: &str) -> Self;
}

impl BearerToken for reqwest::RequestBuilder {
    fn bearer_token(self, token: &str) -> Self {
        self.header("Authorization", format!("Bearer {}", token))
    }
}
