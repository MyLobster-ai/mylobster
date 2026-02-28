mod anthropic;
mod bedrock;
mod gemini;
mod groq;
mod mistral;
mod ollama;
mod openai;
pub(crate) mod openai_codex;
pub(crate) mod openai_compat;

use crate::config::Config;
use crate::gateway::TokenUsage;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

// ============================================================================
// Provider Types
// ============================================================================

/// A message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderMessage {
    pub role: String,
    pub content: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<serde_json::Value>>,
}

/// A request to a model provider.
#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub model: String,
    pub messages: Vec<ProviderMessage>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub stream: bool,
    pub tools: Option<Vec<serde_json::Value>>,
    pub tool_choice: Option<serde_json::Value>,
}

/// A response from a model provider.
#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<String>,
    pub usage: TokenUsage,
}

impl ProviderResponse {
    pub fn content_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

/// A content block in a response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentBlock {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    Image {
        media_type: String,
        data: String,
    },
}

/// Events streamed from a provider.
pub enum StreamEvent {
    Delta(String),
    ToolCall(serde_json::Value),
    Done(TokenUsage),
    Error(String),
}

// ============================================================================
// Provider Trait
// ============================================================================

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn chat(&self, request: ProviderRequest) -> Result<ProviderResponse>;
    async fn stream_chat(&self, request: ProviderRequest) -> Result<mpsc::Receiver<StreamEvent>>;
    fn name(&self) -> &str;
}

// ============================================================================
// Provider Resolution
// ============================================================================

pub fn resolve_provider(config: &Config, model: &str) -> Result<Box<dyn ModelProvider>> {
    let provider_name = detect_provider(config, model);

    match provider_name {
        "anthropic" => {
            let api_key = config
                .models
                .providers
                .get("anthropic")
                .and_then(|p| p.api_key.clone())
                .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
                .ok_or_else(|| anyhow::anyhow!("No Anthropic API key configured"))?;

            let base_url = config
                .models
                .providers
                .get("anthropic")
                .map(|p| p.base_url.clone())
                .unwrap_or_else(|| "https://api.anthropic.com".to_string());

            Ok(Box::new(anthropic::AnthropicProvider::new(
                api_key,
                base_url,
                model.to_string(),
            )))
        }
        "openai" => {
            let api_key = config
                .models
                .providers
                .get("openai")
                .and_then(|p| p.api_key.clone())
                .or_else(|| std::env::var("OPENAI_API_KEY").ok())
                .ok_or_else(|| anyhow::anyhow!("No OpenAI API key configured"))?;

            let base_url = config
                .models
                .providers
                .get("openai")
                .map(|p| p.base_url.clone())
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());

            Ok(Box::new(openai::OpenAiProvider::new(
                api_key,
                base_url,
                model.to_string(),
            )))
        }
        "google" => {
            let api_key = config
                .models
                .providers
                .get("google")
                .and_then(|p| p.api_key.clone())
                .or_else(|| std::env::var("GOOGLE_API_KEY").ok())
                .ok_or_else(|| anyhow::anyhow!("No Google API key configured"))?;

            Ok(Box::new(gemini::GeminiProvider::new(
                api_key,
                model.to_string(),
            )))
        }
        "groq" => {
            let api_key = config
                .models
                .providers
                .get("groq")
                .and_then(|p| p.api_key.clone())
                .or_else(|| std::env::var("GROQ_API_KEY").ok())
                .ok_or_else(|| anyhow::anyhow!("No Groq API key configured"))?;

            let base_url = config
                .models
                .providers
                .get("groq")
                .map(|p| p.base_url.clone())
                .unwrap_or_else(|| "https://api.groq.com/openai/v1".to_string());

            Ok(Box::new(groq::GroqProvider::new(
                api_key,
                base_url,
                model.to_string(),
            )))
        }
        "mistral" => {
            let api_key = config
                .models
                .providers
                .get("mistral")
                .and_then(|p| p.api_key.clone())
                .or_else(|| std::env::var("MISTRAL_API_KEY").ok())
                .ok_or_else(|| anyhow::anyhow!("No Mistral API key configured"))?;

            let base_url = config
                .models
                .providers
                .get("mistral")
                .map(|p| p.base_url.clone())
                .unwrap_or_else(|| "https://api.mistral.ai/v1".to_string());

            Ok(Box::new(mistral::MistralProvider::new(
                api_key,
                base_url,
                model.to_string(),
            )))
        }
        "ollama" => {
            let api_key = config
                .models
                .providers
                .get("ollama")
                .and_then(|p| p.api_key.clone())
                .or_else(|| std::env::var("OLLAMA_API_KEY").ok());

            let base_url = config
                .models
                .providers
                .get("ollama")
                .map(|p| p.base_url.clone())
                .unwrap_or_else(|| "http://127.0.0.1:11434".to_string());

            Ok(Box::new(ollama::OllamaProvider::new(
                base_url,
                model.to_string(),
                api_key,
            )))
        }
        _ => anyhow::bail!("No provider found for model: {}", model),
    }
}

fn detect_provider(config: &Config, model: &str) -> &'static str {
    let lower = model.to_lowercase();

    // Anthropic models
    if lower.contains("claude") || lower.starts_with("anthropic") {
        return "anthropic";
    }

    // OpenAI models
    if lower.starts_with("gpt")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
    {
        return "openai";
    }

    // Gemini models
    if lower.starts_with("gemini") {
        return "google";
    }

    // Mistral models
    if lower.starts_with("mistral")
        || lower.starts_with("pixtral")
        || lower.starts_with("codestral")
    {
        return "mistral";
    }

    // Groq models (only when groq provider is explicitly configured)
    if config.models.providers.contains_key("groq")
        && (lower.starts_with("llama-") || lower.starts_with("mixtral-"))
    {
        return "groq";
    }

    // Ollama models: tag separator `:` indicates local models (e.g. llama3.3:latest)
    if model.contains(':') {
        return "ollama";
    }

    // Default to anthropic
    "anthropic"
}

// ============================================================================
// AgentModelConfig Helper
// ============================================================================

use crate::config::AgentModelConfig;

impl AgentModelConfig {
    pub fn primary_model(&self) -> Option<String> {
        match self {
            AgentModelConfig::Simple(s) => Some(s.clone()),
            AgentModelConfig::Detailed(d) => d.primary.clone(),
        }
    }
}
