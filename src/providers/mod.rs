mod anthropic;
mod anthropic_compat;
pub mod bedrock;
pub mod catalog;
pub mod copilot;
pub mod deepinfra;
pub mod deepseek;
pub mod gemini;
mod groq;
pub mod lm_studio;
pub mod manifest;
pub mod minimax;
mod mistral;
pub mod moonshot;
mod ollama;
pub mod openai;
pub(crate) mod openai_codex;
pub(crate) mod openai_compat;
pub mod openrouter;
pub mod promos;
pub mod qianfan;
pub mod shims;
pub mod stepfun;
pub mod xai;
pub mod zai;

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
    /// Provider replay hook event (v2026.4.1).
    Replay(serde_json::Value),
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
    OaiCompatDef {
        // v2026.5.2: manifest-driven Z.AI endpoint (api.z.ai native).
        name: "zai",
        default_base_url: "https://api.z.ai/api/paas/v4",
        env_key: "ZAI_API_KEY",
    },
    OaiCompatDef {
        name: "cerebras",
        default_base_url: "https://api.cerebras.ai/v1",
        env_key: "CEREBRAS_API_KEY",
    },
    OaiCompatDef {
        name: "deepinfra",
        default_base_url: "https://api.deepinfra.com/v1/openai",
        env_key: "DEEPINFRA_API_KEY",
    },
    OaiCompatDef {
        name: "stepfun",
        default_base_url: "https://api.stepfun.ai/v1",
        env_key: "STEPFUN_API_KEY",
    },
    OaiCompatDef {
        name: "deepseek",
        default_base_url: "https://api.deepseek.com",
        env_key: "DEEPSEEK_API_KEY",
    },
    // ---- v2026.6.x–7.1 new providers (bundled-native; upstream ships these
    // as npm plugins — behavior ported, packaging kept native) ----
    OaiCompatDef {
        name: "featherless",
        default_base_url: "https://api.featherless.ai/v1",
        env_key: "FEATHERLESS_API_KEY",
    },
    OaiCompatDef {
        name: "longcat",
        default_base_url: "https://api.longcat.chat/openai",
        env_key: "LONGCAT_API_KEY",
    },
    OaiCompatDef {
        name: "cohere",
        default_base_url: "https://api.cohere.ai/compatibility/v1",
        env_key: "COHERE_API_KEY",
    },
    OaiCompatDef {
        name: "clawrouter",
        default_base_url: "https://clawrouter.openclaw.ai",
        env_key: "CLAWROUTER_API_KEY",
    },
    OaiCompatDef {
        name: "tencent",
        default_base_url: "https://tokenhub.tencentmaas.com/v1",
        env_key: "TOKENHUB_API_KEY",
    },
    OaiCompatDef {
        // Meta muse-spark-1.1 (Responses API upstream; routed through the
        // completions-compatible path here — encrypted reasoning replay and
        // live model validation tracked as remaining work).
        name: "meta",
        default_base_url: "https://api.meta.ai/v1",
        env_key: "MODEL_API_KEY",
    },
    OaiCompatDef {
        // ds4: local DeepSeek V4 Flash server (docs-level provider riding
        // the generic `localService` on-demand startup).
        name: "ds4",
        default_base_url: "http://127.0.0.1:8000/v1",
        env_key: "DS4_API_KEY",
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

    // Model suppression (v2026.4.29): manifest-declared suppressions block
    // known-bad provider/model pairs before resolution. Explicitly configured
    // models (present in the config provider's model list) can bypass
    // conditional suppressions but not unconditional ones (#74451).
    {
        let bare_model = model
            .split_once('/')
            .map(|(_, rest)| rest)
            .unwrap_or(model);
        let provider_cfg = config.models.providers.get(provider_name);
        let base_url = provider_cfg.map(|p| p.base_url.as_str());
        let explicitly_configured = provider_cfg
            .map(|p| p.models.iter().any(|m| m.id == bare_model || m.id == model))
            .unwrap_or(false);
        for candidate in [model, bare_model] {
            if let Some(verdict) = manifest::resolve_model_suppression(
                provider_name,
                candidate,
                base_url,
                explicitly_configured,
            ) {
                anyhow::bail!("{}", verdict.error_message);
            }
        }
    }

    match provider_name {
        "openrouter" => {
            let api_key = config
                .models
                .providers
                .get("openrouter")
                .and_then(|p| p.api_key.clone())
                .or_else(|| std::env::var("OPENROUTER_API_KEY").ok())
                .ok_or_else(|| anyhow::anyhow!("No API key configured for openrouter"))?;
            let base_url = config
                .models
                .providers
                .get("openrouter")
                .map(|p| p.base_url.clone())
                .unwrap_or_else(|| openrouter::OPENROUTER_DEFAULT_BASE_URL.to_string());
            Ok(Box::new(openrouter::OpenRouterProvider::new(
                api_key,
                base_url,
                model.to_string(),
            )))
        }
        "lmstudio" => {
            let api_key = config
                .models
                .providers
                .get("lmstudio")
                .and_then(|p| p.api_key.clone())
                .or_else(|| std::env::var(lm_studio::LMSTUDIO_DEFAULT_API_KEY_ENV_VAR).ok());
            let base_url = config
                .models
                .providers
                .get("lmstudio")
                .map(|p| p.base_url.clone())
                .unwrap_or_else(|| lm_studio::LMSTUDIO_DEFAULT_BASE_URL.to_string());
            let params = config
                .models
                .providers
                .get("lmstudio")
                .and_then(|p| p.params.clone());
            Ok(Box::new(lm_studio::LmStudioProvider::new(
                api_key,
                base_url,
                model.to_string(),
                params.as_ref(),
            )))
        }
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
                let local_service = config
                    .models
                    .providers
                    .get(provider_name)
                    .and_then(|p| p.local_service.clone());
                return Ok(Box::new(
                    GenericOpenAiCompatProvider::new(
                        api_key,
                        base_url,
                        model.to_string(),
                        provider_name.to_string(),
                    )
                    .with_local_service(local_service),
                ));
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
            "cerebras" => "cerebras",
            "deepinfra" => "deepinfra",
            "zai" | "z-ai" => "zai",
            "stepfun" => "stepfun",
            "deepseek" => "deepseek",
            "lmstudio" | "lm-studio" => "lmstudio",
            "featherless" => "featherless",
            "longcat" => "longcat",
            "cohere" => "cohere",
            "clawrouter" => "clawrouter",
            "tencent" | "hy3" | "hunyuan" => "tencent",
            "meta" => "meta",
            "ds4" => "ds4",
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

    // GLM family routes to Z.AI (v2026.6.x: stop GLM-5 fallthrough to the
    // default provider producing misleading missing-key errors). Ollama-style
    // `:tag` ids keep routing to Ollama below.
    if lower.starts_with("glm-") && !model.contains(':') {
        return "zai";
    }

    // DeepSeek bare ids route to DeepSeek (Ollama-style `:tag` ids keep
    // routing to Ollama below).
    if lower.starts_with("deepseek-") && !model.contains(':') {
        return "deepseek";
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
// Error classification (v2026.6.x–7.1 — provider-side `detectErrorKind`)
// ============================================================================

/// Coarse provider-error classification shared by fallback/failover logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    RateLimit,
    Billing,
    Auth,
    Timeout,
    Overloaded,
    Format,
    Unknown,
}

impl ProviderErrorKind {
    /// Whether the failure is worth retrying against the same profile.
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            ProviderErrorKind::RateLimit | ProviderErrorKind::Overloaded | ProviderErrorKind::Timeout
        )
    }
}

/// Classify a provider error from HTTP status + message text. Ordering
/// matters (v2026.6.x fix): rate-limit detection runs BEFORE timeout so
/// messages like "429: request timed out waiting for capacity" classify as
/// rate-limit, not timeout. Bare `status: internal server error` messages
/// classify as retryable overload.
pub fn detect_error_kind(status: Option<u16>, message: &str) -> ProviderErrorKind {
    let lower = message.to_ascii_lowercase();
    match status {
        Some(429) => return ProviderErrorKind::RateLimit,
        Some(401) | Some(403) => {
            if lower.contains("budget") || lower.contains("billing") || lower.contains("credit") {
                return ProviderErrorKind::Billing;
            }
            return ProviderErrorKind::Auth;
        }
        Some(402) => return ProviderErrorKind::Billing,
        Some(500) | Some(502) | Some(503) | Some(529) => return ProviderErrorKind::Overloaded,
        _ => {}
    }
    // Rate-limit before timeout (upstream `detectErrorKind` ordering fix).
    if lower.contains("rate limit") || lower.contains("rate_limit") || lower.contains("429") {
        return ProviderErrorKind::RateLimit;
    }
    if lower.contains("timed out") || lower.contains("timeout") {
        return ProviderErrorKind::Timeout;
    }
    if lower.contains("overloaded") || lower.contains("internal server error") {
        return ProviderErrorKind::Overloaded;
    }
    if lower.contains("credit") || lower.contains("billing") || lower.contains("quota") {
        return ProviderErrorKind::Billing;
    }
    if lower.contains("unauthorized") || lower.contains("invalid api key") {
        return ProviderErrorKind::Auth;
    }
    if lower.contains("must end with a user message") || lower.contains("invalid request format") {
        return ProviderErrorKind::Format;
    }
    ProviderErrorKind::Unknown
}

// ============================================================================
// Bounded body reads (v2026.6.1–7.1)
// ============================================================================

/// Default cap for provider/media/catalog/polling response bodies.
pub const DEFAULT_PROVIDER_BODY_LIMIT_BYTES: usize = 2 * 1024 * 1024;

/// Read a response body with a byte cap, rejecting oversized payloads
/// instead of buffering them unbounded (v2026.6.1–7.1 hardening applied
/// across provider fetches).
pub async fn read_body_bounded(resp: reqwest::Response, max_bytes: usize) -> Result<String> {
    use futures::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if buf.len() + chunk.len() > max_bytes {
            anyhow::bail!(
                "provider response exceeded {} byte limit",
                max_bytes
            );
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|_| anyhow::anyhow!("provider response was not valid UTF-8"))
}

/// Parse a bounded JSON body, surfacing malformed responses as provider-owned
/// errors rather than panics or unbounded buffering.
pub async fn read_json_bounded(
    resp: reqwest::Response,
    max_bytes: usize,
    provider_label: &str,
) -> Result<serde_json::Value> {
    let text = read_body_bounded(resp, max_bytes).await?;
    serde_json::from_str(&text)
        .map_err(|_| anyhow::anyhow!("{}: malformed JSON response", provider_label))
}

// ============================================================================
// Local service startup (v2026.6.x `localService` — used by e.g. the `ds4`
// local DeepSeek V4 Flash provider)
// ============================================================================

/// Default readiness window for on-demand local model servers.
pub const LOCAL_SERVICE_DEFAULT_READY_TIMEOUT_MS: u64 = 30_000;

/// Whether the local service's health endpoint answers.
pub async fn local_service_healthy(health_url: &str) -> bool {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(1_500))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    matches!(client.get(health_url).send().await, Ok(resp) if resp.status().is_success())
}

/// Ensure an on-demand local model service is running before an
/// OpenAI-compatible request: probe `healthUrl`; when unreachable, spawn the
/// configured command (detached) and poll until ready or the readiness
/// window elapses. Without a `healthUrl`, the command is spawned
/// best-effort.
pub async fn ensure_local_service_started(
    cfg: &crate::config::LocalServiceConfig,
) -> Result<()> {
    if let Some(health_url) = cfg.health_url.as_deref() {
        if local_service_healthy(health_url).await {
            return Ok(());
        }
    }
    let mut command = tokio::process::Command::new(&cfg.command);
    if let Some(args) = &cfg.args {
        command.args(args);
    }
    if let Some(cwd) = &cfg.cwd {
        command.current_dir(cwd);
    }
    if let Some(env) = &cfg.env {
        command.envs(env);
    }
    command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("localService spawn failed ({}): {}", cfg.command, e))?;

    let Some(health_url) = cfg.health_url.as_deref() else {
        return Ok(());
    };
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(
            cfg.ready_timeout_ms.unwrap_or(LOCAL_SERVICE_DEFAULT_READY_TIMEOUT_MS),
        );
    loop {
        if local_service_healthy(health_url).await {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "localService did not become healthy at {} within the readiness window",
                health_url
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

// ============================================================================
// Provider auth-state prewarm (v2026.6.x)
// ============================================================================

static PREWARMED_PROVIDERS: once_cell::sync::OnceCell<Vec<&'static str>> =
    once_cell::sync::OnceCell::new();

/// Prewarm the provider auth-state snapshot (env-detected providers) so
/// startup paths answer from a cached snapshot (~ms) instead of re-scanning
/// the environment on every call. Idempotent; safe to defer past readiness.
pub fn prewarm_provider_auth() -> &'static [&'static str] {
    PREWARMED_PROVIDERS.get_or_init(resolve_implicit_providers)
}

/// Cached provider auth snapshot when prewarmed; `None` before prewarm.
pub fn prewarmed_provider_auth() -> Option<&'static [&'static str]> {
    PREWARMED_PROVIDERS.get().map(|v| v.as_slice())
}

// ============================================================================
// Implicit discovery gating (v2026.6.x: `models.mode = "replace"` skips
// implicit provider discovery)
// ============================================================================

/// Whether implicit env-based provider discovery applies for this config.
pub fn implicit_discovery_enabled(config: &Config) -> bool {
    config.models.mode != crate::config::ModelsMode::Replace
}

/// Config-aware implicit provider resolution: honors
/// `models.mode = "replace"` by returning only explicitly configured
/// providers (fail-closed; no env synthesis).
pub fn resolve_implicit_providers_for(config: &Config) -> Vec<&'static str> {
    if implicit_discovery_enabled(config) {
        resolve_implicit_providers()
    } else {
        Vec::new()
    }
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
        ("CEREBRAS_API_KEY", "cerebras"),
        ("DEEPINFRA_API_KEY", "deepinfra"),
        ("ZAI_API_KEY", "zai"),
        ("STEPFUN_API_KEY", "stepfun"),
        ("DEEPSEEK_API_KEY", "deepseek"),
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
    /// On-demand local model service (v2026.6.x `localService`).
    local_service: Option<crate::config::LocalServiceConfig>,
}

impl GenericOpenAiCompatProvider {
    fn new(api_key: String, base_url: String, model: String, provider_name: String) -> Self {
        Self {
            api_key,
            base_url,
            model,
            provider_name,
            client: reqwest::Client::new(),
            local_service: None,
        }
    }

    fn with_local_service(
        mut self,
        local_service: Option<crate::config::LocalServiceConfig>,
    ) -> Self {
        self.local_service = local_service;
        self
    }

    async fn ensure_local_service(&self) {
        if let Some(cfg) = &self.local_service {
            if let Err(e) = ensure_local_service_started(cfg).await {
                tracing::warn!("localService startup failed for {}: {}", self.provider_name, e);
            }
        }
    }
}

#[async_trait]
impl ModelProvider for GenericOpenAiCompatProvider {
    async fn chat(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        self.ensure_local_service().await;
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
        self.ensure_local_service().await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    // ====================================================================
    // detect_provider (v2026.3.11 — MiniMax + alternative providers)
    // ====================================================================

    #[test]
    fn detect_anthropic_claude_models() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "claude-sonnet-4-6"), "anthropic");
        assert_eq!(detect_provider(&config, "claude-3-opus"), "anthropic");
    }

    #[test]
    fn detect_openai_gpt_models() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "gpt-4o"), "openai");
        assert_eq!(detect_provider(&config, "gpt-4-turbo"), "openai");
        assert_eq!(detect_provider(&config, "o1-preview"), "openai");
        assert_eq!(detect_provider(&config, "o3-mini"), "openai");
    }

    #[test]
    fn detect_gemini_models() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "gemini-2.0-flash"), "google");
        assert_eq!(detect_provider(&config, "gemini-pro"), "google");
    }

    #[test]
    fn detect_mistral_models() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "mistral-large"), "mistral");
        assert_eq!(detect_provider(&config, "codestral-latest"), "mistral");
        assert_eq!(detect_provider(&config, "pixtral-12b"), "mistral");
    }

    #[test]
    fn detect_xai_grok_models() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "grok-2"), "xai");
    }

    #[test]
    fn detect_ollama_models_with_tag() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "llama3.3:latest"), "ollama");
        assert_eq!(detect_provider(&config, "phi4:14b"), "ollama");
    }

    // v2026.3.11: MiniMax provider prefix
    #[test]
    fn detect_minimax_prefix() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "minimax/abab6.5-chat"), "minimax");
    }

    // v2026.3.11: Alternative provider prefixes
    #[test]
    fn detect_together_prefix() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "together/llama-3-70b"), "together");
    }

    #[test]
    fn detect_venice_prefix() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "venice/llama-3.1-405b"), "venice");
    }

    #[test]
    fn detect_openrouter_prefix() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "openrouter/gpt-4-turbo"), "openrouter");
    }

    #[test]
    fn detect_nvidia_prefix() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "nvidia/nemotron-4-340b"), "nvidia");
        assert_eq!(detect_provider(&config, "nim/llama-3"), "nvidia");
    }

    #[test]
    fn detect_qianfan_baidu_prefix() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "qianfan/ernie-4.0"), "qianfan");
        assert_eq!(detect_provider(&config, "baidu/ernie-bot"), "qianfan");
    }

    #[test]
    fn detect_doubao_volcengine_prefix() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "doubao/doubao-pro"), "doubao");
        assert_eq!(detect_provider(&config, "volcengine/doubao-lite"), "doubao");
    }

    #[test]
    fn detect_mimo_xiaomi_prefix() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "mimo/mimo-llm"), "mimo");
        assert_eq!(detect_provider(&config, "xiaomi/mimo-vl"), "mimo");
    }

    #[test]
    fn detect_kimi_prefix() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "kimi/moonshot-v1-8k"), "kimi");
    }

    #[test]
    fn detect_cloudflare_prefix() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "cloudflare/llama-3"), "cloudflare");
        assert_eq!(detect_provider(&config, "cf/phi-2"), "cloudflare");
    }

    #[test]
    fn detect_copilot_github_prefix() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "copilot/gpt-4"), "copilot");
        assert_eq!(detect_provider(&config, "github/gpt-4o"), "copilot");
    }

    #[test]
    fn detect_bedrock_aws_prefix() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "bedrock/claude-3"), "bedrock");
        assert_eq!(detect_provider(&config, "aws/claude-3"), "bedrock");
    }

    #[test]
    fn detect_unknown_prefix_falls_through() {
        let config = Config::default();
        // Unknown prefix with slash — falls through to model name matching
        assert_eq!(detect_provider(&config, "unknown/claude-model"), "anthropic");
    }

    #[test]
    fn detect_unknown_model_defaults_to_anthropic() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "some-random-model"), "anthropic");
    }

    // ====================================================================
    // OPENAI_COMPAT_PROVIDERS includes MiniMax (v2026.3.11)
    // ====================================================================

    #[test]
    fn openai_compat_includes_minimax() {
        assert!(OPENAI_COMPAT_PROVIDERS.iter().any(|p| p.name == "minimax"));
    }

    #[test]
    fn openai_compat_minimax_base_url() {
        let minimax = OPENAI_COMPAT_PROVIDERS
            .iter()
            .find(|p| p.name == "minimax")
            .unwrap();
        assert_eq!(minimax.default_base_url, "https://api.minimaxi.chat/v1");
    }

    // ====================================================================
    // ANTHROPIC_COMPAT_PROVIDERS includes MiniMax (v2026.3.11)
    // ====================================================================

    #[test]
    fn anthropic_compat_includes_minimax() {
        assert!(ANTHROPIC_COMPAT_PROVIDERS.iter().any(|p| p.name == "minimax"));
    }

    // ====================================================================
    // v2026.3.11 alternative provider entries
    // ====================================================================

    #[test]
    fn openai_compat_includes_all_v2026_3_11_providers() {
        let names: Vec<&str> = OPENAI_COMPAT_PROVIDERS.iter().map(|p| p.name).collect();
        assert!(names.contains(&"venice"));
        assert!(names.contains(&"minimax"));
        assert!(names.contains(&"nvidia"));
        assert!(names.contains(&"kilocode"));
        assert!(names.contains(&"qianfan"));
        assert!(names.contains(&"doubao"));
        assert!(names.contains(&"byteplus"));
        assert!(names.contains(&"vllm"));
    }

    #[test]
    fn anthropic_compat_includes_mimo_kimi() {
        let names: Vec<&str> = ANTHROPIC_COMPAT_PROVIDERS.iter().map(|p| p.name).collect();
        assert!(names.contains(&"mimo"));
        assert!(names.contains(&"kimi"));
    }

    // ====================================================================
    // v2026.4.26 — Cerebras as bundled OpenAI-compatible provider
    // ====================================================================

    #[test]
    fn detect_cerebras_prefix() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "cerebras/llama-3.3-70b"), "cerebras");
    }

    #[test]
    fn openai_compat_includes_cerebras() {
        assert!(OPENAI_COMPAT_PROVIDERS.iter().any(|p| p.name == "cerebras"));
    }

    #[test]
    fn openai_compat_cerebras_base_url() {
        let cerebras = OPENAI_COMPAT_PROVIDERS
            .iter()
            .find(|p| p.name == "cerebras")
            .unwrap();
        assert_eq!(cerebras.default_base_url, "https://api.cerebras.ai/v1");
    }

    #[test]
    fn openai_compat_cerebras_env_key() {
        let cerebras = OPENAI_COMPAT_PROVIDERS
            .iter()
            .find(|p| p.name == "cerebras")
            .unwrap();
        assert_eq!(cerebras.env_key, "CEREBRAS_API_KEY");
    }

    // ====================================================================
    // v2026.4.27 — DeepInfra as bundled OpenAI-compatible provider
    // ====================================================================

    #[test]
    fn detect_deepinfra_prefix() {
        let config = Config::default();
        assert_eq!(
            detect_provider(&config, "deepinfra/meta-llama/Llama-3.3-70B-Instruct"),
            "deepinfra"
        );
    }

    #[test]
    fn openai_compat_includes_deepinfra() {
        assert!(OPENAI_COMPAT_PROVIDERS.iter().any(|p| p.name == "deepinfra"));
    }

    #[test]
    fn openai_compat_deepinfra_base_url() {
        let deepinfra = OPENAI_COMPAT_PROVIDERS
            .iter()
            .find(|p| p.name == "deepinfra")
            .unwrap();
        assert_eq!(
            deepinfra.default_base_url,
            "https://api.deepinfra.com/v1/openai"
        );
    }

    #[test]
    fn openai_compat_deepinfra_env_key() {
        let deepinfra = OPENAI_COMPAT_PROVIDERS
            .iter()
            .find(|p| p.name == "deepinfra")
            .unwrap();
        assert_eq!(deepinfra.env_key, "DEEPINFRA_API_KEY");
    }


    // ====================================================================
    // v2026.7.1 — error classification, discovery gating, new providers
    // ====================================================================

    #[test]
    fn rate_limit_detected_before_timeout() {
        // The message mentions both; rate-limit must win (upstream ordering fix).
        assert_eq!(
            detect_error_kind(None, "429 rate limit: request timed out waiting for capacity"),
            ProviderErrorKind::RateLimit
        );
        assert_eq!(
            detect_error_kind(None, "request timed out"),
            ProviderErrorKind::Timeout
        );
    }

    #[test]
    fn bare_internal_server_error_is_retryable_overload() {
        let kind = detect_error_kind(None, "status: internal server error");
        assert_eq!(kind, ProviderErrorKind::Overloaded);
        assert!(kind.is_retryable());
    }

    #[test]
    fn status_code_classification() {
        assert_eq!(detect_error_kind(Some(429), ""), ProviderErrorKind::RateLimit);
        assert_eq!(detect_error_kind(Some(402), ""), ProviderErrorKind::Billing);
        assert_eq!(
            detect_error_kind(Some(403), "budget limit exceeded"),
            ProviderErrorKind::Billing
        );
        assert_eq!(detect_error_kind(Some(401), "bad key"), ProviderErrorKind::Auth);
        assert_eq!(detect_error_kind(Some(529), ""), ProviderErrorKind::Overloaded);
        assert_eq!(detect_error_kind(None, "???"), ProviderErrorKind::Unknown);
    }

    #[test]
    fn format_errors_do_not_classify_retryable() {
        let kind = detect_error_kind(None, "The conversation must end with a user message");
        assert_eq!(kind, ProviderErrorKind::Format);
        assert!(!kind.is_retryable());
    }

    #[test]
    fn replace_mode_skips_implicit_discovery() {
        let mut config = Config::default();
        assert!(implicit_discovery_enabled(&config));
        config.models.mode = crate::config::ModelsMode::Replace;
        assert!(!implicit_discovery_enabled(&config));
        assert!(resolve_implicit_providers_for(&config).is_empty());
    }

    #[test]
    fn prewarm_snapshot_is_idempotent() {
        let first = prewarm_provider_auth();
        let second = prewarm_provider_auth();
        assert_eq!(first, second);
        assert!(prewarmed_provider_auth().is_some());
    }

    #[test]
    fn detect_new_v7_1_provider_prefixes() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "featherless/Qwen/Qwen3-32B"), "featherless");
        assert_eq!(detect_provider(&config, "longcat/LongCat-2.0"), "longcat");
        assert_eq!(detect_provider(&config, "cohere/command-a-03-2025"), "cohere");
        assert_eq!(detect_provider(&config, "clawrouter/gpt-5.6"), "clawrouter");
        assert_eq!(detect_provider(&config, "tencent/hy3"), "tencent");
        assert_eq!(detect_provider(&config, "hy3/hy3"), "tencent");
        assert_eq!(detect_provider(&config, "meta/muse-spark-1.1"), "meta");
        assert_eq!(detect_provider(&config, "ds4/deepseek-v4-flash"), "ds4");
    }

    #[test]
    fn glm_bare_ids_route_to_zai_not_default() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "glm-5"), "zai");
        assert_eq!(detect_provider(&config, "GLM-4.7"), "zai");
        // Ollama-style tags keep routing to ollama.
        assert_eq!(detect_provider(&config, "glm-4:9b"), "ollama");
    }

    #[test]
    fn deepseek_bare_ids_route_to_deepseek() {
        let config = Config::default();
        assert_eq!(detect_provider(&config, "deepseek-v4-flash"), "deepseek");
        assert_eq!(detect_provider(&config, "deepseek-r1:8b"), "ollama");
    }

    #[test]
    fn new_compat_defs_registered() {
        for name in ["featherless", "longcat", "cohere", "clawrouter", "tencent", "meta", "ds4"] {
            assert!(
                OPENAI_COMPAT_PROVIDERS.iter().any(|d| d.name == name),
                "{} missing from OPENAI_COMPAT_PROVIDERS",
                name
            );
        }
    }

    #[tokio::test]
    async fn bounded_body_read_rejects_oversized_payloads() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(64)))
            .mount(&server)
            .await;
        let resp = reqwest::get(format!("{}/big", server.uri())).await.unwrap();
        let err = read_body_bounded(resp, 16).await.unwrap_err();
        assert!(err.to_string().contains("byte limit"));

        let resp = reqwest::get(format!("{}/big", server.uri())).await.unwrap();
        let ok = read_body_bounded(resp, 1024).await.unwrap();
        assert_eq!(ok.len(), 64);
    }

    #[tokio::test]
    async fn bounded_json_read_flags_malformed_responses() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/bad"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not-json"))
            .mount(&server)
            .await;
        let resp = reqwest::get(format!("{}/bad", server.uri())).await.unwrap();
        let err = read_json_bounded(resp, 1024, "TestProvider").await.unwrap_err();
        assert!(err.to_string().contains("TestProvider"));
        assert!(err.to_string().contains("malformed"));
    }
}
