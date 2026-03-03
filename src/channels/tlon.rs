use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

// ============================================================================
// Tlon / Urbit Channel Implementation
// ============================================================================

/// Tlon (Urbit) channel integration via the Urbit HTTP API.
///
/// Connects to an Urbit ship's Eyre HTTP interface to send and receive
/// messages through Tlon (the Urbit messaging app, formerly Landscape).
///
/// Authentication uses the ship's `+code` (web login code) to obtain
/// a session cookie from `/~/login`. The cookie value is stored in memory
/// and included in subsequent requests via the `Cookie` header.
///
/// Messages are sent by poking the `%chat-store` or `%graph-store` agent
/// via the Eyre API at `/~/channel/<uid>`.
///
/// Reference: <https://developers.urbit.org/reference/arvo/eyre/guide>
pub struct TlonChannel {
    /// Ship URL (e.g. `http://localhost:8080` or `https://ship.urbit.org`).
    ship_url: Option<String>,
    /// Ship name (e.g. `~zod`, `~sampel-palnet`).
    ship_name: Option<String>,
    /// Ship +code for authentication.
    ship_code: Option<String>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// HTTP client for API calls.
    client: Client,
    /// Session cookie obtained after authentication.
    session_cookie: Arc<RwLock<Option<String>>>,
}

impl TlonChannel {
    pub fn new() -> Self {
        Self {
            ship_url: None,
            ship_name: None,
            ship_code: None,
            enabled: None,
            client: Client::new(),
            session_cookie: Arc::new(RwLock::new(None)),
        }
    }

    /// Create a configured Tlon channel.
    pub fn with_config(ship_url: String, ship_name: String, ship_code: String) -> Self {
        Self {
            ship_url: Some(ship_url),
            ship_name: Some(ship_name),
            ship_code: Some(ship_code),
            enabled: Some(true),
            client: Client::new(),
            session_cookie: Arc::new(RwLock::new(None)),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    /// Authenticate with the Urbit ship by posting the +code to /~/login.
    ///
    /// Extracts the `urbitsession` cookie from the response `Set-Cookie` header
    /// and stores it for subsequent API calls.
    async fn authenticate(&self) -> Result<()> {
        let ship_url = self
            .ship_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Tlon ship_url not configured"))?;

        let ship_code = self
            .ship_code
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Tlon ship_code not configured"))?;

        let login_url = format!("{}/~/login", ship_url.trim_end_matches('/'));

        let resp = self
            .client
            .post(&login_url)
            .body(format!("password={}", ship_code))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Tlon: login failed ({}): {}", status, text);
        }

        // Extract the session cookie from Set-Cookie header.
        if let Some(set_cookie) = resp.headers().get("set-cookie") {
            if let Ok(cookie_str) = set_cookie.to_str() {
                // The cookie format is typically: urbitsession=0v...; Path=/; ...
                let cookie_value = cookie_str
                    .split(';')
                    .next()
                    .unwrap_or(cookie_str)
                    .to_string();
                *self.session_cookie.write().await = Some(cookie_value);
            }
        }

        Ok(())
    }

    /// Get the session cookie header value, if authenticated.
    async fn get_cookie(&self) -> Option<String> {
        self.session_cookie.read().await.clone()
    }
}

#[async_trait]
impl ChannelPlugin for TlonChannel {
    fn id(&self) -> &str {
        "tlon"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Tlon".to_string(),
            description: "Tlon (Urbit) channel via ship HTTP API".to_string(),
            enabled: self.is_enabled(),
            multi_account: false,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![
            ChannelCapability::SendText,
            ChannelCapability::ReceiveText,
            ChannelCapability::Groups,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        let ship_url = match &self.ship_url {
            Some(url) => url.clone(),
            None => {
                warn!("Tlon channel enabled but no ship_url configured");
                return Ok(());
            }
        };

        let ship_name = self.ship_name.as_deref().unwrap_or("~unknown");

        if self.ship_code.is_none() {
            warn!("Tlon channel enabled but no ship_code configured");
            return Ok(());
        }

        info!(
            ship_url = %ship_url,
            ship_name = %ship_name,
            "Tlon channel starting"
        );

        // Authenticate with the ship.
        match self.authenticate().await {
            Ok(()) => info!("Tlon: authenticated with ship {}", ship_name),
            Err(e) => warn!("Tlon: authentication failed: {}", e),
        }

        // TODO: Open an SSE channel at /~/channel/<uid> to receive events.
        // Subscribe to the chat-store or graph-store for incoming messages.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("Tlon channel stopping");
            // Clear session cookie.
            *self.session_cookie.write().await = None;
            // TODO: Close the SSE channel and unsubscribe from agents.
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        let ship_url = self
            .ship_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Tlon ship_url not configured"))?;

        let ship_name = self
            .ship_name
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Tlon ship_name not configured"))?;

        let cookie = self
            .get_cookie()
            .await
            .ok_or_else(|| anyhow::anyhow!("Tlon: not authenticated — call start_account first"))?;

        // `to` is a Tlon channel path (e.g. "chat/~sampel-palnet/general").
        let channel_uid = uuid::Uuid::new_v4().to_string();
        let url = format!(
            "{}/~/channel/{}",
            ship_url.trim_end_matches('/'),
            channel_uid,
        );

        // Construct a poke to the graph-store agent to add a message node.
        let now_ms = chrono::Utc::now().timestamp_millis();
        let node_index = format!("/{}", now_ms);
        let ship_bare = ship_name.trim_start_matches('~');

        let body = serde_json::json!([
            {
                "id": 1,
                "action": "poke",
                "ship": ship_bare,
                "app": "graph-push-hook",
                "mark": "graph-update-3",
                "json": {
                    "add-nodes": {
                        "resource": {
                            "ship": ship_bare,
                            "name": to,
                        },
                        "nodes": {
                            (node_index.clone()): {
                                "post": {
                                    "author": ship_bare,
                                    "index": node_index,
                                    "time-sent": now_ms,
                                    "contents": [{ "text": message }],
                                    "hash": serde_json::Value::Null,
                                    "signatures": [],
                                },
                                "children": serde_json::Value::Null,
                            }
                        }
                    }
                }
            }
        ]);

        info!(channel_path = %to, "Tlon: sending message");

        let resp = self
            .client
            .put(&url)
            .header("Cookie", &cookie)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Tlon send message failed ({}): {}", status, text);
        }

        Ok(())
    }
}
