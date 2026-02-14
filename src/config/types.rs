use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ============================================================================
// Gateway Configuration
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GatewayBindMode {
    #[default]
    Loopback,
    Lan,
    Auto,
    Custom,
    Tailnet,
}

impl std::str::FromStr for GatewayBindMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "loopback" => Ok(Self::Loopback),
            "lan" => Ok(Self::Lan),
            "auto" => Ok(Self::Auto),
            "custom" => Ok(Self::Custom),
            "tailnet" => Ok(Self::Tailnet),
            _ => Err(format!("invalid bind mode: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GatewayAuthMode {
    #[default]
    Token,
    Password,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GatewayTailscaleMode {
    #[default]
    Off,
    Serve,
    Funnel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GatewayReloadMode {
    Off,
    Restart,
    Hot,
    #[default]
    Hybrid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayTlsConfig {
    pub enabled: Option<bool>,
    pub auto_generate: Option<bool>,
    pub cert_path: Option<String>,
    pub key_path: Option<String>,
    pub ca_path: Option<String>,
}

impl Default for GatewayTlsConfig {
    fn default() -> Self {
        Self {
            enabled: None,
            auto_generate: None,
            cert_path: None,
            key_path: None,
            ca_path: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayControlUiConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub base_path: Option<String>,
    pub root: Option<String>,
    pub allowed_origins: Option<Vec<String>>,
    #[serde(default)]
    pub allow_insecure_auth: bool,
    #[serde(default)]
    pub dangerously_disable_device_auth: bool,
}

impl Default for GatewayControlUiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base_path: None,
            root: None,
            allowed_origins: None,
            allow_insecure_auth: false,
            dangerously_disable_device_auth: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GatewayAuthConfig {
    #[serde(default)]
    pub mode: GatewayAuthMode,
    pub token: Option<String>,
    pub password: Option<String>,
    #[serde(default)]
    pub allow_tailscale: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GatewayTailscaleConfig {
    #[serde(default)]
    pub mode: GatewayTailscaleMode,
    #[serde(default)]
    pub reset_on_exit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GatewayRemoteConfig {
    pub url: Option<String>,
    pub transport: Option<String>,
    pub token: Option<String>,
    pub password: Option<String>,
    pub tls_fingerprint: Option<String>,
    pub ssh_target: Option<String>,
    pub ssh_identity: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayReloadConfig {
    #[serde(default)]
    pub mode: GatewayReloadMode,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
}

impl Default for GatewayReloadConfig {
    fn default() -> Self {
        Self {
            mode: GatewayReloadMode::default(),
            debounce_ms: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GatewayHttpChatCompletionsConfig {
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayHttpResponsesConfig {
    pub enabled: Option<bool>,
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: u64,
    pub files: Option<GatewayHttpResponsesFilesConfig>,
    pub images: Option<GatewayHttpResponsesImagesConfig>,
}

impl Default for GatewayHttpResponsesConfig {
    fn default() -> Self {
        Self {
            enabled: None,
            max_body_bytes: 20 * 1024 * 1024,
            files: None,
            images: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayHttpResponsesFilesConfig {
    pub allow_url: Option<bool>,
    pub allowed_mimes: Option<Vec<String>>,
    #[serde(default = "default_file_max_bytes")]
    pub max_bytes: u64,
    #[serde(default = "default_file_max_chars")]
    pub max_chars: u64,
    #[serde(default = "default_max_redirects")]
    pub max_redirects: u32,
    #[serde(default = "default_file_timeout_ms")]
    pub timeout_ms: u64,
    pub pdf: Option<GatewayHttpResponsesPdfConfig>,
}

impl Default for GatewayHttpResponsesFilesConfig {
    fn default() -> Self {
        Self {
            allow_url: None,
            allowed_mimes: None,
            max_bytes: 5 * 1024 * 1024,
            max_chars: 200_000,
            max_redirects: 3,
            timeout_ms: 10_000,
            pdf: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayHttpResponsesPdfConfig {
    #[serde(default = "default_pdf_max_pages")]
    pub max_pages: u32,
    #[serde(default = "default_pdf_max_pixels")]
    pub max_pixels: u64,
    #[serde(default = "default_pdf_min_text_chars")]
    pub min_text_chars: u64,
}

impl Default for GatewayHttpResponsesPdfConfig {
    fn default() -> Self {
        Self {
            max_pages: 4,
            max_pixels: 4_000_000,
            min_text_chars: 200,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayHttpResponsesImagesConfig {
    pub allow_url: Option<bool>,
    pub allowed_mimes: Option<Vec<String>>,
    #[serde(default = "default_image_max_bytes")]
    pub max_bytes: u64,
    #[serde(default = "default_max_redirects")]
    pub max_redirects: u32,
    #[serde(default = "default_file_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for GatewayHttpResponsesImagesConfig {
    fn default() -> Self {
        Self {
            allow_url: None,
            allowed_mimes: None,
            max_bytes: 10 * 1024 * 1024,
            max_redirects: 3,
            timeout_ms: 10_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GatewayHttpEndpointsConfig {
    pub chat_completions: Option<GatewayHttpChatCompletionsConfig>,
    pub responses: Option<GatewayHttpResponsesConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GatewayHttpConfig {
    pub endpoints: Option<GatewayHttpEndpointsConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GatewayNodesConfig {
    pub browser: Option<bool>,
    #[serde(default)]
    pub allow_commands: Vec<String>,
    #[serde(default)]
    pub deny_commands: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayConfig {
    #[serde(default = "default_gateway_port")]
    pub port: u16,
    pub mode: Option<String>,
    #[serde(default)]
    pub bind: GatewayBindMode,
    pub custom_bind_host: Option<String>,
    #[serde(default)]
    pub control_ui: GatewayControlUiConfig,
    #[serde(default)]
    pub auth: GatewayAuthConfig,
    #[serde(default)]
    pub tailscale: GatewayTailscaleConfig,
    pub remote: Option<GatewayRemoteConfig>,
    #[serde(default)]
    pub reload: GatewayReloadConfig,
    #[serde(default)]
    pub tls: GatewayTlsConfig,
    #[serde(default)]
    pub http: GatewayHttpConfig,
    pub nodes: Option<GatewayNodesConfig>,
    pub trusted_proxies: Option<Vec<String>>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            port: 18789,
            mode: None,
            bind: GatewayBindMode::Loopback,
            custom_bind_host: None,
            control_ui: GatewayControlUiConfig::default(),
            auth: GatewayAuthConfig::default(),
            tailscale: GatewayTailscaleConfig::default(),
            remote: None,
            reload: GatewayReloadConfig::default(),
            tls: GatewayTlsConfig::default(),
            http: GatewayHttpConfig::default(),
            nodes: None,
            trusted_proxies: None,
        }
    }
}

// ============================================================================
// Agent Configuration
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    #[default]
    Medium,
    High,
    Xhigh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VerboseLevel {
    #[default]
    Off,
    On,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ElevatedLevel {
    #[default]
    Off,
    On,
    Ask,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BlockStreamingLevel {
    #[default]
    Off,
    On,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BlockStreamingBreak {
    #[default]
    TextEnd,
    MessageEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AgentCompactionMode {
    #[default]
    Default,
    Safeguard,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentModelConfig {
    Simple(String),
    Detailed(AgentModelListConfig),
}

impl Default for AgentModelConfig {
    fn default() -> Self {
        Self::Simple("claude-opus-4".to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentModelListConfig {
    pub primary: Option<String>,
    #[serde(default)]
    pub fallbacks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentModelEntryConfig {
    pub alias: Option<String>,
    pub params: Option<HashMap<String, serde_json::Value>>,
    pub streaming: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentCompactionConfig {
    #[serde(default)]
    pub mode: AgentCompactionMode,
    pub reserve_tokens_floor: Option<u64>,
    pub max_history_share: Option<f64>,
    pub memory_flush: Option<AgentCompactionMemoryFlushConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentCompactionMemoryFlushConfig {
    pub enabled: Option<bool>,
    pub soft_threshold_tokens: Option<u64>,
    pub prompt: Option<String>,
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentContextPruningConfig {
    pub mode: Option<String>,
    pub ttl: Option<String>,
    pub keep_last_assistants: Option<u32>,
    pub soft_trim_ratio: Option<f64>,
    pub hard_clear_ratio: Option<f64>,
    pub min_prunable_tool_chars: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatConfig {
    pub every: Option<String>,
    pub active_hours: Option<HeartbeatActiveHours>,
    pub model: Option<String>,
    pub session: Option<String>,
    pub target: Option<String>,
    pub to: Option<String>,
    pub account_id: Option<String>,
    pub prompt: Option<String>,
    #[serde(default = "default_heartbeat_ack_max_chars")]
    pub ack_max_chars: u32,
    #[serde(default)]
    pub include_reasoning: bool,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            every: Some("30m".to_string()),
            active_hours: None,
            model: None,
            session: None,
            target: None,
            to: None,
            account_id: None,
            prompt: None,
            ack_max_chars: 30,
            include_reasoning: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatActiveHours {
    pub start: Option<u32>,
    pub end: Option<u32>,
    pub timezone: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HumanDelayConfig {
    pub mode: Option<String>,
    pub min_ms: Option<u64>,
    pub max_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockStreamingCoalesceConfig {
    pub min_chars: Option<u32>,
    pub max_chars: Option<u32>,
    pub idle_ms: Option<u64>,
}

impl Default for BlockStreamingCoalesceConfig {
    fn default() -> Self {
        Self {
            min_chars: None,
            max_chars: None,
            idle_ms: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BlockStreamingChunkConfig {
    pub min_chars: Option<u32>,
    pub max_chars: Option<u32>,
    pub break_preference: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SubagentsConfig {
    pub max_concurrent: Option<u32>,
    pub archive_after_minutes: Option<u32>,
    pub allow_agents: Option<Vec<String>>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentDefaultsConfig {
    #[serde(default)]
    pub model: AgentModelConfig,
    pub image_model: Option<String>,
    #[serde(default)]
    pub models: HashMap<String, AgentModelEntryConfig>,
    pub workspace: Option<String>,
    pub repo_root: Option<String>,
    pub skip_bootstrap: Option<bool>,
    pub bootstrap_max_chars: Option<u64>,
    pub user_timezone: Option<String>,
    pub time_format: Option<String>,
    pub envelope_timezone: Option<String>,
    pub envelope_timestamp: Option<String>,
    pub envelope_elapsed: Option<String>,
    pub context_tokens: Option<u64>,
    pub context_pruning: Option<AgentContextPruningConfig>,
    #[serde(default)]
    pub compaction: AgentCompactionConfig,
    pub memory_search: Option<bool>,
    pub thinking_default: Option<ThinkingLevel>,
    pub verbose_default: Option<VerboseLevel>,
    pub elevated_default: Option<ElevatedLevel>,
    pub block_streaming_default: Option<BlockStreamingLevel>,
    pub block_streaming_break: Option<BlockStreamingBreak>,
    pub block_streaming_chunk: Option<BlockStreamingChunkConfig>,
    pub block_streaming_coalesce: Option<BlockStreamingCoalesceConfig>,
    pub human_delay: Option<HumanDelayConfig>,
    pub timeout_seconds: Option<u64>,
    pub media_max_mb: Option<u64>,
    pub typing_interval_seconds: Option<u64>,
    pub typing_mode: Option<String>,
    pub heartbeat: Option<HeartbeatConfig>,
    pub max_concurrent: Option<u32>,
    pub subagents: Option<SubagentsConfig>,
    pub sandbox: Option<AgentSandboxConfig>,
    pub cli_backends: Option<HashMap<String, CliBackendConfig>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentSandboxConfig {
    pub mode: Option<String>,
    pub workspace_access: Option<String>,
    pub session_tools_visibility: Option<String>,
    pub scope: Option<String>,
    pub per_session: Option<bool>,
    pub workspace_root: Option<String>,
    pub docker: Option<SandboxDockerSettings>,
    pub browser: Option<SandboxBrowserSettings>,
    pub prune: Option<SandboxPruneSettings>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CliBackendConfig {
    pub command: String,
    pub args: Option<Vec<String>>,
    pub output: Option<String>,
    pub resume_output: Option<String>,
    pub input: Option<String>,
    pub max_prompt_arg_chars: Option<u64>,
    pub env: Option<HashMap<String, String>>,
    pub clear_env: Option<Vec<String>>,
    pub model_arg: Option<String>,
    pub model_aliases: Option<HashMap<String, String>>,
    pub session_arg: Option<String>,
    pub session_args: Option<Vec<String>>,
    pub resume_args: Option<Vec<String>>,
    pub session_mode: Option<String>,
    pub session_id_fields: Option<Vec<String>>,
    pub system_prompt_arg: Option<String>,
    pub system_prompt_mode: Option<String>,
    pub system_prompt_when: Option<String>,
    pub image_arg: Option<String>,
    pub image_mode: Option<String>,
    pub serialize: Option<bool>,
}

// ============================================================================
// Agents (multi-agent) Configuration
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentEntry {
    pub id: String,
    pub default: Option<bool>,
    pub name: Option<String>,
    pub workspace: Option<String>,
    pub agent_dir: Option<String>,
    pub model: Option<AgentModelConfig>,
    pub skills: Option<Vec<String>>,
    pub memory_search: Option<bool>,
    pub human_delay: Option<HumanDelayConfig>,
    pub heartbeat: Option<HeartbeatConfig>,
    pub identity: Option<IdentityConfig>,
    pub group_chat: Option<GroupChatConfig>,
    pub subagents: Option<SubagentsConfig>,
    pub sandbox: Option<AgentSandboxConfig>,
    pub tools: Option<AgentToolsConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentBinding {
    pub agent_id: String,
    #[serde(rename = "match")]
    pub match_rule: AgentBindingMatch,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentBindingMatch {
    pub channel: Option<String>,
    pub account_id: Option<String>,
    pub peer: Option<String>,
    pub guild_id: Option<String>,
    pub team_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentsConfig {
    pub defaults: Option<AgentDefaultsConfig>,
    #[serde(default)]
    pub list: Vec<AgentEntry>,
    #[serde(default)]
    pub bindings: Vec<AgentBinding>,
}

// ============================================================================
// Models Configuration
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ModelApi {
    #[default]
    OpenaiCompletions,
    OpenaiResponses,
    AnthropicMessages,
    GoogleGenerativeAi,
    GithubCopilot,
    BedrockConverseStream,
    Ollama,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelCompatConfig {
    pub supports_store: Option<bool>,
    pub supports_developer_role: Option<bool>,
    pub supports_reasoning_effort: Option<bool>,
    pub max_tokens_field: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDefinitionConfig {
    pub id: String,
    pub name: String,
    pub api: Option<ModelApi>,
    pub reasoning: bool,
    pub input: Vec<String>,
    pub cost: ModelCostConfig,
    pub context_window: u64,
    pub max_tokens: u64,
    pub headers: Option<HashMap<String, String>>,
    pub compat: Option<ModelCompatConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostConfig {
    pub input: f64,
    pub output: f64,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelProviderConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub auth: Option<String>,
    pub api: Option<ModelApi>,
    pub headers: Option<HashMap<String, String>>,
    pub auth_header: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelDefinitionConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ModelsMode {
    #[default]
    Merge,
    Replace,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BedrockDiscoveryConfig {
    pub enabled: Option<bool>,
    pub region: Option<String>,
    pub provider_filter: Option<Vec<String>>,
    pub refresh_interval: Option<String>,
    pub default_context_window: Option<u64>,
    pub default_max_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelsConfig {
    #[serde(default)]
    pub mode: ModelsMode,
    #[serde(default)]
    pub providers: HashMap<String, ModelProviderConfig>,
    pub bedrock_discovery: Option<BedrockDiscoveryConfig>,
}

impl ModelsConfig {
    pub fn apply_anthropic_key(&mut self, key: &str) {
        self.providers
            .entry("anthropic".to_string())
            .and_modify(|p| p.api_key = Some(key.to_string()))
            .or_insert_with(|| ModelProviderConfig {
                base_url: "https://api.anthropic.com".to_string(),
                api_key: Some(key.to_string()),
                auth: None,
                api: Some(ModelApi::AnthropicMessages),
                headers: None,
                auth_header: None,
                models: vec![],
            });
    }

    pub fn apply_openai_key(&mut self, key: &str) {
        self.providers
            .entry("openai".to_string())
            .and_modify(|p| p.api_key = Some(key.to_string()))
            .or_insert_with(|| ModelProviderConfig {
                base_url: "https://api.openai.com/v1".to_string(),
                api_key: Some(key.to_string()),
                auth: None,
                api: Some(ModelApi::OpenaiCompletions),
                headers: None,
                auth_header: None,
                models: vec![],
            });
    }

    pub fn apply_groq_key(&mut self, key: &str) {
        self.providers
            .entry("groq".to_string())
            .and_modify(|p| p.api_key = Some(key.to_string()))
            .or_insert_with(|| ModelProviderConfig {
                base_url: "https://api.groq.com/openai/v1".to_string(),
                api_key: Some(key.to_string()),
                auth: None,
                api: Some(ModelApi::OpenaiCompletions),
                headers: None,
                auth_header: None,
                models: vec![],
            });
    }

    pub fn apply_ollama_key(&mut self, key: &str) {
        self.providers
            .entry("ollama".to_string())
            .and_modify(|p| p.api_key = Some(key.to_string()))
            .or_insert_with(|| ModelProviderConfig {
                base_url: "http://127.0.0.1:11434".to_string(),
                api_key: Some(key.to_string()),
                auth: None,
                api: Some(ModelApi::Ollama),
                headers: None,
                auth_header: None,
                models: vec![],
            });
    }
}

// ============================================================================
// Channels Configuration
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GroupPolicy {
    #[default]
    Open,
    Disabled,
    Allowlist,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DmPolicy {
    Pairing,
    Allowlist,
    #[default]
    Open,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReplyToMode {
    #[default]
    Off,
    First,
    All,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChannelDefaultsConfig {
    pub group_policy: Option<GroupPolicy>,
    pub heartbeat: Option<HeartbeatConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChannelsConfig {
    pub defaults: Option<ChannelDefaultsConfig>,
    #[serde(default)]
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub discord: DiscordConfig,
    #[serde(default)]
    pub slack: SlackConfig,
    #[serde(default)]
    pub whatsapp: WhatsAppConfig,
    #[serde(default)]
    pub signal: SignalConfig,
    #[serde(default)]
    pub imessage: IMessageConfig,
    pub googlechat: Option<GoogleChatConfig>,
    pub msteams: Option<MsTeamsConfig>,
    pub irc: Option<IrcConfig>,
    /// Extension channels loaded via plugins.
    #[serde(flatten)]
    pub extensions: HashMap<String, serde_json::Value>,
}

// ============================================================================
// Telegram Configuration
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TelegramStreamMode {
    Off,
    #[default]
    Partial,
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TelegramReactionLevel {
    Off,
    #[default]
    Ack,
    Minimal,
    Extensive,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TelegramActionConfig {
    pub reactions: Option<bool>,
    pub send_message: Option<bool>,
    pub delete_message: Option<bool>,
    pub edit_message: Option<bool>,
    pub sticker: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelegramCustomCommand {
    pub command: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TelegramGroupConfig {
    pub require_mention: Option<bool>,
    pub group_policy: Option<GroupPolicy>,
    pub tools: Option<serde_json::Value>,
    pub tools_by_sender: Option<HashMap<String, serde_json::Value>>,
    pub skills: Option<Vec<String>>,
    pub topics: Option<HashMap<String, TelegramTopicConfig>>,
    pub enabled: Option<bool>,
    pub allow_from: Option<Vec<String>>,
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TelegramTopicConfig {
    pub require_mention: Option<bool>,
    pub group_policy: Option<GroupPolicy>,
    pub skills: Option<Vec<String>>,
    pub enabled: Option<bool>,
    pub allow_from: Option<Vec<String>>,
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelegramAccountConfig {
    pub name: Option<String>,
    pub capabilities: Option<Vec<String>>,
    pub markdown: Option<bool>,
    pub commands: Option<bool>,
    pub custom_commands: Option<Vec<TelegramCustomCommand>>,
    pub config_writes: Option<bool>,
    pub dm_policy: Option<DmPolicy>,
    pub enabled: Option<bool>,
    pub bot_token: Option<String>,
    pub token_file: Option<String>,
    pub reply_to_mode: Option<ReplyToMode>,
    pub groups: Option<HashMap<String, TelegramGroupConfig>>,
    pub allow_from: Option<Vec<String>>,
    pub group_allow_from: Option<Vec<String>>,
    pub group_policy: Option<GroupPolicy>,
    pub history_limit: Option<u32>,
    pub dm_history_limit: Option<u32>,
    pub dms: Option<serde_json::Value>,
    #[serde(default = "default_telegram_text_chunk_limit")]
    pub text_chunk_limit: u32,
    pub chunk_mode: Option<String>,
    pub block_streaming: Option<bool>,
    pub draft_chunk: Option<bool>,
    pub block_streaming_coalesce: Option<BlockStreamingCoalesceConfig>,
    pub stream_mode: Option<TelegramStreamMode>,
    pub media_max_mb: Option<u64>,
    pub timeout_seconds: Option<u64>,
    pub retry: Option<OutboundRetryConfig>,
    pub network: Option<TelegramNetworkConfig>,
    pub proxy: Option<String>,
    pub webhook_url: Option<String>,
    pub webhook_secret: Option<String>,
    pub webhook_path: Option<String>,
    pub actions: Option<TelegramActionConfig>,
    pub reaction_notifications: Option<String>,
    pub reaction_level: Option<TelegramReactionLevel>,
    pub heartbeat: Option<HeartbeatConfig>,
    pub link_preview: Option<bool>,
    pub response_prefix: Option<String>,
}

impl Default for TelegramAccountConfig {
    fn default() -> Self {
        Self {
            name: None,
            capabilities: None,
            markdown: None,
            commands: None,
            custom_commands: None,
            config_writes: None,
            dm_policy: None,
            enabled: None,
            bot_token: None,
            token_file: None,
            reply_to_mode: None,
            groups: None,
            allow_from: None,
            group_allow_from: None,
            group_policy: None,
            history_limit: None,
            dm_history_limit: None,
            dms: None,
            text_chunk_limit: 4000,
            chunk_mode: None,
            block_streaming: None,
            draft_chunk: None,
            block_streaming_coalesce: None,
            stream_mode: None,
            media_max_mb: None,
            timeout_seconds: None,
            retry: None,
            network: None,
            proxy: None,
            webhook_url: None,
            webhook_secret: None,
            webhook_path: None,
            actions: None,
            reaction_notifications: None,
            reaction_level: None,
            heartbeat: None,
            link_preview: Some(true),
            response_prefix: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TelegramNetworkConfig {
    pub auto_select_family: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TelegramConfig {
    pub accounts: Option<HashMap<String, TelegramAccountConfig>>,
    #[serde(flatten)]
    pub default_account: TelegramAccountConfig,
}

impl TelegramConfig {
    pub fn apply_token(&mut self, token: &str) {
        self.default_account.bot_token = Some(token.to_string());
    }
}

// ============================================================================
// Discord Configuration
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DiscordDmConfig {
    pub enabled: Option<bool>,
    pub policy: Option<DmPolicy>,
    pub allow_from: Option<Vec<String>>,
    pub group_enabled: Option<bool>,
    pub group_channels: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DiscordGuildChannelConfig {
    pub allow: Option<bool>,
    pub require_mention: Option<bool>,
    pub tools: Option<serde_json::Value>,
    pub tools_by_sender: Option<HashMap<String, serde_json::Value>>,
    pub skills: Option<Vec<String>>,
    pub enabled: Option<bool>,
    pub users: Option<Vec<String>>,
    pub system_prompt: Option<String>,
    pub include_thread_starter: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DiscordGuildEntry {
    pub slug: Option<String>,
    pub require_mention: Option<bool>,
    pub tools: Option<serde_json::Value>,
    pub tools_by_sender: Option<HashMap<String, serde_json::Value>>,
    pub reaction_notifications: Option<String>,
    pub users: Option<Vec<String>>,
    pub channels: Option<HashMap<String, DiscordGuildChannelConfig>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DiscordActionConfig {
    pub reactions: Option<bool>,
    pub stickers: Option<bool>,
    pub polls: Option<bool>,
    pub permissions: Option<bool>,
    pub messages: Option<bool>,
    pub threads: Option<bool>,
    pub pins: Option<bool>,
    pub search: Option<bool>,
    pub member_info: Option<bool>,
    pub role_info: Option<bool>,
    pub roles: Option<bool>,
    pub channel_info: Option<bool>,
    pub voice_status: Option<bool>,
    pub events: Option<bool>,
    pub moderation: Option<bool>,
    pub emoji_uploads: Option<bool>,
    pub sticker_uploads: Option<bool>,
    pub channels: Option<bool>,
    pub presence: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DiscordIntentsConfig {
    pub presence: Option<bool>,
    pub guild_members: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DiscordExecApprovalConfig {
    pub enabled: Option<bool>,
    pub approvers: Option<Vec<String>>,
    pub agent_filter: Option<Vec<String>>,
    pub session_filter: Option<Vec<String>>,
    pub cleanup_after_resolve: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordAccountConfig {
    pub name: Option<String>,
    pub capabilities: Option<Vec<String>>,
    pub markdown: Option<bool>,
    pub commands: Option<bool>,
    pub config_writes: Option<bool>,
    pub enabled: Option<bool>,
    pub token: Option<String>,
    pub allow_bots: Option<bool>,
    pub group_policy: Option<GroupPolicy>,
    #[serde(default = "default_discord_text_chunk_limit")]
    pub text_chunk_limit: u32,
    pub chunk_mode: Option<String>,
    pub block_streaming: Option<bool>,
    pub block_streaming_coalesce: Option<BlockStreamingCoalesceConfig>,
    pub max_lines_per_message: Option<u32>,
    pub media_max_mb: Option<u64>,
    pub history_limit: Option<u32>,
    pub dm_history_limit: Option<u32>,
    pub dms: Option<DiscordDmConfig>,
    pub retry: Option<OutboundRetryConfig>,
    pub actions: Option<DiscordActionConfig>,
    pub reply_to_mode: Option<ReplyToMode>,
    pub dm: Option<DiscordDmConfig>,
    pub guilds: Option<HashMap<String, DiscordGuildEntry>>,
    pub heartbeat: Option<HeartbeatConfig>,
    pub exec_approvals: Option<DiscordExecApprovalConfig>,
    pub agent_components: Option<serde_json::Value>,
    pub intents: Option<DiscordIntentsConfig>,
    pub pluralkit: Option<bool>,
    pub response_prefix: Option<String>,
}

impl Default for DiscordAccountConfig {
    fn default() -> Self {
        Self {
            name: None,
            capabilities: None,
            markdown: None,
            commands: None,
            config_writes: None,
            enabled: None,
            token: None,
            allow_bots: None,
            group_policy: None,
            text_chunk_limit: 2000,
            chunk_mode: None,
            block_streaming: None,
            block_streaming_coalesce: None,
            max_lines_per_message: None,
            media_max_mb: None,
            history_limit: None,
            dm_history_limit: None,
            dms: None,
            retry: None,
            actions: None,
            reply_to_mode: None,
            dm: None,
            guilds: None,
            heartbeat: None,
            exec_approvals: None,
            agent_components: None,
            intents: None,
            pluralkit: None,
            response_prefix: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DiscordConfig {
    pub accounts: Option<HashMap<String, DiscordAccountConfig>>,
    #[serde(flatten)]
    pub default_account: DiscordAccountConfig,
}

impl DiscordConfig {
    pub fn apply_token(&mut self, token: &str) {
        self.default_account.token = Some(token.to_string());
    }
}

// ============================================================================
// Slack Configuration
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SlackDmConfig {
    pub enabled: Option<bool>,
    pub policy: Option<DmPolicy>,
    pub allow_from: Option<Vec<String>>,
    pub group_enabled: Option<bool>,
    pub group_channels: Option<Vec<String>>,
    pub reply_to_mode: Option<ReplyToMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SlackChannelConfig {
    pub enabled: Option<bool>,
    pub allow: Option<bool>,
    pub require_mention: Option<bool>,
    pub tools: Option<serde_json::Value>,
    pub tools_by_sender: Option<HashMap<String, serde_json::Value>>,
    pub allow_bots: Option<bool>,
    pub users: Option<Vec<String>>,
    pub skills: Option<Vec<String>>,
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SlackActionConfig {
    pub reactions: Option<bool>,
    pub messages: Option<bool>,
    pub pins: Option<bool>,
    pub search: Option<bool>,
    pub permissions: Option<bool>,
    pub member_info: Option<bool>,
    pub channel_info: Option<bool>,
    pub emoji_list: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SlackSlashCommandConfig {
    pub enabled: Option<bool>,
    pub name: Option<String>,
    pub session_prefix: Option<String>,
    pub ephemeral: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SlackThreadConfig {
    pub history_scope: Option<String>,
    pub inherit_parent: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlackAccountConfig {
    pub name: Option<String>,
    pub mode: Option<String>,
    pub signing_secret: Option<String>,
    pub webhook_path: Option<String>,
    pub capabilities: Option<Vec<String>>,
    pub markdown: Option<bool>,
    pub commands: Option<bool>,
    pub config_writes: Option<bool>,
    pub enabled: Option<bool>,
    pub bot_token: Option<String>,
    pub app_token: Option<String>,
    pub user_token: Option<String>,
    pub user_token_read_only: Option<bool>,
    pub allow_bots: Option<bool>,
    pub require_mention: Option<bool>,
    pub group_policy: Option<GroupPolicy>,
    pub history_limit: Option<u32>,
    pub dm_history_limit: Option<u32>,
    pub dms: Option<SlackDmConfig>,
    #[serde(default = "default_slack_text_chunk_limit")]
    pub text_chunk_limit: u32,
    pub chunk_mode: Option<String>,
    pub block_streaming: Option<bool>,
    pub block_streaming_coalesce: Option<BlockStreamingCoalesceConfig>,
    pub media_max_mb: Option<u64>,
    pub reaction_notifications: Option<String>,
    pub reaction_allowlist: Option<Vec<String>>,
    pub reply_to_mode: Option<ReplyToMode>,
    pub reply_to_mode_by_chat_type: Option<HashMap<String, ReplyToMode>>,
    pub thread: Option<SlackThreadConfig>,
    pub actions: Option<SlackActionConfig>,
    pub slash_command: Option<SlackSlashCommandConfig>,
    pub dm: Option<SlackDmConfig>,
    pub channels: Option<HashMap<String, SlackChannelConfig>>,
    pub heartbeat: Option<HeartbeatConfig>,
    pub response_prefix: Option<String>,
}

impl Default for SlackAccountConfig {
    fn default() -> Self {
        Self {
            name: None,
            mode: None,
            signing_secret: None,
            webhook_path: None,
            capabilities: None,
            markdown: None,
            commands: None,
            config_writes: None,
            enabled: None,
            bot_token: None,
            app_token: None,
            user_token: None,
            user_token_read_only: None,
            allow_bots: None,
            require_mention: None,
            group_policy: None,
            history_limit: None,
            dm_history_limit: None,
            dms: None,
            text_chunk_limit: 4000,
            chunk_mode: None,
            block_streaming: None,
            block_streaming_coalesce: None,
            media_max_mb: None,
            reaction_notifications: None,
            reaction_allowlist: None,
            reply_to_mode: None,
            reply_to_mode_by_chat_type: None,
            thread: None,
            actions: None,
            slash_command: None,
            dm: None,
            channels: None,
            heartbeat: None,
            response_prefix: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SlackConfig {
    pub accounts: Option<HashMap<String, SlackAccountConfig>>,
    #[serde(flatten)]
    pub default_account: SlackAccountConfig,
}

impl SlackConfig {
    pub fn apply_bot_token(&mut self, token: &str) {
        self.default_account.bot_token = Some(token.to_string());
    }

    pub fn apply_app_token(&mut self, token: &str) {
        self.default_account.app_token = Some(token.to_string());
    }
}

// ============================================================================
// WhatsApp Configuration
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WhatsAppActionConfig {
    pub reactions: Option<bool>,
    pub send_message: Option<bool>,
    pub polls: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WhatsAppAckReaction {
    pub emoji: Option<String>,
    pub direct: Option<bool>,
    pub group: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WhatsAppAccountConfig {
    pub name: Option<String>,
    pub capabilities: Option<Vec<String>>,
    pub markdown: Option<bool>,
    pub config_writes: Option<bool>,
    pub enabled: Option<bool>,
    pub send_read_receipts: Option<bool>,
    pub message_prefix: Option<String>,
    pub response_prefix: Option<String>,
    pub auth_dir: Option<String>,
    pub dm_policy: Option<DmPolicy>,
    pub self_chat_mode: Option<String>,
    pub allow_from: Option<Vec<String>>,
    pub group_allow_from: Option<Vec<String>>,
    pub group_policy: Option<GroupPolicy>,
    pub history_limit: Option<u32>,
    pub dm_history_limit: Option<u32>,
    pub dms: Option<serde_json::Value>,
    #[serde(default = "default_whatsapp_text_chunk_limit")]
    pub text_chunk_limit: u32,
    pub chunk_mode: Option<String>,
    pub media_max_mb: Option<u64>,
    pub block_streaming: Option<bool>,
    pub block_streaming_coalesce: Option<BlockStreamingCoalesceConfig>,
    pub groups: Option<HashMap<String, serde_json::Value>>,
    pub ack_reaction: Option<WhatsAppAckReaction>,
    pub debounce_ms: Option<u64>,
    pub heartbeat: Option<HeartbeatConfig>,
    pub actions: Option<WhatsAppActionConfig>,
}

impl Default for WhatsAppAccountConfig {
    fn default() -> Self {
        Self {
            name: None,
            capabilities: None,
            markdown: None,
            config_writes: None,
            enabled: None,
            send_read_receipts: Some(true),
            message_prefix: None,
            response_prefix: None,
            auth_dir: None,
            dm_policy: None,
            self_chat_mode: None,
            allow_from: None,
            group_allow_from: None,
            group_policy: None,
            history_limit: None,
            dm_history_limit: None,
            dms: None,
            text_chunk_limit: 4000,
            chunk_mode: None,
            media_max_mb: Some(50),
            block_streaming: None,
            block_streaming_coalesce: None,
            groups: None,
            ack_reaction: None,
            debounce_ms: None,
            heartbeat: None,
            actions: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WhatsAppConfig {
    pub accounts: Option<HashMap<String, WhatsAppAccountConfig>>,
    #[serde(flatten)]
    pub default_account: WhatsAppAccountConfig,
}

// ============================================================================
// Signal Configuration
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SignalConfig {
    pub enabled: Option<bool>,
    pub api_url: Option<String>,
    pub phone_number: Option<String>,
    pub allow_from: Option<Vec<String>>,
    pub group_policy: Option<GroupPolicy>,
    pub dm_policy: Option<DmPolicy>,
}

// ============================================================================
// iMessage Configuration
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct IMessageConfig {
    pub enabled: Option<bool>,
    pub provider: Option<String>,
    pub api_url: Option<String>,
    pub api_password: Option<String>,
    pub allow_from: Option<Vec<String>>,
    pub group_policy: Option<GroupPolicy>,
    pub dm_policy: Option<DmPolicy>,
}

// ============================================================================
// Google Chat / MS Teams / IRC Configuration
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GoogleChatConfig {
    pub enabled: Option<bool>,
    pub service_account_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MsTeamsConfig {
    pub enabled: Option<bool>,
    pub webhook_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct IrcConfig {
    pub enabled: Option<bool>,
    pub server: Option<String>,
    pub port: Option<u16>,
    pub nickname: Option<String>,
    pub channels: Option<Vec<String>>,
    pub tls: Option<bool>,
}

// ============================================================================
// Tools Configuration
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolProfileId {
    Minimal,
    Coding,
    Messaging,
    #[default]
    Full,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ToolPolicyConfig {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub also_allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    pub profile: Option<ToolProfileId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ExecToolConfig {
    pub host: Option<String>,
    pub security: Option<String>,
    pub ask: Option<String>,
    pub node: Option<String>,
    #[serde(default)]
    pub path_prepend: Vec<String>,
    #[serde(default)]
    pub safe_bins: Vec<String>,
    pub background_ms: Option<u64>,
    pub timeout_sec: Option<u64>,
    pub approval_running_notice_ms: Option<u64>,
    pub cleanup_ms: Option<u64>,
    pub notify_on_exit: Option<bool>,
    pub apply_patch: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchConfig {
    pub provider: Option<String>,
    pub api_key: Option<String>,
    pub max_results: Option<u32>,
    pub timeout_seconds: Option<u64>,
    pub cache_ttl_minutes: Option<u64>,
    pub perplexity: Option<PerplexitySearchConfig>,
    pub grok: Option<GrokSearchConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PerplexitySearchConfig {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GrokSearchConfig {
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub inline_citations: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WebFetchConfig {
    pub enabled: Option<bool>,
    pub max_chars: Option<u64>,
    pub max_chars_cap: Option<u64>,
    pub timeout_seconds: Option<u64>,
    pub cache_ttl_minutes: Option<u64>,
    pub max_redirects: Option<u32>,
    pub user_agent: Option<String>,
    pub readability: Option<bool>,
    pub firecrawl: Option<FirecrawlConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FirecrawlConfig {
    pub enabled: Option<bool>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub only_main_content: Option<bool>,
    pub max_age_ms: Option<u64>,
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WebToolsConfig {
    pub search: Option<WebSearchConfig>,
    pub fetch: Option<WebFetchConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MediaToolsConfig {
    pub models: Option<Vec<serde_json::Value>>,
    pub concurrency: Option<u32>,
    pub image: Option<serde_json::Value>,
    pub audio: Option<serde_json::Value>,
    pub video: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LinkToolsConfig {
    pub enabled: Option<bool>,
    pub scope: Option<String>,
    pub max_links: Option<u32>,
    pub timeout_seconds: Option<u64>,
    pub models: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MessageToolConfig {
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentToAgentConfig {
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolsConfig {
    pub profile: Option<ToolProfileId>,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub also_allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub by_provider: HashMap<String, ToolPolicyConfig>,
    pub elevated: Option<serde_json::Value>,
    pub exec: Option<ExecToolConfig>,
    pub sandbox: Option<AgentSandboxConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ToolsConfig {
    pub profile: Option<ToolProfileId>,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub also_allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub by_provider: HashMap<String, ToolPolicyConfig>,
    #[serde(default)]
    pub web: WebToolsConfig,
    pub media: Option<MediaToolsConfig>,
    pub links: Option<LinkToolsConfig>,
    pub message: Option<MessageToolConfig>,
    pub agent_to_agent: Option<AgentToAgentConfig>,
    pub elevated: Option<serde_json::Value>,
    pub exec: Option<ExecToolConfig>,
    pub subagents: Option<SubagentsConfig>,
    pub sandbox: Option<AgentSandboxConfig>,
}

// ============================================================================
// Memory Configuration
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MemoryBackend {
    #[default]
    Builtin,
    Qmd,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MemoryConfig {
    #[serde(default)]
    pub backend: MemoryBackend,
    pub citations: Option<String>,
    pub qmd: Option<MemoryQmdConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MemoryQmdConfig {
    pub command: Option<String>,
    pub search_mode: Option<String>,
    pub include_default_memory: Option<bool>,
    #[serde(default)]
    pub paths: Vec<MemoryQmdIndexPath>,
    pub sessions: Option<MemoryQmdSessionConfig>,
    pub update: Option<MemoryQmdUpdateConfig>,
    pub limits: Option<MemoryQmdLimitsConfig>,
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryQmdIndexPath {
    pub path: String,
    pub name: Option<String>,
    pub pattern: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MemoryQmdSessionConfig {
    pub enabled: Option<bool>,
    pub export_dir: Option<String>,
    pub retention_days: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MemoryQmdUpdateConfig {
    pub interval: Option<String>,
    pub debounce_ms: Option<u64>,
    pub on_boot: Option<bool>,
    pub wait_for_boot_sync: Option<bool>,
    pub embed_interval: Option<String>,
    pub command_timeout_ms: Option<u64>,
    pub update_timeout_ms: Option<u64>,
    pub embed_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MemoryQmdLimitsConfig {
    pub max_results: Option<u32>,
    pub max_snippet_chars: Option<u64>,
    pub max_injected_chars: Option<u64>,
    pub timeout_ms: Option<u64>,
}

// ============================================================================
// Memory Search (tool-level) Configuration
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddingProvider {
    #[default]
    Openai,
    Gemini,
    Local,
    Voyage,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MemorySearchConfig {
    pub enabled: Option<bool>,
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub extra_paths: Vec<String>,
    pub experimental: Option<bool>,
    pub provider: Option<EmbeddingProvider>,
    pub remote: Option<MemorySearchRemoteConfig>,
    pub fallback: Option<String>,
    pub model: Option<String>,
    pub local: Option<MemorySearchLocalConfig>,
    pub store: Option<MemorySearchStoreConfig>,
    pub chunking: Option<MemorySearchChunkingConfig>,
    pub sync: Option<MemorySearchSyncConfig>,
    pub query: Option<MemorySearchQueryConfig>,
    pub cache: Option<MemorySearchCacheConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MemorySearchRemoteConfig {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub batch: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MemorySearchLocalConfig {
    pub model_path: Option<String>,
    pub model_cache_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MemorySearchStoreConfig {
    pub driver: Option<String>,
    pub path: Option<String>,
    pub vector: Option<serde_json::Value>,
    pub cache: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MemorySearchChunkingConfig {
    pub tokens: Option<u32>,
    pub overlap: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MemorySearchSyncConfig {
    pub on_boot: Option<bool>,
    pub interval: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MemorySearchQueryConfig {
    pub max_results: Option<u32>,
    pub min_score: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MemorySearchCacheConfig {
    pub enabled: Option<bool>,
    pub max_entries: Option<u64>,
}

// ============================================================================
// Plugins Configuration
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PluginEntryConfig {
    pub enabled: Option<bool>,
    pub config: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PluginsLoadConfig {
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PluginSlotsConfig {
    pub memory: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PluginInstallRecord {
    pub source: String,
    pub spec: Option<String>,
    pub source_path: Option<String>,
    pub install_path: Option<String>,
    pub version: Option<String>,
    pub installed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PluginsConfig {
    pub enabled: Option<bool>,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    pub load: Option<PluginsLoadConfig>,
    pub slots: Option<PluginSlotsConfig>,
    #[serde(default)]
    pub entries: HashMap<String, PluginEntryConfig>,
    pub installs: Option<HashMap<String, PluginInstallRecord>>,
}

// ============================================================================
// Hooks Configuration
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HookMappingMatch {
    pub path: Option<String>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HookMappingTransform {
    pub module: String,
    pub export: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HookMappingConfig {
    pub id: Option<String>,
    #[serde(rename = "match")]
    pub match_rule: Option<HookMappingMatch>,
    pub action: Option<String>,
    pub wake_mode: Option<String>,
    pub name: Option<String>,
    pub agent_id: Option<String>,
    pub session_key: Option<String>,
    pub message_template: Option<String>,
    pub text_template: Option<String>,
    pub deliver: Option<bool>,
    pub allow_unsafe_external_content: Option<bool>,
    pub channel: Option<String>,
    pub to: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub transform: Option<HookMappingTransform>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HooksGmailConfig {
    pub account: Option<String>,
    pub label: Option<String>,
    pub topic: Option<String>,
    pub subscription: Option<String>,
    pub push_token: Option<String>,
    pub hook_url: Option<String>,
    pub include_body: Option<bool>,
    pub max_bytes: Option<u64>,
    pub renew_every_minutes: Option<u64>,
    pub allow_unsafe_external_content: Option<bool>,
    pub serve: Option<serde_json::Value>,
    pub tailscale: Option<serde_json::Value>,
    pub model: Option<String>,
    pub thinking: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InternalHookHandlerConfig {
    pub event: String,
    pub module: String,
    pub export: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HooksConfig {
    pub enabled: Option<bool>,
    pub path: Option<String>,
    pub token: Option<String>,
    pub allowed_agent_ids: Option<Vec<String>>,
    pub max_body_bytes: Option<u64>,
    pub presets: Option<Vec<String>>,
    pub transforms_dir: Option<String>,
    #[serde(default)]
    pub mappings: Vec<HookMappingConfig>,
    pub gmail: Option<HooksGmailConfig>,
    pub internal: Option<serde_json::Value>,
}

// ============================================================================
// Messages Configuration
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GroupChatConfig {
    #[serde(default)]
    pub mention_patterns: Vec<String>,
    pub history_limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DmChatConfig {
    pub history_limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct QueueConfig {
    pub mode: Option<String>,
    pub by_channel: Option<HashMap<String, serde_json::Value>>,
    pub debounce_ms: Option<u64>,
    pub debounce_ms_by_channel: Option<HashMap<String, u64>>,
    pub cap: Option<u32>,
    pub drop: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InboundDebounceConfig {
    pub debounce_ms: Option<u64>,
    pub by_channel: Option<HashMap<String, u64>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AudioTranscriptionConfig {
    pub command: Option<Vec<String>>,
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AudioConfig {
    pub transcription: Option<AudioTranscriptionConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MessagesConfig {
    pub message_prefix: Option<String>,
    pub response_prefix: Option<String>,
    pub group_chat: Option<GroupChatConfig>,
    pub dm: Option<DmChatConfig>,
    pub queue: Option<QueueConfig>,
    pub inbound: Option<InboundDebounceConfig>,
    pub ack_reaction: Option<String>,
    pub ack_reaction_scope: Option<String>,
    pub remove_ack_after_reply: Option<bool>,
    pub tts: Option<serde_json::Value>,
    pub audio: Option<AudioConfig>,
}

// ============================================================================
// Commands Configuration
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CommandsConfig {
    pub native: Option<serde_json::Value>,
    pub native_skills: Option<serde_json::Value>,
    pub text: Option<bool>,
    pub bash: Option<bool>,
    pub bash_foreground_ms: Option<u64>,
    pub config: Option<bool>,
    pub debug: Option<bool>,
    pub restart: Option<bool>,
    pub use_access_groups: Option<bool>,
    pub owner_allow_from: Option<Vec<String>>,
    pub allow_from: Option<HashMap<String, Vec<serde_json::Value>>>,
}

// ============================================================================
// Session Configuration
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SessionScope {
    #[default]
    PerSender,
    Global,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DmScope {
    #[default]
    Main,
    PerPeer,
    PerChannelPeer,
    PerAccountChannelPeer,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionResetConfig {
    pub mode: Option<String>,
    pub at_hour: Option<u32>,
    pub idle_minutes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionResetByTypeConfig {
    pub direct: Option<SessionResetConfig>,
    pub dm: Option<SessionResetConfig>,
    pub group: Option<SessionResetConfig>,
    pub thread: Option<SessionResetConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionSendPolicyMatch {
    pub channel: Option<String>,
    pub chat_type: Option<String>,
    pub key_prefix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionSendPolicyRule {
    pub action: String,
    #[serde(rename = "match")]
    pub match_rule: Option<SessionSendPolicyMatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionSendPolicyConfig {
    pub default: Option<String>,
    #[serde(default)]
    pub rules: Vec<SessionSendPolicyRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionAgentToAgentConfig {
    pub max_ping_pong_turns: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionMaintenanceConfig {
    pub mode: Option<String>,
    pub prune_after: Option<String>,
    pub prune_days: Option<u32>,
    pub max_entries: Option<u64>,
    pub rotate_bytes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfig {
    #[serde(default)]
    pub scope: SessionScope,
    pub dm_scope: Option<DmScope>,
    pub identity_links: Option<HashMap<String, Vec<String>>>,
    pub reset_triggers: Option<Vec<String>>,
    pub idle_minutes: Option<u64>,
    pub reset: Option<SessionResetConfig>,
    pub reset_by_type: Option<SessionResetByTypeConfig>,
    pub reset_by_channel: Option<HashMap<String, SessionResetConfig>>,
    pub store: Option<String>,
    pub typing_interval_seconds: Option<u64>,
    pub typing_mode: Option<String>,
    pub main_key: Option<String>,
    pub send_policy: Option<SessionSendPolicyConfig>,
    pub agent_to_agent: Option<SessionAgentToAgentConfig>,
    pub maintenance: Option<SessionMaintenanceConfig>,
}

// ============================================================================
// Logging & Diagnostics Configuration
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LoggingLevel {
    Silent,
    Fatal,
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LoggingConfig {
    #[serde(default)]
    pub level: LoggingLevel,
    pub file: Option<String>,
    pub console_level: Option<LoggingLevel>,
    pub console_style: Option<String>,
    pub redact_sensitive: Option<String>,
    pub redact_patterns: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsOtelConfig {
    pub enabled: Option<bool>,
    pub endpoint: Option<String>,
    pub protocol: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub service_name: Option<String>,
    pub traces: Option<bool>,
    pub metrics: Option<bool>,
    pub logs: Option<bool>,
    pub sample_rate: Option<f64>,
    pub flush_interval_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsCacheTraceConfig {
    pub enabled: Option<bool>,
    pub file_path: Option<String>,
    pub include_messages: Option<bool>,
    pub include_prompt: Option<bool>,
    pub include_system: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsConfig {
    pub enabled: Option<bool>,
    pub flags: Option<Vec<String>>,
    pub otel: Option<DiagnosticsOtelConfig>,
    pub cache_trace: Option<DiagnosticsCacheTraceConfig>,
}

// ============================================================================
// Sandbox Configuration
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SandboxDockerSettings {
    pub image: Option<String>,
    pub container_prefix: Option<String>,
    pub workdir: Option<String>,
    pub read_only_root: Option<bool>,
    pub tmpfs: Option<Vec<String>>,
    pub network: Option<String>,
    pub user: Option<String>,
    pub cap_drop: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
    pub setup_command: Option<String>,
    pub pids_limit: Option<u32>,
    pub memory: Option<String>,
    pub memory_swap: Option<String>,
    pub cpus: Option<f64>,
    pub ulimits: Option<HashMap<String, String>>,
    pub seccomp_profile: Option<String>,
    pub apparmor_profile: Option<String>,
    pub dns: Option<Vec<String>>,
    pub extra_hosts: Option<Vec<String>>,
    pub binds: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SandboxBrowserSettings {
    pub enabled: Option<bool>,
    pub image: Option<String>,
    pub container_prefix: Option<String>,
    pub cdp_port: Option<u16>,
    pub vnc_port: Option<u16>,
    pub no_vnc_port: Option<u16>,
    pub headless: Option<bool>,
    pub enable_no_vnc: Option<bool>,
    pub allow_host_control: Option<bool>,
    pub auto_start: Option<bool>,
    pub auto_start_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SandboxPruneSettings {
    pub idle_hours: Option<f64>,
    pub max_age_days: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SandboxConfig {
    pub docker: Option<SandboxDockerSettings>,
    pub browser: Option<SandboxBrowserSettings>,
    pub prune: Option<SandboxPruneSettings>,
}

// ============================================================================
// Browser Configuration
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BrowserProfileConfig {
    pub cdp_port: Option<u16>,
    pub cdp_url: Option<String>,
    pub driver: Option<String>,
    pub color: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSnapshotDefaults {
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub evaluate_enabled: bool,
    pub cdp_url: Option<String>,
    #[serde(default = "default_remote_cdp_timeout")]
    pub remote_cdp_timeout_ms: u64,
    pub remote_cdp_handshake_timeout_ms: Option<u64>,
    pub color: Option<String>,
    pub executable_path: Option<String>,
    #[serde(default)]
    pub headless: bool,
    #[serde(default)]
    pub no_sandbox: bool,
    #[serde(default)]
    pub attach_only: bool,
    pub default_profile: Option<String>,
    pub profiles: Option<HashMap<String, BrowserProfileConfig>>,
    pub snapshot_defaults: Option<BrowserSnapshotDefaults>,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            evaluate_enabled: true,
            cdp_url: None,
            remote_cdp_timeout_ms: 1500,
            remote_cdp_handshake_timeout_ms: None,
            color: Some("#FF4500".to_string()),
            executable_path: None,
            headless: false,
            no_sandbox: false,
            attach_only: false,
            default_profile: Some("chrome".to_string()),
            profiles: None,
            snapshot_defaults: None,
        }
    }
}

// ============================================================================
// TTS Configuration
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TtsProvider {
    #[default]
    Elevenlabs,
    Openai,
    Edge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TtsAutoMode {
    #[default]
    Off,
    Always,
    Inbound,
    Tagged,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TtsElevenlabsConfig {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub voice_id: Option<String>,
    pub model_id: Option<String>,
    pub seed: Option<u64>,
    pub apply_text_normalization: Option<String>,
    pub language_code: Option<String>,
    pub voice_settings: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TtsOpenaiConfig {
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub voice: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TtsEdgeConfig {
    pub enabled: Option<bool>,
    pub voice: Option<String>,
    pub lang: Option<String>,
    pub output_format: Option<String>,
    pub pitch: Option<String>,
    pub rate: Option<String>,
    pub volume: Option<String>,
    pub save_subtitles: Option<bool>,
    pub proxy: Option<String>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TtsConfig {
    pub auto: Option<TtsAutoMode>,
    pub enabled: Option<bool>,
    pub mode: Option<String>,
    pub provider: Option<TtsProvider>,
    pub summary_model: Option<String>,
    pub model_overrides: Option<serde_json::Value>,
    pub elevenlabs: Option<TtsElevenlabsConfig>,
    pub openai: Option<TtsOpenaiConfig>,
    pub edge: Option<TtsEdgeConfig>,
    pub prefs_path: Option<String>,
    pub max_text_length: Option<u64>,
    pub timeout_ms: Option<u64>,
}

// ============================================================================
// Cron Configuration
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CronConfig {
    pub enabled: Option<bool>,
    pub store: Option<String>,
    pub max_concurrent_runs: Option<u32>,
    pub session_retention: Option<String>,
}

// ============================================================================
// Web Configuration
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WebReconnectConfig {
    pub initial_ms: Option<u64>,
    pub max_ms: Option<u64>,
    pub factor: Option<f64>,
    pub jitter: Option<f64>,
    pub max_attempts: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WebConfig {
    pub enabled: Option<bool>,
    pub heartbeat_seconds: Option<u64>,
    pub reconnect: Option<WebReconnectConfig>,
}

// ============================================================================
// Identity Configuration
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct IdentityConfig {
    pub name: Option<String>,
    pub theme: Option<String>,
    pub emoji: Option<String>,
    pub avatar: Option<String>,
}

// ============================================================================
// Outbound Retry Configuration
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboundRetryConfig {
    #[serde(default = "default_retry_attempts")]
    pub attempts: u32,
    #[serde(default = "default_retry_min_delay")]
    pub min_delay_ms: u64,
    #[serde(default = "default_retry_max_delay")]
    pub max_delay_ms: u64,
    pub jitter: Option<f64>,
}

impl Default for OutboundRetryConfig {
    fn default() -> Self {
        Self {
            attempts: 3,
            min_delay_ms: 1000,
            max_delay_ms: 10_000,
            jitter: Some(0.1),
        }
    }
}

// ============================================================================
// Default value helper functions
// ============================================================================

fn default_true() -> bool {
    true
}

fn default_gateway_port() -> u16 {
    18789
}

fn default_debounce_ms() -> u64 {
    300
}

fn default_max_body_bytes() -> u64 {
    20 * 1024 * 1024
}

fn default_file_max_bytes() -> u64 {
    5 * 1024 * 1024
}

fn default_file_max_chars() -> u64 {
    200_000
}

fn default_max_redirects() -> u32 {
    3
}

fn default_file_timeout_ms() -> u64 {
    10_000
}

fn default_pdf_max_pages() -> u32 {
    4
}

fn default_pdf_max_pixels() -> u64 {
    4_000_000
}

fn default_pdf_min_text_chars() -> u64 {
    200
}

fn default_image_max_bytes() -> u64 {
    10 * 1024 * 1024
}

fn default_remote_cdp_timeout() -> u64 {
    1500
}

fn default_telegram_text_chunk_limit() -> u32 {
    4000
}

fn default_discord_text_chunk_limit() -> u32 {
    2000
}

fn default_slack_text_chunk_limit() -> u32 {
    4000
}

fn default_whatsapp_text_chunk_limit() -> u32 {
    4000
}

fn default_heartbeat_ack_max_chars() -> u32 {
    30
}

fn default_retry_attempts() -> u32 {
    3
}

fn default_retry_min_delay() -> u64 {
    1000
}

fn default_retry_max_delay() -> u64 {
    10_000
}
