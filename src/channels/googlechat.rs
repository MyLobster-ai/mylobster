use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use tracing::{info, warn};

// ============================================================================
// Google Chat Channel Implementation
// ============================================================================

/// Google Chat channel integration.
///
/// Supports two modes:
/// - **Webhook mode**: Posts messages to a Google Chat space via an incoming
///   webhook URL. Simple, no OAuth required, outbound-only.
/// - **Service account mode**: Uses a Google service account to call the
///   Google Chat API for full bidirectional messaging.
pub struct GoogleChatChannel {
    /// Incoming webhook URL for the Google Chat space.
    webhook_url: Option<String>,
    /// Service account JSON key (serialized) for API access.
    service_account: Option<String>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// HTTP client for API calls.
    client: Client,
}

impl GoogleChatChannel {
    pub fn new() -> Self {
        Self {
            webhook_url: None,
            service_account: None,
            enabled: None,
            client: Client::new(),
        }
    }

    /// Create a webhook-only Google Chat channel.
    pub fn with_webhook(webhook_url: String) -> Self {
        Self {
            webhook_url: Some(webhook_url),
            service_account: None,
            enabled: Some(true),
            client: Client::new(),
        }
    }

    /// Create a Google Chat channel with service account credentials.
    pub fn with_service_account(service_account: String) -> Self {
        Self {
            webhook_url: None,
            service_account: Some(service_account),
            enabled: Some(true),
            client: Client::new(),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }
}

#[async_trait]
impl ChannelPlugin for GoogleChatChannel {
    fn id(&self) -> &str {
        "googlechat"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Google Chat".to_string(),
            description: "Google Chat (Workspace) channel via webhook or Chat API".to_string(),
            enabled: self.is_enabled(),
            multi_account: false,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![
            ChannelCapability::SendText,
            ChannelCapability::ReceiveText,
            ChannelCapability::SendMedia,
            ChannelCapability::Groups,
            ChannelCapability::Threads,
            ChannelCapability::Reactions,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        if self.webhook_url.is_some() {
            info!("Google Chat channel starting (webhook mode)");
        } else if self.service_account.is_some() {
            info!("Google Chat channel starting (service account mode)");
            // TODO: Parse the service account JSON and set up OAuth2 token refresh.
        } else {
            warn!("Google Chat channel enabled but no webhook_url or service_account configured");
        }

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("Google Chat channel stopping");
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        // Prefer webhook mode if configured.
        if let Some(webhook_url) = &self.webhook_url {
            return self.send_via_webhook(webhook_url, message).await;
        }

        // Fall back to Chat API with service account.
        if self.service_account.is_some() {
            return self.send_via_api(to, message).await;
        }

        anyhow::bail!("Google Chat: no webhook_url or service_account configured");
    }
}

impl GoogleChatChannel {
    /// Send a message via Google Chat incoming webhook.
    ///
    /// The webhook URL is space-specific; the `to` parameter is ignored
    /// in webhook mode (messages go to the space the webhook belongs to).
    async fn send_via_webhook(&self, webhook_url: &str, message: &str) -> Result<()> {
        let body = serde_json::json!({
            "text": message,
        });

        info!("Google Chat: sending message via webhook");

        let resp = self
            .client
            .post(webhook_url)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Google Chat webhook send failed ({}): {}", status, text);
        }

        Ok(())
    }

    /// Send a message via the Google Chat API (service account auth).
    ///
    /// `to` is a Chat API space name (e.g. `spaces/AAAA...`).
    async fn send_via_api(&self, to: &str, message: &str) -> Result<()> {
        // TODO: Implement OAuth2 token acquisition from service account credentials.
        // Use `https://chat.googleapis.com/v1/{space}/messages` endpoint.

        let url = format!(
            "https://chat.googleapis.com/v1/{}/messages",
            to
        );

        let body = serde_json::json!({
            "text": message,
        });

        info!(space = %to, "Google Chat: sending message via Chat API");

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Google Chat API send failed ({}): {}", status, text);
        }

        Ok(())
    }
}
