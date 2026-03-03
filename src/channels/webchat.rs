use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

// ============================================================================
// WebChat Channel Implementation
// ============================================================================

/// Built-in WebChat channel using axum WebSocket.
///
/// Provides a real-time chat widget that can be embedded in web pages.
/// Unlike REST-based channels, WebChat runs entirely within the gateway
/// process using axum's WebSocket support.
///
/// Connected clients are tracked in memory. Messages can be sent to a
/// specific client by their session ID, or broadcast to all connected
/// clients.
///
/// This channel does not require any external service or API keys.
pub struct WebChatChannel {
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// Bind address for the WebChat endpoint (if separate from main gateway).
    bind_address: Option<String>,
    /// Port for the WebChat endpoint (if separate from main gateway).
    port: Option<u16>,
    /// Maximum number of concurrent WebSocket connections.
    max_connections: Option<u32>,
    /// Connected clients keyed by session ID.
    clients: Arc<RwLock<HashMap<String, WebChatClient>>>,
}

/// Represents a connected WebChat client.
struct WebChatClient {
    /// Unique session identifier.
    session_id: String,
    /// Sender half of the WebSocket channel for pushing messages.
    tx: tokio::sync::mpsc::UnboundedSender<String>,
}

impl WebChatChannel {
    pub fn new() -> Self {
        Self {
            enabled: None,
            bind_address: None,
            port: None,
            max_connections: None,
            clients: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a configured WebChat channel.
    pub fn with_config(bind_address: String, port: u16) -> Self {
        Self {
            enabled: Some(true),
            bind_address: Some(bind_address),
            port: Some(port),
            max_connections: Some(100),
            clients: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    /// Register a new WebChat client connection.
    pub async fn register_client(
        &self,
        session_id: String,
        tx: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let max = self.max_connections.unwrap_or(100) as usize;
        let mut clients = self.clients.write().await;

        if clients.len() >= max {
            anyhow::bail!(
                "WebChat: max connections ({}) reached, rejecting client {}",
                max,
                session_id
            );
        }

        clients.insert(
            session_id.clone(),
            WebChatClient {
                session_id,
                tx,
            },
        );

        Ok(())
    }

    /// Remove a disconnected client.
    pub async fn unregister_client(&self, session_id: &str) {
        self.clients.write().await.remove(session_id);
    }

    /// Get the number of currently connected clients.
    pub async fn connected_count(&self) -> usize {
        self.clients.read().await.len()
    }

    /// Broadcast a message to all connected clients.
    pub async fn broadcast(&self, message: &str) {
        let clients = self.clients.read().await;
        for (id, client) in clients.iter() {
            if client.tx.send(message.to_string()).is_err() {
                warn!(session_id = %id, "WebChat: failed to send to client (disconnected?)");
            }
        }
    }
}

#[async_trait]
impl ChannelPlugin for WebChatChannel {
    fn id(&self) -> &str {
        "webchat"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "WebChat".to_string(),
            description: "Built-in WebChat channel using axum WebSocket".to_string(),
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
            ChannelCapability::TypingIndicators,
            ChannelCapability::ReadReceipts,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        let bind = self.bind_address.as_deref().unwrap_or("0.0.0.0");
        let port = self.port.unwrap_or(0);
        let max_conn = self.max_connections.unwrap_or(100);

        info!(
            bind = %bind,
            port = %port,
            max_connections = %max_conn,
            "WebChat channel starting"
        );

        // The WebChat WebSocket endpoint is typically registered on the main
        // gateway router (e.g. at `/ws/webchat`). If a separate bind address
        // or port is configured, a dedicated axum listener would be spawned here.
        //
        // TODO: Register the WebSocket upgrade handler on the gateway router:
        //   router.route("/ws/webchat", get(webchat_ws_handler))
        //
        // The handler would:
        // 1. Upgrade the HTTP connection to WebSocket.
        // 2. Create an mpsc channel for outbound messages.
        // 3. Register the client via `register_client()`.
        // 4. Spawn a read loop for incoming messages.
        // 5. Spawn a write loop that forwards messages from the mpsc channel.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            let count = self.connected_count().await;
            info!(
                connected_clients = %count,
                "WebChat channel stopping"
            );

            // Close all client connections.
            let mut clients = self.clients.write().await;
            clients.clear();
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        let clients = self.clients.read().await;

        // `to` is a session ID. If `to` is "*", broadcast to all clients.
        if to == "*" {
            drop(clients);
            self.broadcast(message).await;
            return Ok(());
        }

        match clients.get(to) {
            Some(client) => {
                let payload = serde_json::json!({
                    "type": "message",
                    "session_id": to,
                    "content": message,
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                });

                client
                    .tx
                    .send(payload.to_string())
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "WebChat: client {} disconnected, cannot send message",
                            to
                        )
                    })?;

                info!(session_id = %to, "WebChat: message sent to client");
                Ok(())
            }
            None => {
                anyhow::bail!("WebChat: no client with session_id '{}'", to);
            }
        }
    }
}
