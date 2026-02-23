use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::config::Config;
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use subtle::ConstantTimeEq;
use tracing::{debug, info, warn};

/// Synology Chat channel integration.
///
/// Communicates with Synology Chat via incoming/outgoing webhooks.
/// Outbound messages are sent as `POST` with `payload={"text":"..."}`.
/// Inbound messages are received as form-urlencoded webhook POSTs.
pub struct SynologyChatChannel {
    enabled: bool,
    token: Option<String>,
    incoming_url: Option<String>,
    bot_name: String,
    dm_policy: crate::config::DmPolicy,
    allowed_user_ids: Vec<String>,
    rate_limit_per_minute: u32,
    client: Client,
}

impl SynologyChatChannel {
    pub fn new(config: &Config) -> Self {
        let chat_config = config.channels.synology_chat.as_ref();
        let account = chat_config.map(|c| &c.default_account);

        let enabled = account
            .and_then(|a| a.enabled)
            .unwrap_or(false);

        let token = account.and_then(|a| a.token.clone());
        let incoming_url = account.and_then(|a| a.incoming_url.clone());
        let bot_name = account
            .and_then(|a| a.bot_name.clone())
            .unwrap_or_else(|| "MyLobster".to_string());
        let dm_policy = account
            .and_then(|a| a.dm_policy)
            .unwrap_or(crate::config::DmPolicy::Open);
        let allowed_user_ids = account
            .and_then(|a| a.allowed_user_ids.clone())
            .unwrap_or_default();
        let rate_limit_per_minute = account
            .and_then(|a| a.rate_limit_per_minute)
            .unwrap_or(30);

        let allow_insecure = account
            .and_then(|a| a.allow_insecure_ssl)
            .unwrap_or(false);

        let client = Client::builder()
            .danger_accept_invalid_certs(allow_insecure)
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            enabled,
            token,
            incoming_url,
            bot_name,
            dm_policy,
            allowed_user_ids,
            rate_limit_per_minute,
            client,
        }
    }

    /// Validate an inbound webhook token using constant-time comparison.
    fn validate_token(&self, received: &str) -> bool {
        match &self.token {
            Some(expected) => {
                let expected_bytes = expected.as_bytes();
                let received_bytes = received.as_bytes();
                // Constant-time comparison to prevent timing attacks
                expected_bytes.ct_eq(received_bytes).into()
            }
            None => false,
        }
    }

    /// Sanitize inbound message text by stripping dangerous patterns.
    fn sanitize_input(text: &str) -> String {
        // Strip potential injection patterns
        text.replace('\0', "")
            .replace('\r', "")
            // Limit length to prevent abuse
            .chars()
            .take(4096)
            .collect()
    }
}

#[async_trait]
impl ChannelPlugin for SynologyChatChannel {
    fn id(&self) -> &str {
        "synology_chat"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Synology Chat".to_string(),
            description: "Synology Chat NAS messaging integration via webhooks".to_string(),
            enabled: self.enabled,
            multi_account: true,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![
            ChannelCapability::SendText,
            ChannelCapability::ReceiveText,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        if self.token.is_none() {
            warn!("Synology Chat: no token configured, webhook validation will reject all messages");
        }
        if self.incoming_url.is_none() {
            warn!("Synology Chat: no incoming_url configured, outbound messages disabled");
        }

        info!(
            bot_name = %self.bot_name,
            dm_policy = ?self.dm_policy,
            "Synology Chat channel started"
        );
        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        info!("Synology Chat channel stopped");
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        let incoming_url = self.incoming_url.as_ref().ok_or_else(|| {
            anyhow::anyhow!("Synology Chat: no incoming_url configured for outbound messages")
        })?;

        // Build payload per Synology Chat incoming webhook format
        let user_ids: Vec<serde_json::Value> = if to.is_empty() {
            vec![]
        } else {
            to.split(',')
                .map(|id| serde_json::Value::Number(id.trim().parse::<i64>().unwrap_or(0).into()))
                .collect()
        };

        let payload = serde_json::json!({
            "text": message,
            "user_ids": user_ids,
        });

        let payload_str = serde_json::to_string(&payload)?;

        debug!(url = %incoming_url, "Sending Synology Chat message");

        let resp = self
            .client
            .post(incoming_url)
            .form(&[("payload", &payload_str)])
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Synology Chat: send failed with status {}: {}",
                status,
                body
            );
        }

        Ok(())
    }
}

/// Send a standalone message via Synology Chat (used by the channel dispatch in mod.rs).
pub async fn send_message(config: &Config, to: &str, message: &str) -> Result<()> {
    let channel = SynologyChatChannel::new(config);
    channel.send_message(to, message).await
}
