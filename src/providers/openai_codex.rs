//! OpenAI Codex WebSocket transport (v2026.2.26).
//!
//! Implements a WebSocket-first transport for the openai-codex provider,
//! with SSE fallback via the existing `openai_compat` functions.

use super::*;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// OpenAI Codex provider with WebSocket-first transport.
pub struct OpenAiCodexProvider {
    api_key: String,
    model: String,
    base_url: String,
    client: Client,
    /// Whether to prefer WebSocket over SSE.
    prefer_ws: bool,
}

impl OpenAiCodexProvider {
    /// Create a new Codex provider.
    pub fn new(api_key: String, model: String, base_url: Option<String>) -> Self {
        Self {
            api_key,
            model,
            base_url: base_url.unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            client: Client::new(),
            prefer_ws: true,
        }
    }

    /// Create with SSE-only mode (WebSocket disabled).
    pub fn new_sse_only(api_key: String, model: String, base_url: Option<String>) -> Self {
        let mut provider = Self::new(api_key, model, base_url);
        provider.prefer_ws = false;
        provider
    }

    /// Attempt WebSocket connection for streaming.
    async fn stream_via_ws(
        &self,
        request: ProviderRequest,
    ) -> Result<mpsc::Receiver<StreamEvent>> {
        // Build WS URL from base URL
        let ws_url = self
            .base_url
            .replace("https://", "wss://")
            .replace("http://", "ws://");
        let ws_url = format!("{}/chat/completions", ws_url);

        debug!("Attempting Codex WebSocket connection to {}", ws_url);

        let ws_request = tokio_tungstenite::tungstenite::http::Request::builder()
            .uri(&ws_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .body(())
            .map_err(|e| anyhow::anyhow!("Failed to build WS request: {}", e))?;

        let (ws_stream, _) = tokio_tungstenite::connect_async(ws_request)
            .await
            .map_err(|e| anyhow::anyhow!("WebSocket connect failed: {}", e))?;

        use futures::{SinkExt, StreamExt};
        let (mut ws_tx, mut ws_rx) = ws_stream.split();

        // Send the chat completion request as JSON
        let body = super::openai_compat::build_request(request, true);
        ws_tx
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&body)?.into(),
            ))
            .await?;

        let (tx, rx) = mpsc::channel(256);

        // Spawn a task to read frames and forward as StreamEvents
        tokio::spawn(async move {
            while let Some(msg) = ws_rx.next().await {
                match msg {
                    Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                        let text_str: &str = &text;
                        if text_str.starts_with("data: [DONE]") || text_str == "[DONE]" {
                            break;
                        }
                        let data = text_str.strip_prefix("data: ").unwrap_or(text_str);
                        if let Ok(chunk) = serde_json::from_str::<serde_json::Value>(data) {
                            if let Some(delta_text) = chunk
                                .pointer("/choices/0/delta/content")
                                .and_then(|v| v.as_str())
                            {
                                if !delta_text.is_empty() {
                                    let _ = tx
                                        .send(StreamEvent::Delta(delta_text.to_string()))
                                        .await;
                                }
                            }
                        }
                    }
                    Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => break,
                    Err(e) => {
                        warn!("Codex WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
            let _ = tx
                .send(StreamEvent::Done(crate::gateway::TokenUsage::default()))
                .await;
        });

        Ok(rx)
    }
}

#[async_trait]
impl ModelProvider for OpenAiCodexProvider {
    async fn chat(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        super::openai_compat::openai_compat_chat(
            &self.client,
            &self.base_url,
            &self.api_key,
            request,
            "openai-codex",
        )
        .await
    }

    async fn stream_chat(&self, request: ProviderRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        if self.prefer_ws {
            match self.stream_via_ws(request.clone()).await {
                Ok(rx) => return Ok(rx),
                Err(e) => {
                    warn!("Codex WebSocket streaming failed, falling back to SSE: {}", e);
                }
            }
        }

        // SSE fallback
        super::openai_compat::openai_compat_stream_chat(
            &self.client,
            &self.base_url,
            &self.api_key,
            request,
            "openai-codex",
        )
        .await
    }

    fn name(&self) -> &str {
        "openai-codex"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_provider_default_url() {
        let provider = OpenAiCodexProvider::new(
            "key".to_string(),
            "codex-latest".to_string(),
            None,
        );
        assert!(provider.prefer_ws);
        assert_eq!(provider.base_url, "https://api.openai.com/v1");
    }

    #[test]
    fn codex_provider_sse_only() {
        let provider = OpenAiCodexProvider::new_sse_only(
            "key".to_string(),
            "codex-latest".to_string(),
            None,
        );
        assert!(!provider.prefer_ws);
    }

    #[test]
    fn codex_provider_custom_url() {
        let provider = OpenAiCodexProvider::new(
            "key".to_string(),
            "codex-latest".to_string(),
            Some("https://custom.api/v1".to_string()),
        );
        assert_eq!(provider.base_url, "https://custom.api/v1");
    }
}
