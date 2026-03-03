use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use tracing::{info, warn};

// ============================================================================
// Microsoft Teams Channel Implementation
// ============================================================================

/// Microsoft Teams channel integration using the Bot Framework REST API.
///
/// Communicates with Teams via the Azure Bot Service / Bot Framework v3 API.
/// Messages are sent using the Bot Connector REST API at
/// `https://smba.trafficmanager.net/` (or the `serviceUrl` from incoming
/// activities).
///
/// Configuration requires an Azure Bot registration with app ID and password.
pub struct TeamsChannel {
    /// Azure Bot app ID (client ID from Azure AD app registration).
    app_id: Option<String>,
    /// Azure Bot app password (client secret).
    app_password: Option<String>,
    /// Bot Framework service URL (set from incoming activity `serviceUrl`).
    service_url: Option<String>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// HTTP client for Bot Framework API calls.
    client: Client,
}

impl TeamsChannel {
    pub fn new() -> Self {
        Self {
            app_id: None,
            app_password: None,
            service_url: None,
            enabled: None,
            client: Client::new(),
        }
    }

    /// Create a configured Teams channel.
    pub fn with_config(app_id: String, app_password: String) -> Self {
        Self {
            app_id: Some(app_id),
            app_password: Some(app_password),
            service_url: None,
            enabled: Some(true),
            client: Client::new(),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    /// Acquire an OAuth2 token from Azure AD for the Bot Framework.
    ///
    /// Calls `https://login.microsoftonline.com/botframework.com/oauth2/v2.0/token`
    /// with client credentials grant.
    async fn acquire_token(&self) -> Result<String> {
        let app_id = self
            .app_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Teams app_id not configured"))?;
        let app_password = self
            .app_password
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Teams app_password not configured"))?;

        let token_url =
            "https://login.microsoftonline.com/botframework.com/oauth2/v2.0/token";

        let resp = self
            .client
            .post(token_url)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", app_id),
                ("client_secret", app_password),
                (
                    "scope",
                    "https://api.botframework.com/.default",
                ),
            ])
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Teams OAuth2 token request failed ({}): {}", status, text);
        }

        let body: serde_json::Value = resp.json().await?;
        let token = body["access_token"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Teams: no access_token in OAuth2 response"))?
            .to_string();

        Ok(token)
    }
}

#[async_trait]
impl ChannelPlugin for TeamsChannel {
    fn id(&self) -> &str {
        "teams"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Microsoft Teams".to_string(),
            description: "Microsoft Teams channel via Bot Framework REST API".to_string(),
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
            ChannelCapability::EditMessage,
            ChannelCapability::DeleteMessage,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        if self.app_id.is_none() || self.app_password.is_none() {
            warn!("Teams channel enabled but app_id or app_password not configured");
            return Ok(());
        }

        info!("Microsoft Teams channel starting");

        // Validate credentials by acquiring an initial token.
        match self.acquire_token().await {
            Ok(_) => info!("Teams: OAuth2 token acquired successfully"),
            Err(e) => warn!("Teams: failed to acquire initial OAuth2 token: {}", e),
        }

        // TODO: Set up an HTTP endpoint to receive incoming Bot Framework activities.
        // The Bot Framework sends POST requests to the bot's messaging endpoint.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("Microsoft Teams channel stopping");
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        let service_url = self
            .service_url
            .as_deref()
            .unwrap_or("https://smba.trafficmanager.net/amer/");

        let token = self.acquire_token().await?;

        // `to` is a conversation ID from the Bot Framework activity.
        // Format: `POST {serviceUrl}/v3/conversations/{conversationId}/activities`
        let url = format!(
            "{}v3/conversations/{}/activities",
            service_url.trim_end_matches('/').to_string() + "/",
            to,
        );

        let body = serde_json::json!({
            "type": "message",
            "text": message,
        });

        info!(conversation_id = %to, "Teams: sending message");

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
            anyhow::bail!("Teams send failed ({}): {}", status, text);
        }

        Ok(())
    }
}
