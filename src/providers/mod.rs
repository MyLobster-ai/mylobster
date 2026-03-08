mod anthropic;
mod anthropic_compat;
mod bedrock;
mod copilot;
mod gemini;
mod groq;
mod mistral;
mod ollama;
mod openai;
pub(crate) mod openai_codex;
pub(crate) mod openai_compat;
mod xai;

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

/// Configuration for extended thinking.
#[derive(Debug, Clone)]
pub struct ThinkingConfig {
    pub budget_tokens: u64,
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
    pub thinking: Option<ThinkingConfig>,
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
    Thinking(String),
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
    Thinking(String),
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
// OpenAI-Compatible Provider Definitions
// ============================================================================

/// Configuration for an OpenAI-compatible provider.
struct OaiCompatDef {
    name: &'static str,
    default_base_url: &'static str,
    env_key: &'static str,
}

/// All OpenAI-compatible providers with their defaults.
const OPENAI_COMPAT_PROVIDERS: &[OaiCompatDef] = &[
    OaiCompatDef {
        name: "together",
        default_base_url: "https://api.together.xyz/v1",
        env_key: "TOGETHER_API_KEY",
    },
    OaiCompatDef {
        name: "huggingface",
        default_base_url: "https://api-inference.huggingface.co/v1",
        env_key: "HUGGINGFACE_API_KEY",
    },
    OaiCompatDef {
        name: "openrouter",
        default_base_url: "https://openrouter.ai/api/v1",
        env_key: "OPENROUTER_API_KEY",
    },
    OaiCompatDef {
        name: "moonshot",
        default_base_url: "https://api.moonshot.ai/v1",
        env_key: "MOONSHOT_API_KEY",
    },
    OaiCompatDef {
        name: "qwen",
        default_base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        env_key: "QWEN_API_KEY",
    },
    OaiCompatDef {
        name: "venice",
        default_base_url: "https://api.venice.ai/api/v1",
        env_key: "VENICE_API_KEY",
    },
    OaiCompatDef {
        name: "minimax",
        default_base_url: "https://api.minimaxi.chat/v1",
        env_key: "MINIMAX_API_KEY",
    },
    OaiCompatDef {
        name: "nvidia",
        default_base_url: "https://integrate.api.nvidia.com/v1",
        env_key: "NVIDIA_API_KEY",
    },
    OaiCompatDef {
        name: "kilocode",
        default_base_url: "https://api.kilocode.ai/v1",
        env_key: "KILOCODE_API_KEY",
    },
    OaiCompatDef {
        name: "vllm",
        default_base_url: "http://127.0.0.1:8000/v1",
        env_key: "VLLM_API_KEY",
    },
    OaiCompatDef {
        name: "qianfan",
        default_base_url: "https://qianfan.baidubce.com/v2",
        env_key: "QIANFAN_API_KEY",
    },
    OaiCompatDef {
        name: "doubao",
        default_base_url: "https://ark.cn-beijing.volces.com/api/v3",
        env_key: "DOUBAO_API_KEY",
    },
    OaiCompatDef {
        name: "byteplus",
        default_base_url: "https://ark.ap-southeast.bytepluses.com/api/v3",
        env_key: "BYTEPLUS_API_KEY",
    },
];

/// Configuration for an Anthropic-compatible provider.
struct AnthropicCompatDef {
    name: &'static str,
    default_base_url: &'static str,
    env_key: &'static str,
}

/// All Anthropic-compatible providers with their defaults.
const ANTHROPIC_COMPAT_PROVIDERS: &[AnthropicCompatDef] = &[
    AnthropicCompatDef {
        name: "minimax",
        default_base_url: "https://api.minimax.chat",
        env_key: "MINIMAX_API_KEY",
    },
    AnthropicCompatDef {
        name: "mimo",
        default_base_url: "https://api.mimo.ai",
        env_key: "MIMO_API_KEY",
    },
    AnthropicCompatDef {
        name: "kimi",
        default_base_url: "https://api.moonshot.cn",
        env_key: "KIMI_API_KEY",
    },
    AnthropicCompatDef {
        name: "cloudflare",
        default_base_url: "https://gateway.ai.cloudflare.com",
        env_key: "CLOUDFLARE_AI_API_KEY",
    },
];

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
        "xai" => {
            let api_key = config
                .models
                .providers
                .get("xai")
                .and_then(|p| p.api_key.clone())
                .or_else(|| std::env::var("XAI_API_KEY").ok())
                .ok_or_else(|| anyhow::anyhow!("No xAI API key configured"))?;

            let base_url = config
                .models
                .providers
                .get("xai")
                .map(|p| p.base_url.clone())
                .unwrap_or_else(|| "https://api.x.ai/v1".to_string());

            Ok(Box::new(xai::XaiProvider::new(
                api_key,
                base_url,
                model.to_string(),
            )))
        }
        "copilot" => {
            let github_token = config
                .models
                .providers
                .get("copilot")
                .and_then(|p| p.api_key.clone())
                .or_else(|| std::env::var("GITHUB_TOKEN").ok())
                .ok_or_else(|| anyhow::anyhow!("No GitHub token configured for Copilot"))?;

            Ok(Box::new(copilot::CopilotProvider::new(
                github_token,
                model.to_string(),
            )))
        }
        "bedrock" => {
            let region = config
                .models
                .providers
                .get("bedrock")
                .map(|p| p.base_url.clone())
                .unwrap_or_else(|| "us-east-1".to_string());

            Ok(Box::new(bedrock::BedrockProvider::new(
                region,
                model.to_string(),
            )))
        }
        _ => {
            // Try OpenAI-compatible providers
            if let Some(def) = OPENAI_COMPAT_PROVIDERS
                .iter()
                .find(|d| d.name == provider_name)
            {
                let api_key = config
                    .models
                    .providers
                    .get(provider_name)
                    .and_then(|p| p.api_key.clone())
                    .or_else(|| std::env::var(def.env_key).ok())
                    .ok_or_else(|| {
                        anyhow::anyhow!("No API key configured for {}", provider_name)
                    })?;

                let base_url = config
                    .models
                    .providers
                    .get(provider_name)
                    .map(|p| p.base_url.clone())
                    .unwrap_or_else(|| def.default_base_url.to_string());

                // Reuse GroqProvider (it's just an OpenAI-compat wrapper with custom name)
                return Ok(Box::new(GenericOpenAiCompatProvider::new(
                    api_key,
                    base_url,
                    model.to_string(),
                    provider_name.to_string(),
                )));
            }

            // Try Anthropic-compatible providers
            if let Some(def) = ANTHROPIC_COMPAT_PROVIDERS
                .iter()
                .find(|d| d.name == provider_name)
            {
                let api_key = config
                    .models
                    .providers
                    .get(provider_name)
                    .and_then(|p| p.api_key.clone())
                    .or_else(|| std::env::var(def.env_key).ok())
                    .ok_or_else(|| {
                        anyhow::anyhow!("No API key configured for {}", provider_name)
                    })?;

                let base_url = config
                    .models
                    .providers
                    .get(provider_name)
                    .map(|p| p.base_url.clone())
                    .unwrap_or_else(|| def.default_base_url.to_string());

                return Ok(Box::new(anthropic_compat::AnthropicCompatProvider::new(
                    api_key,
                    base_url,
                    model.to_string(),
                    provider_name.to_string(),
                )));
            }

            anyhow::bail!("No provider found for model: {}", model)
        }
    }
}

fn detect_provider(config: &Config, model: &str) -> &'static str {
    let lower = model.to_lowercase();

    // Check explicit provider prefix (e.g., "together/llama-3", "openrouter/gpt-4")
    if let Some(slash_pos) = model.find('/') {
        let prefix = &model[..slash_pos].to_lowercase();
        // Map common prefixes to provider names
        let mapped = match prefix.as_str() {
            "together" => "together",
            "hf" | "huggingface" => "huggingface",
            "openrouter" | "or" => "openrouter",
            "moonshot" => "moonshot",
            "qwen" => "qwen",
            "venice" => "venice",
            "nvidia" | "nim" => "nvidia",
            "kilocode" => "kilocode",
            "vllm" => "vllm",
            "qianfan" | "baidu" => "qianfan",
            "doubao" | "volcengine" => "doubao",
            "byteplus" => "byteplus",
            "minimax" => "minimax",
            "mimo" | "xiaomi" => "mimo",
            "kimi" => "kimi",
            "cloudflare" | "cf" => "cloudflare",
            "xai" | "grok" => "xai",
            "copilot" | "github" => "copilot",
            "bedrock" | "aws" => "bedrock",
            _ => "",
        };
        if !mapped.is_empty() {
            return mapped;
        }
    }

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

    // xAI Grok models
    if lower.starts_with("grok") {
        return "xai";
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

/// Auto-detect providers from environment variables.
pub fn resolve_implicit_providers() -> Vec<&'static str> {
    let mut providers = Vec::new();

    let env_checks: &[(&str, &str)] = &[
        ("ANTHROPIC_API_KEY", "anthropic"),
        ("OPENAI_API_KEY", "openai"),
        ("GOOGLE_API_KEY", "google"),
        ("GROQ_API_KEY", "groq"),
        ("MISTRAL_API_KEY", "mistral"),
        ("XAI_API_KEY", "xai"),
        ("GITHUB_TOKEN", "copilot"),
        ("AWS_ACCESS_KEY_ID", "bedrock"),
        ("TOGETHER_API_KEY", "together"),
        ("HUGGINGFACE_API_KEY", "huggingface"),
        ("OPENROUTER_API_KEY", "openrouter"),
        ("MOONSHOT_API_KEY", "moonshot"),
        ("QWEN_API_KEY", "qwen"),
        ("VENICE_API_KEY", "venice"),
        ("NVIDIA_API_KEY", "nvidia"),
        ("KILOCODE_API_KEY", "kilocode"),
        ("QIANFAN_API_KEY", "qianfan"),
        ("DOUBAO_API_KEY", "doubao"),
        ("BYTEPLUS_API_KEY", "byteplus"),
        ("MINIMAX_API_KEY", "minimax"),
        ("MIMO_API_KEY", "mimo"),
        ("KIMI_API_KEY", "kimi"),
        ("CLOUDFLARE_AI_API_KEY", "cloudflare"),
    ];

    for (env_var, provider) in env_checks {
        if std::env::var(env_var).is_ok() {
            providers.push(*provider);
        }
    }

    providers
}

// ============================================================================
// Generic OpenAI-Compatible Provider
// ============================================================================

/// A generic provider that uses the OpenAI-compatible API.
/// Used for Together AI, HuggingFace, OpenRouter, etc.
struct GenericOpenAiCompatProvider {
    api_key: String,
    base_url: String,
    model: String,
    provider_name: String,
    client: reqwest::Client,
}

impl GenericOpenAiCompatProvider {
    fn new(api_key: String, base_url: String, model: String, provider_name: String) -> Self {
        Self {
            api_key,
            base_url,
            model,
            provider_name,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl ModelProvider for GenericOpenAiCompatProvider {
    async fn chat(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        openai_compat::openai_compat_chat(
            &self.client,
            &self.base_url,
            &self.api_key,
            request,
            &self.provider_name,
        )
        .await
    }

    async fn stream_chat(&self, request: ProviderRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        openai_compat::openai_compat_stream_chat(
            &self.client,
            &self.base_url,
            &self.api_key,
            request,
            &self.provider_name,
        )
        .await
    }

    fn name(&self) -> &str {
        &self.provider_name
    }
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
