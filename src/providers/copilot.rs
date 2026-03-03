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
