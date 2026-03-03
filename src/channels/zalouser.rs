use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use tracing::{info, warn};

// ============================================================================
// Zalo Personal (User) Channel Implementation
// ============================================================================

/// Zalo Personal (user-level) channel integration.
///
/// Unlike the Zalo OA channel which operates on behalf of an Official Account,
/// this channel sends messages as a regular Zalo user via the Zalo Social API.
///
/// Messages are sent via `POST https://openapi.zalo.me/v2.0/oa/message`
/// using a user access token obtained through the Zalo Login OAuth2 flow.
///
/// API docs: <https://developers.zalo.me/docs/social-api>
///
/// Note: The Zalo Social API has stricter rate limits and usage policies
/// compared to the OA API. User-level messaging may require explicit consent.
pub struct ZaloUserChannel {
    /// Zalo user access token (obtained via Zalo Login OAuth2 flow).
    user_access_token: Option<String>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// HTTP client for API calls.
    client: Client,
}

/// Zalo Social API base URL.
const ZALO_SOCIAL_API_BASE: &str = "https://openapi.zalo.me/v2.0";

impl ZaloUserChannel {
    pub fn new() -> Self {
        Self {
            user_access_token: None,
            enabled: None,
            client: Client::new(),
        }
    }

    /// Create a configured Zalo Personal channel.
    pub fn with_config(user_access_token: String) -> Self {
        Self {
            user_access_token: Some(user_access_token),
            enabled: Some(true),
            client: Client::new(),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }
}

#[async_trait]
impl ChannelPlugin for ZaloUserChannel {
    fn id(&self) -> &str {
        "zalouser"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Zalo Personal".to_string(),
            description: "Zalo personal messaging channel via Social API".to_string(),
            enabled: self.is_enabled(),
            multi_account: false,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![
            ChannelCapability::SendText,
            ChannelCapability::ReceiveText,
            ChannelCapability::SendMedia,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        if self.user_access_token.is_none() {
            warn!("Zalo Personal channel enabled but no user_access_token configured");
            return Ok(());
        }

        info!("Zalo Personal channel starting");

        // Verify the token by calling the user profile endpoint.
        if let Some(token) = &self.user_access_token {
            let url = format!("{}/me?fields=id,name", ZALO_SOCIAL_API_BASE);
            match self
                .client
                .get(&url)
                .header("access_token", token.as_str())
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    let body: serde_json::Value = resp.json().await.unwrap_or_default();
                    if body["error"].as_i64() == Some(0) || body["id"].is_string() {
                        let name = body["name"].as_str().unwrap_or("unknown");
                        info!(name = %name, "Zalo Personal: authenticated successfully");
                    } else {
                        let msg = body["message"].as_str().unwrap_or("unknown error");
                        warn!("Zalo Personal: profile returned error: {}", msg);
                    }
                }
                Ok(resp) => {
                    warn!(
                        "Zalo Personal: profile endpoint returned status {}",
                        resp.status()
                    );
                }
                Err(e) => {
                    warn!("Zalo Personal: failed to verify token: {}", e);
                }
            }
        }

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("Zalo Personal channel stopping");
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        let token = self
            .user_access_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Zalo Personal user_access_token not configured"))?;

        // `to` is a Zalo friend's user ID.
        let url = format!("{}/apigraph/v2/me/message", ZALO_SOCIAL_API_BASE);

        let body = serde_json::json!({
            "to": to,
            "message": message,
        });

        info!(to = %to, "Zalo Personal: sending message");

        let resp = self
            .client
            .post(&url)
            .header("access_token", token)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Zalo Personal send message failed ({}): {}", status, text);
        }

        // Check the Zalo API-level error code.
        let result: serde_json::Value = resp.json().await?;
        let error = result["error"].as_i64().unwrap_or(-1);
        if error != 0 {
            let msg = result["message"].as_str().unwrap_or("unknown error");
            anyhow::bail!("Zalo Personal send error (code {}): {}", error, msg);
        }

        Ok(())
    }
}
