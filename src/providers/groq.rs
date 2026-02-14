use super::openai_compat;
use super::*;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use tokio::sync::mpsc;

pub struct GroqProvider {
    api_key: String,
    base_url: String,
    model: String,
    client: Client,
}

impl GroqProvider {
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        Self {
            api_key,
            base_url,
            model,
            client: Client::new(),
        }
    }
}

#[async_trait]
impl ModelProvider for GroqProvider {
    async fn chat(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        openai_compat::openai_compat_chat(
            &self.client,
            &self.base_url,
            &self.api_key,
            request,
            "Groq",
        )
        .await
    }

    async fn stream_chat(&self, request: ProviderRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        openai_compat::openai_compat_stream_chat(
            &self.client,
            &self.base_url,
            &self.api_key,
            request,
            "Groq",
        )
        .await
    }

    fn name(&self) -> &str {
        "groq"
    }
}
