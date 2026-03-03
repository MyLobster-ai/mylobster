use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use tracing::{info, warn};

// ============================================================================
// Feishu / Lark Channel Implementation
// ============================================================================

/// Feishu (Lark) channel integration via the Feishu Open Platform API.
///
/// Feishu is the enterprise collaboration platform by ByteDance (known as
/// Lark internationally). This channel communicates via the Feishu Bot API
/// to send and receive messages.
///
/// API docs: <https://open.feishu.cn/document/server-docs/im-v1/message/create>
///
/// Authentication uses an app_id + app_secret to obtain a `tenant_access_token`
/// via `POST https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal`.
pub struct FeishuChannel {
    /// Feishu app ID from the Feishu Open Platform developer console.
    app_id: Option<String>,
    /// Feishu app secret.
    app_secret: Option<String>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// HTTP client for API calls.
    client: Client,
}

/// Feishu API base URL.
const FEISHU_API_BASE: &str = "https://open.feishu.cn/open-apis";

impl FeishuChannel {
    pub fn new() -> Self {
        Self {
            app_id: None,
            app_secret: None,
            enabled: None,
            client: Client::new(),
        }
    }

    /// Create a configured Feishu channel.
    pub fn with_config(app_id: String, app_secret: String) -> Self {
        Self {
            app_id: Some(app_id),
            app_secret: Some(app_secret),
            enabled: Some(true),
            client: Client::new(),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    /// Acquire a tenant_access_token from the Feishu Open Platform.
    async fn acquire_tenant_token(&self) -> Result<String> {
        let app_id = self
            .app_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Feishu app_id not configured"))?;
        let app_secret = self
            .app_secret
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Feishu app_secret not configured"))?;

        let url = format!(
            "{}/auth/v3/tenant_access_token/internal",
            FEISHU_API_BASE,
        );

        let body = serde_json::json!({
            "app_id": app_id,
            "app_secret": app_secret,
        });

        let resp = self.client.post(&url).json(&body).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Feishu tenant_access_token request failed ({}): {}",
                status,
                text
            );
        }

        let result: serde_json::Value = resp.json().await?;
        let code = result["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            let msg = result["msg"].as_str().unwrap_or("unknown error");
            anyhow::bail!("Feishu token error (code {}): {}", code, msg);
        }

        let token = result["tenant_access_token"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Feishu: no tenant_access_token in response"))?
            .to_string();

        Ok(token)
    }
}

#[async_trait]
impl ChannelPlugin for FeishuChannel {
    fn id(&self) -> &str {
        "feishu"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Feishu".to_string(),
            description: "Feishu (Lark) channel via Open Platform API".to_string(),
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

        if self.app_id.is_none() || self.app_secret.is_none() {
            warn!("Feishu channel enabled but app_id or app_secret not configured");
            return Ok(());
        }

        info!("Feishu channel starting");

        // Verify credentials by acquiring an initial token.
        match self.acquire_tenant_token().await {
            Ok(_) => info!("Feishu: tenant_access_token acquired successfully"),
            Err(e) => warn!("Feishu: failed to acquire initial token: {}", e),
        }

        // TODO: Register an event subscription endpoint to receive incoming
        // messages. Feishu sends events via HTTP POST to the app's event URL.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("Feishu channel stopping");
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        let token = self.acquire_tenant_token().await?;

        // `to` is a Feishu chat_id (group) or open_id (user).
        // We default to sending to a chat_id. The receive_id_type determines
        // whether `to` is a chat_id, open_id, user_id, or union_id.
        let url = format!(
            "{}/im/v1/messages?receive_id_type=chat_id",
            FEISHU_API_BASE,
        );

        let body = serde_json::json!({
            "receive_id": to,
            "msg_type": "text",
            "content": serde_json::json!({ "text": message }).to_string(),
        });

        info!(chat_id = %to, "Feishu: sending message");

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
            anyhow::bail!("Feishu send message failed ({}): {}", status, text);
        }

        // Check the Feishu API-level error code.
        let result: serde_json::Value = resp.json().await?;
        let code = result["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            let msg = result["msg"].as_str().unwrap_or("unknown error");
            anyhow::bail!("Feishu send error (code {}): {}", code, msg);
        }

        Ok(())
    }
}
