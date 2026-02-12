use super::protocol::*;

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio_tungstenite::{connect_async, tungstenite};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Close code descriptions.
pub fn describe_close_code(code: u16) -> Option<&'static str> {
    match code {
        1000 => Some("normal closure"),
        1006 => Some("abnormal closure (no close frame)"),
        1008 => Some("policy violation"),
        1012 => Some("service restart"),
        _ => None,
    }
}

/// Options for creating a gateway client.
#[derive(Debug, Clone)]
pub struct GatewayClientOptions {
    pub url: String,
    pub token: Option<String>,
    pub password: Option<String>,
    pub instance_id: Option<String>,
    pub client_name: Option<String>,
    pub client_version: Option<String>,
    pub min_protocol: Option<u32>,
    pub max_protocol: Option<u32>,
}

impl Default for GatewayClientOptions {
    fn default() -> Self {
        Self {
            url: "ws://127.0.0.1:18789/ws".to_string(),
            token: None,
            password: None,
            instance_id: None,
            client_name: None,
            client_version: None,
            min_protocol: None,
            max_protocol: None,
        }
    }
}

type PendingMap =
    Arc<RwLock<HashMap<String, oneshot::Sender<Result<serde_json::Value, ProtocolError>>>>>;

/// A client for connecting to the MyLobster gateway.
pub struct GatewayClient {
    opts: GatewayClientOptions,
    tx: Option<mpsc::Sender<String>>,
    pending: PendingMap,
    seq: Arc<AtomicU64>,
    event_tx: mpsc::Sender<EventFrame>,
    event_rx: Option<mpsc::Receiver<EventFrame>>,
    connected: Arc<std::sync::atomic::AtomicBool>,
}

impl GatewayClient {
    /// Create a new gateway client.
    pub fn new(opts: GatewayClientOptions) -> Self {
        let (event_tx, event_rx) = mpsc::channel(256);
        Self {
            opts,
            tx: None,
            pending: Arc::new(RwLock::new(HashMap::new())),
            seq: Arc::new(AtomicU64::new(1)),
            event_tx,
            event_rx: Some(event_rx),
            connected: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Take the event receiver (can only be called once).
    pub fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<EventFrame>> {
        self.event_rx.take()
    }

    /// Connect to the gateway.
    pub async fn connect(&mut self) -> Result<()> {
        let url = if let Some(ref token) = self.opts.token {
            format!("{}?token={}", self.opts.url, token)
        } else {
            self.opts.url.clone()
        };

        info!("Connecting to gateway at {}", self.opts.url);
        let (ws_stream, _) = connect_async(&url).await?;
        let (mut ws_tx, mut ws_rx) = ws_stream.split();

        let (tx, mut rx) = mpsc::channel::<String>(256);
        self.tx = Some(tx);
        self.connected.store(true, Ordering::SeqCst);

        // Spawn writer
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if ws_tx
                    .send(tungstenite::Message::Text(msg.into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        // Spawn reader
        let pending = self.pending.clone();
        let event_tx = self.event_tx.clone();
        let connected = self.connected.clone();

        tokio::spawn(async move {
            while let Some(msg) = ws_rx.next().await {
                match msg {
                    Ok(tungstenite::Message::Text(text)) => {
                        let text_str: &str = &text;
                        if let Ok(frame) = serde_json::from_str::<serde_json::Value>(text_str) {
                            // Check if it's a response (has "id" and ("result" or "error"))
                            if let Some(id) = frame.get("id").and_then(|v| v.as_str()) {
                                if frame.get("result").is_some() || frame.get("error").is_some() {
                                    let mut pending = pending.write().await;
                                    if let Some(sender) = pending.remove(id) {
                                        if let Some(error) = frame.get("error") {
                                            let proto_error: ProtocolError =
                                                serde_json::from_value(error.clone()).unwrap_or(
                                                    ProtocolError {
                                                        code: -1,
                                                        message: "Unknown error".to_string(),
                                                        data: None,
                                                    },
                                                );
                                            let _ = sender.send(Err(proto_error));
                                        } else {
                                            let result = frame
                                                .get("result")
                                                .cloned()
                                                .unwrap_or(serde_json::Value::Null);
                                            let _ = sender.send(Ok(result));
                                        }
                                    }
                                    continue;
                                }
                            }

                            // Check if it's an event
                            if let Some(event_name) = frame.get("event").and_then(|v| v.as_str()) {
                                let event = EventFrame {
                                    event: event_name.to_string(),
                                    data: frame.get("data").cloned(),
                                    seq: frame.get("seq").and_then(|v| v.as_u64()),
                                };
                                let _ = event_tx.send(event).await;
                            }
                        }
                    }
                    Ok(tungstenite::Message::Close(_)) => {
                        info!("Gateway connection closed");
                        break;
                    }
                    Err(e) => {
                        error!("Gateway WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
            connected.store(false, Ordering::SeqCst);
        });

        info!("Connected to gateway");
        Ok(())
    }

    /// Send a request to the gateway and wait for response.
    pub async fn request<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<T> {
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Not connected"))?;

        let id = Uuid::new_v4().to_string();
        let (resp_tx, resp_rx) = oneshot::channel();

        {
            let mut pending = self.pending.write().await;
            pending.insert(id.clone(), resp_tx);
        }

        let request = RequestFrame {
            id,
            method: method.to_string(),
            params,
            seq: Some(self.seq.fetch_add(1, Ordering::SeqCst)),
        };

        let json = serde_json::to_string(&request)?;
        tx.send(json).await?;

        let result = resp_rx.await??;
        let typed: T = serde_json::from_value(result)?;
        Ok(typed)
    }

    /// Check if client is connected.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Disconnect from the gateway.
    pub fn disconnect(&mut self) {
        self.tx = None;
        self.connected.store(false, Ordering::SeqCst);
    }
}
