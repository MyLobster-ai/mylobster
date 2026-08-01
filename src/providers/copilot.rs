//! GitHub Copilot provider.
//!
//! Uses OAuth token exchange to get a session token, then calls the
//! Copilot chat completions API (OpenAI-compatible format with dynamic base URL).

use super::openai_compat;
use super::*;
use anyhow::Result;
use async_trait::async_trait;
use parking_lot::RwLock;
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::mpsc;

const TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const DEFAULT_BASE_URL: &str = "https://api.githubcopilot.com";

/// Copilot token-exchange identity used for image/vision requests
/// (v2026.6.x: image support needs the `vscode-chat` identity).
pub const COPILOT_VISION_IDENTITY: &str = "vscode-chat";

/// Copilot Opus 4.8 caps (v2026.7.1 catalog refresh: Opus 4.8 with the 1M
/// context window is available through Copilot).
pub const COPILOT_OPUS_4_8_CONTEXT_WINDOW: u64 = 1_048_576;

/// Fetch the live Copilot model catalog from `{base}/models` with a session
/// bearer (v2026.5.x: catalogs refresh from the endpoint instead of static
/// lists). Returns model ids; malformed payloads are provider-owned errors.
pub async fn copilot_fetch_models(
    client: &Client,
    base_url: &str,
    session_token: &str,
) -> Result<Vec<String>> {
    let resp = client
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .header("Authorization", format!("Bearer {}", session_token))
        .header("Editor-Version", "vscode/1.99")
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        anyhow::bail!("Copilot /models failed ({})", status);
    }
    let payload = super::read_json_bounded(
        resp,
        super::DEFAULT_PROVIDER_BODY_LIMIT_BYTES,
        "Copilot models",
    )
    .await?;
    let rows = payload
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| anyhow::anyhow!("Copilot models: malformed JSON response"))?;
    Ok(rows
        .iter()
        .filter_map(|row| row.get("id").and_then(|id| id.as_str()))
        .map(|id| id.to_string())
        .collect())
}

#[derive(Debug, Deserialize)]
struct CopilotToken {
    token: String,
    expires_at: i64,
    #[serde(default)]
    endpoints: Option<CopilotEndpoints>,
}

#[derive(Debug, Deserialize)]
struct CopilotEndpoints {
    api: Option<String>,
}

struct CachedToken {
    token: String,
    base_url: String,
    expires_at: i64,
}

pub struct CopilotProvider {
    github_token: String,
    model: String,
    client: Client,
    cached: Arc<RwLock<Option<CachedToken>>>,
}

impl CopilotProvider {
    pub fn new(github_token: String, model: String) -> Self {
        Self {
            github_token,
            model,
            client: Client::new(),
            cached: Arc::new(RwLock::new(None)),
        }
    }

    async fn get_session_token(&self) -> Result<(String, String)> {
        // Check cache
        {
            let cached = self.cached.read();
            if let Some(ref ct) = *cached {
                let now = chrono::Utc::now().timestamp();
                if ct.expires_at > now + 60 {
                    return Ok((ct.token.clone(), ct.base_url.clone()));
                }
            }
        }

        // Exchange GitHub token for Copilot session token
        let resp = self
            .client
            .get(TOKEN_URL)
            .header("Authorization", format!("token {}", self.github_token))
            .header("User-Agent", "MyLobster-Agent/1.0")
            .header("Accept", "application/json")
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitHub Copilot token exchange failed ({}): {}", status, text);
        }

        let ct: CopilotToken = resp.json().await?;
        let base_url = ct
            .endpoints
            .as_ref()
            .and_then(|e| e.api.clone())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        let token = ct.token.clone();
        let url = base_url.clone();

        // Cache the token
        {
            let mut cached = self.cached.write();
            *cached = Some(CachedToken {
                token: ct.token,
                base_url,
                expires_at: ct.expires_at,
            });
        }

        Ok((token, url))
    }
}

#[async_trait]
impl ModelProvider for CopilotProvider {
    async fn chat(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        let (token, base_url) = self.get_session_token().await?;
        openai_compat::openai_compat_chat(&self.client, &base_url, &token, request, "Copilot")
            .await
    }

    async fn stream_chat(&self, request: ProviderRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        let (token, base_url) = self.get_session_token().await?;
        openai_compat::openai_compat_stream_chat(
            &self.client,
            &base_url,
            &token,
            request,
            "Copilot",
        )
        .await
    }

    fn name(&self) -> &str {
        "copilot"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn vision_identity_and_opus_caps() {
        assert_eq!(COPILOT_VISION_IDENTITY, "vscode-chat");
        assert_eq!(COPILOT_OPUS_4_8_CONTEXT_WINDOW, 1_048_576);
    }

    #[tokio::test]
    async fn fetch_models_parses_catalog_rows() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"id": "gpt-5.5"},
                    {"id": "claude-opus-4.8"},
                    {"no_id": true}
                ]
            })))
            .mount(&server)
            .await;
        let client = Client::new();
        let models = copilot_fetch_models(&client, &server.uri(), "sess")
            .await
            .unwrap();
        assert_eq!(models, vec!["gpt-5.5", "claude-opus-4.8"]);
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(
            reqs[0].headers.get("authorization").unwrap().to_str().unwrap(),
            "Bearer sess"
        );
    }

    #[tokio::test]
    async fn fetch_models_rejects_malformed_payload() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"nope": 1})))
            .mount(&server)
            .await;
        let client = Client::new();
        let err = copilot_fetch_models(&Client::new(), &server.uri(), "sess")
            .await
            .unwrap_err();
        let _ = client;
        assert!(err.to_string().contains("malformed"));
    }
}
