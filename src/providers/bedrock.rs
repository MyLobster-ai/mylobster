use super::*;
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

pub struct BedrockProvider {
    region: String,
    model: String,
}

impl BedrockProvider {
    pub fn new(region: String, model: String) -> Self {
        Self { region, model }
    }
}

#[async_trait]
impl ModelProvider for BedrockProvider {
    async fn chat(&self, _request: ProviderRequest) -> Result<ProviderResponse> {
        anyhow::bail!(
            "AWS Bedrock provider (region={}, model={}) is not yet implemented",
            self.region,
            self.model
        )
    }

    async fn stream_chat(&self, _request: ProviderRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        anyhow::bail!(
            "AWS Bedrock provider (region={}, model={}) is not yet implemented",
            self.region,
            self.model
        )
    }

    fn name(&self) -> &str {
        "bedrock"
    }
}
