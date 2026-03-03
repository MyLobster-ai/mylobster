use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use tracing::{info, warn};

// ============================================================================
// Zalo OA Channel Implementation
// ============================================================================

/// Zalo Official Account (OA) channel integration.
///
/// Communicates with users via the Zalo OA API. Official Accounts can
/// send proactive messages to followers and reply to incoming messages.
///
/// Messages are sent via `POST https://openapi.zalo.me/v3.0/oa/message/cs`
/// (customer service messages) or `/v3.0/oa/message` (transactional).
///
/// API docs: <https://developers.zalo.me/docs/official-account>
pub struct ZaloChannel {
    /// Zalo OA access token (obtained via OAuth2 flow).
    oa_access_token: Option<String>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// HTTP client for API calls.
    client: Client,
}

/// Zalo OA API base URL.
const ZALO_OA_API_BASE: &str = "https://openapi.zalo.me/v3.0/oa";

impl ZaloChannel {
    pub fn new() -> Self {
        Self {
            oa_access_token: None,
            enabled: None,
            client: Client::new(),
        }
    }

    /// Create a configured Zalo OA channel.
    pub fn with_config(oa_access_token: String) -> Self {
        Self {
            oa_access_token: Some(oa_access_token),
            enabled: Some(true),
            client: Client::new(),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }
}

#[async_trait]
impl ChannelPlugin for ZaloChannel {
    fn id(&self) -> &str {
        "zalo"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Zalo".to_string(),
            description: "Zalo Official Account messaging channel".to_string(),
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
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        if self.oa_access_token.is_none() {
            warn!("Zalo OA channel enabled but no oa_access_token configured");
            return Ok(());
        }

        info!("Zalo OA channel starting");

        // Verify the token by calling the OA info endpoint.
        if let Some(token) = &self.oa_access_token {
            let url = format!("{}/getoa", ZALO_OA_API_BASE);
            match self
                .client
                .get(&url)
                .header("access_token", token.as_str())
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    let body: serde_json::Value = resp.json().await.unwrap_or_default();
                    if body["error"].as_i64() == Some(0) {
                        let oa_name = body["data"]["name"].as_str().unwrap_or("unknown");
                        info!(oa_name = %oa_name, "Zalo OA: authenticated successfully");
                    } else {
                        let msg = body["message"].as_str().unwrap_or("unknown error");
                        warn!("Zalo OA: getoa returned error: {}", msg);
                    }
                }
                Ok(resp) => {
                    warn!("Zalo OA: getoa returned status {}", resp.status());
                }
                Err(e) => {
                    warn!("Zalo OA: failed to verify token: {}", e);
                }
            }
        }

        // TODO: Register a webhook endpoint for incoming Zalo messages.
        // Zalo sends events to the webhook URL configured in the Zalo Developer portal.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("Zalo OA channel stopping");
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        let token = self
            .oa_access_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Zalo OA access_token not configured"))?;

        // `to` is a Zalo user ID (follower of the OA).
        // Use the customer service message endpoint for conversational replies.
        let url = format!("{}/message/cs", ZALO_OA_API_BASE);

        let body = serde_json::json!({
            "recipient": {
                "user_id": to,
            },
            "message": {
                "text": message,
            },
        });

        info!(user_id = %to, "Zalo OA: sending message");

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
            anyhow::bail!("Zalo OA send message failed ({}): {}", status, text);
        }

        // Check the Zalo API-level error code.
        let result: serde_json::Value = resp.json().await?;
        let error = result["error"].as_i64().unwrap_or(-1);
        if error != 0 {
            let msg = result["message"].as_str().unwrap_or("unknown error");
            anyhow::bail!("Zalo OA send error (code {}): {}", error, msg);
        }

        Ok(())
    }
}
