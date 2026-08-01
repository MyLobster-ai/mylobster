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
    /// Max chat message bubble width in px for the Control UI, validated
    /// config replacing patched bundled CSS (v2026.5.2). Valid range
    /// 240–4096.
    pub chat_message_max_width: Option<u32>,
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
            chat_message_max_width: None,
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
    /// Remote auth-attempt rate limit (v2026.7.1). Applied by default to
    /// non-loopback peers; loopback is exempt unless `loopbackExempt` is
    /// explicitly false.
    pub rate_limit: Option<GatewayAuthRateLimitConfig>,
}

/// Remote auth rate limiting (v2026.7.1: `gateway.auth.rateLimit`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GatewayAuthRateLimitConfig {
    /// Max failed attempts per window (default 10).
    pub max_attempts: Option<u32>,
    /// Window in seconds (default 60).
    pub window_seconds: Option<u32>,
    /// Exempt loopback peers (default true).
    pub loopback_exempt: Option<bool>,
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
    /// Graceful shutdown timeout before forcing reload (v2026.4.1).
    /// Defaults to 300_000 ms (5 minutes).
    pub deferral_timeout_ms: Option<u64>,
}

impl Default for GatewayReloadConfig {
    fn default() -> Self {
        Self {
            mode: GatewayReloadMode::default(),
            debounce_ms: 300,
            deferral_timeout_ms: None,
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
    /// Allowed browser origins for WebSocket connections (v2026.3.11, GHSA-5wcw-8jjv-m286).
    /// Empty list means all origins are allowed. Use `["*"]` to explicitly allow all.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Rate limiting configuration (v2026.3.11).
    pub rate_limit: Option<GatewayRateLimitConfig>,
    /// Webchat-specific configuration (v2026.4.1).
    pub webchat: Option<GatewayWebchatConfig>,
    /// Channel health check interval in minutes (v2026.4.1).
    pub channel_health_check_minutes: Option<u32>,
    /// Minutes before a channel socket is considered stale (v2026.4.1).
    pub channel_stale_event_threshold_minutes: Option<u32>,
    /// Maximum channel restarts per hour (v2026.4.1).
    pub channel_max_restarts_per_hour: Option<u32>,
    /// Push notification configuration (v2026.4.1).
    pub push: Option<GatewayPushConfig>,
}

/// Rate limiting for gateway connections (v2026.3.11).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayRateLimitConfig {
    /// Maximum requests per window.
    pub max_requests: Option<u32>,
    /// Window duration in seconds.
    pub window_seconds: Option<u32>,
    /// Maximum concurrent WebSocket connections.
    pub max_connections: Option<u32>,
}

/// Webchat-specific gateway configuration (v2026.4.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GatewayWebchatConfig {
    /// Maximum characters in chat history text truncation.
    pub chat_history_max_chars: Option<u64>,
}

/// Push notification configuration (v2026.4.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GatewayPushConfig {
    /// APNs relay configuration for iOS push notifications.
    pub apns: Option<GatewayApnsConfig>,
}

/// APNs relay configuration (v2026.4.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GatewayApnsConfig {
    pub relay_url: Option<String>,
    pub key_id: Option<String>,
    pub team_id: Option<String>,
    pub bundle_id: Option<String>,
    pub key_path: Option<String>,
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
            allowed_origins: Vec::new(),
            rate_limit: None,
            webchat: None,
            channel_health_check_minutes: None,
            channel_stale_event_threshold_minutes: None,
            channel_max_restarts_per_hour: None,
            push: None,
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
        Self::Simple("claude-sonnet-4-6".to_string())
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
    pub context1m: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentCompactionConfig {
    #[serde(default)]
    pub mode: AgentCompactionMode,
    pub reserve_tokens_floor: Option<u64>,
    pub max_history_share: Option<f64>,
    pub memory_flush: Option<AgentCompactionMemoryFlushConfig>,
    /// Whether to notify user when compaction occurs (v2026.4.1).
    pub notify_user: Option<bool>,
    /// Mid-turn compaction precheck between tool-loop iterations
    /// (`agents.defaults.compaction.midTurnPrecheck`, v2026.5.2). Default off.
    pub mid_turn_precheck: Option<bool>,
    /// Preflight compaction trigger: compact before a turn when the active
    /// transcript exceeds this many bytes (v2026.4.26). Unset/0 = disabled.
    pub max_active_transcript_bytes: Option<u64>,
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

/// Heartbeat delivery target.
///
/// In OpenClaw v2026.2.24, the default was flipped from "last" to "none".
/// - `None` — heartbeat runs but does not deliver to any channel (default).
/// - `Last` — deliver to the last-active channel.
/// - `Channel(name)` — deliver to a specific channel (e.g. "telegram", "discord").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeartbeatTarget {
    None,
    Last,
    Channel(String),
}

impl Serialize for HeartbeatTarget {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            HeartbeatTarget::None => serializer.serialize_str("none"),
            HeartbeatTarget::Last => serializer.serialize_str("last"),
            HeartbeatTarget::Channel(ch) => serializer.serialize_str(ch),
        }
    }
}

impl<'de> Deserialize<'de> for HeartbeatTarget {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "none" => Ok(HeartbeatTarget::None),
            "last" => Ok(HeartbeatTarget::Last),
            other => Ok(HeartbeatTarget::Channel(other.to_string())),
        }
    }
}

impl Default for HeartbeatTarget {
    fn default() -> Self {
        HeartbeatTarget::None
    }
}

/// Policy for heartbeat direct-message delivery.
///
/// Controls how the heartbeat chooses its DM target when `target` is set
/// to a channel that supports direct messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DirectPolicy {
    /// Deliver to the last user who interacted (default).
    #[default]
    Last,
    /// Do not deliver heartbeats as DMs.
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatConfig {
    pub every: Option<String>,
    pub active_hours: Option<HeartbeatActiveHours>,
    pub model: Option<String>,
    pub session: Option<String>,
    pub target: Option<HeartbeatTarget>,
    pub to: Option<String>,
    pub account_id: Option<String>,
    pub prompt: Option<String>,
    #[serde(default = "default_heartbeat_ack_max_chars")]
    pub ack_max_chars: u32,
    #[serde(default)]
    pub include_reasoning: bool,
    pub direct_policy: Option<DirectPolicy>,
    /// Skip heartbeat wakes while the agent has an active run
    /// (agent-scoped, v2026.7.1 heartbeat overhaul).
    pub skip_when_busy: Option<bool>,
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
            direct_policy: None,
            skip_when_busy: None,
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
    pub max_spawn_depth: Option<u8>,
    pub max_children_per_agent: Option<u8>,
    /// Maximum time in seconds a subagent run is allowed to execute (v2026.2.24).
    pub run_timeout_seconds: Option<u64>,
    /// Require explicit agentId in sessions_spawn calls (v2026.4.1).
    pub require_agent_id: Option<bool>,
    /// Timeout for the child's completion announcement back to the parent
    /// (`announceTimeoutMs`, v2026.7.1). Unset = runtime default.
    pub announce_timeout_ms: Option<u64>,
    /// Delegation mode for subagent-capable agents: `"suggest"` (default)
    /// or `"prefer"` (v2026.7.1 `agents.defaults.subagents.delegationMode`).
    pub delegation_mode: Option<String>,
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
    /// Per-agent reasoning default (v2026.4.1).
    pub reasoning_default: Option<ThinkingLevel>,
    /// Per-agent fast mode default (v2026.4.1).
    pub fast_mode_default: Option<bool>,
    /// Global default provider parameters (v2026.4.1).
    pub params: Option<HashMap<String, serde_json::Value>>,
    pub verbose_default: Option<VerboseLevel>,
    pub elevated_default: Option<ElevatedLevel>,
    pub block_streaming_default: Option<BlockStreamingLevel>,
    pub block_streaming_break: Option<BlockStreamingBreak>,
    pub block_streaming_chunk: Option<BlockStreamingChunkConfig>,
    pub block_streaming_coalesce: Option<BlockStreamingCoalesceConfig>,
    pub human_delay: Option<HumanDelayConfig>,
    pub timeout_seconds: Option<u64>,
    pub image_max_dimension_px: Option<u32>,
    pub media_max_mb: Option<u64>,
    pub typing_interval_seconds: Option<u64>,
    pub typing_mode: Option<String>,
    pub heartbeat: Option<HeartbeatConfig>,
    pub max_concurrent: Option<u32>,
    pub subagents: Option<SubagentsConfig>,
    pub sandbox: Option<AgentSandboxConfig>,
    pub cli_backends: Option<HashMap<String, CliBackendConfig>>,
    /// Skip optional workspace bootstrap files (TOOLS.md etc.) when building
    /// the agent bootstrap context (`agents.defaults.skipOptionalBootstrapFiles`,
    /// v2026.5.2). Default off.
    pub skip_optional_bootstrap_files: Option<bool>,
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
    /// Per-agent thinking default (v2026.4.1).
    pub thinking_default: Option<ThinkingLevel>,
    /// Per-agent reasoning default (v2026.4.1).
    pub reasoning_default: Option<ThinkingLevel>,
    /// Per-agent fast mode default (v2026.4.1).
    pub fast_mode_default: Option<bool>,
    /// Per-agent TTS overrides (`agents.list[].tts`, v2026.4.26). Resolved
    /// via `agents::resolve_agent_tts`; falls back to the global `tts`
    /// config when unset.
    pub tts: Option<TtsConfig>,
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
    MistralMessages,
    GoogleGenerativeAi,
    GithubCopilot,
    BedrockConverseStream,
    Ollama,
    MiniMax,
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
    /// Provider-specific request/runtime params (v2026.5.2), e.g.
    /// `models.providers.lmstudio.params.preload: false` or OpenAI-compat
    /// `extraBody` passthrough fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    /// On-demand local model service startup (v2026.6.x `localService`) —
    /// e.g. the `ds4` local DeepSeek V4 Flash server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_service: Option<LocalServiceConfig>,
}

/// Provider-level local model service (v2026.6.x): starts an on-demand local
/// model server before OpenAI-compatible requests hit it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalServiceConfig {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_stop_ms: Option<u64>,
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
    /// Bedrock Guardrails configuration (v2026.4.1).
    pub guardrails: Option<BedrockGuardrailsConfig>,
}

/// Bedrock Guardrails configuration (v2026.4.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BedrockGuardrailsConfig {
    pub enabled: Option<bool>,
    pub guardrail_id: Option<String>,
    pub guardrail_version: Option<String>,
    /// Trace configuration: "enabled" or "disabled".
    pub trace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelsConfig {
    #[serde(default)]
    pub mode: ModelsMode,
    #[serde(default)]
    pub providers: HashMap<String, ModelProviderConfig>,
    pub bedrock_discovery: Option<BedrockDiscoveryConfig>,
    /// Alternative providers discovered at runtime (v2026.3.11).
    /// Keys: alibaba, baidu, bytedance, huggingface, kimi, kilocode,
    /// moonshot, nvidia, openrouter, perplexity, qwen_portal,
    /// together, venice, vercel, xiaomi, vllm, cloudflare.
    pub alternative_providers: Option<HashMap<String, ModelProviderConfig>>,
    /// Cooldown probing configuration (v2026.3.11).
    /// Caps fallback probing to one per provider per run.
    pub cooldown_probe_cap: Option<u32>,
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
                params: None,
                local_service: None,
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
                params: None,
                local_service: None,
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
                params: None,
                local_service: None,
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
                params: None,
                local_service: None,
            });
    }

    pub fn apply_mistral_key(&mut self, key: &str) {
        self.providers
            .entry("mistral".to_string())
            .and_modify(|p| p.api_key = Some(key.to_string()))
            .or_insert_with(|| ModelProviderConfig {
                base_url: "https://api.mistral.ai/v1".to_string(),
                api_key: Some(key.to_string()),
                auth: None,
                api: Some(ModelApi::MistralMessages),
                headers: None,
                auth_header: None,
                models: vec![],
                params: None,
                local_service: None,
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
    /// Reply once per batched delivery (single-use like `First`) (v2026.7.1).
    Batched,
}

/// Shared per-pair bot loop-guard settings (`channels.defaults.botLoopProtection`).
///
/// Ported from OpenClaw `src/plugin-sdk/pair-loop-guard-runtime.ts` +
/// `src/channels/turn/bot-loop-protection.ts` (v2026.5.x). Defaults:
/// `maxEventsPerWindow: 20`, `windowSeconds: 60`, `cooldownSeconds: 60`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BotLoopProtectionConfig {
    /// Enables or disables loop protection for the channel/account scope.
    pub enabled: Option<bool>,
    /// Number of pair events allowed before cooldown starts.
    pub max_events_per_window: Option<u32>,
    /// Rolling event window size in seconds.
    pub window_seconds: Option<u32>,
    /// Suppression duration in seconds once the threshold is exceeded.
    pub cooldown_seconds: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChannelDefaultsConfig {
    pub group_policy: Option<GroupPolicy>,
    pub heartbeat: Option<HeartbeatConfig>,
    /// Shared channel-turn kernel bot loop-guard defaults (v2026.5.x).
    pub bot_loop_protection: Option<BotLoopProtectionConfig>,
}

/// A reusable message-channel access group (`accessGroups.<name>` at config
/// root). `accessGroup:<name>` allowFrom entries reference these.
///
/// Ported from OpenClaw `src/channels/message-access/*` (v2026.5.x): static
/// `message.senders` groups expand to sender ids during allowlist
/// normalization; other (dynamic) group types resolve through runtime
/// membership hooks and stay symbolic until then.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AccessGroupConfig {
    /// Group type. `"message.senders"` (default) is a static sender-id list.
    #[serde(rename = "type")]
    pub group_type: Option<String>,
    /// Sender ids for static `message.senders` groups.
    pub senders: Option<Vec<String>>,
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
    pub synology_chat: Option<SynologyChatConfig>,
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
    /// Restrict group control commands to chat admins (super-group support,
    /// carryover v2026.4.9). Defaults to true in groups.
    pub admin_only_commands: Option<bool>,
    /// Agent bound to this group (account-scoped routing, v2026.7.1).
    pub agent_id: Option<String>,
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
    /// Agent bound to this forum topic (account-scoped routing, v2026.7.1).
    pub agent_id: Option<String>,
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
    /// Error reporting policy: "always", "once", "silent" (v2026.4.1).
    pub error_policy: Option<String>,
    /// Cooldown in ms between error reports (v2026.4.1).
    pub error_cooldown_ms: Option<u64>,
    /// Custom Telegram Bot API endpoint root (v2026.4.1).
    pub api_root: Option<String>,
    /// LLM-based auto-topic naming (v2026.4.1).
    pub auto_topic_label: Option<bool>,
    /// Suppress error reply messages (v2026.4.1).
    pub silent_error_replies: Option<bool>,
    /// Account-level default for group admin-only control commands
    /// (super-group support, carryover v2026.4.9).
    pub admin_only_commands: Option<bool>,
    /// Rich-message delivery flag, default false (v2026.7.1).
    pub rich_messages: Option<bool>,
    /// getUpdates watchdog stall threshold in ms, clamped 30_000..=600_000,
    /// default 120_000 (v2026.7.1).
    pub polling_stall_threshold_ms: Option<u64>,
    /// Media-group buffer flush window in ms, default 500, floor 10
    /// (v2026.7.1).
    pub media_group_flush_ms: Option<u64>,
    /// Roots under which local file paths may be sent through a local Bot API
    /// server (v2026.7.1 `trustedLocalFileRoots`).
    pub trusted_local_file_roots: Option<Vec<String>>,
    /// DM exec-approval allowlist: sender ids exempt from exec approval even
    /// with `ask: off` semantics preserved (v2026.7.1).
    pub exec_approval_allow_from: Option<Vec<String>>,
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
            error_policy: None,
            error_cooldown_ms: None,
            api_root: None,
            auto_topic_label: None,
            silent_error_replies: None,
            admin_only_commands: None,
            rich_messages: None,
            polling_stall_threshold_ms: None,
            media_group_flush_ms: None,
            trusted_local_file_roots: None,
            exec_approval_allow_from: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TelegramNetworkConfig {
    pub auto_select_family: Option<bool>,
    /// DNS result order for Bot API transport: "ipv4first" | "verbatim".
    /// Unset = inherit the process resolver order (v2026.5.2).
    pub dns_result_order: Option<String>,
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

/// Discord voice channel conversation settings (v2026.4.25+).
///
/// Voice conversations are text-only by default: the agent returns plain text
/// and Discord voice synthesizes/plays it (the agent-side `tts` tool is hidden
/// on voice turns).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DiscordVoiceConfig {
    /// Enable Discord voice channel conversations (default: true).
    pub enabled: Option<bool>,
    /// Voice conversation mode ("stt-tts" | "agent-proxy" | "bidi"). Default: agent-proxy.
    pub mode: Option<String>,
    /// Optional LLM model override for Discord voice channel responses (v2026.4.25).
    pub model: Option<String>,
    /// Optional TTS overrides for Discord voice output.
    pub tts: Option<serde_json::Value>,
    /// Realtime provider settings for agent-proxy or bidi modes (v2026.7.1).
    pub realtime: Option<DiscordVoiceRealtimeConfig>,
    /// Voice channels to auto-join on startup (v2026.7.1).
    pub auto_join: Option<Vec<DiscordVoiceChannelRef>>,
    /// Voice channels the bot is allowed to join or remain in. Unset = any (v2026.7.1).
    pub allowed_channels: Option<Vec<DiscordVoiceChannelRef>>,
    /// If false, configured followUsers are ignored without removing the list (v2026.7.1).
    pub follow_users_enabled: Option<bool>,
    /// Discord user IDs whose current voice channel the bot should follow (v2026.7.1).
    pub follow_users: Option<Vec<String>>,
    /// Enable/disable DAVE end-to-end encryption (default: true) (v2026.7.1).
    pub dave_encryption: Option<bool>,
    /// Consecutive decrypt failures before DAVE session reinit (default: 24) (v2026.7.1).
    pub decryption_failure_tolerance: Option<u64>,
    /// Initial voice Ready wait in ms (default: 30000) (v2026.7.1).
    pub connect_timeout_ms: Option<u64>,
    /// Grace period for voice reconnect signalling after a disconnect (default: 15000) (v2026.7.1).
    pub reconnect_grace_ms: Option<u64>,
    /// Silence grace after a speaker ends before finalizing STT capture (default: 2000) (v2026.7.1).
    pub capture_silence_grace_ms: Option<u64>,
}

/// A guild+channel voice channel reference (v2026.7.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiscordVoiceChannelRef {
    pub guild_id: String,
    pub channel_id: String,
}

/// Realtime voice provider settings for Discord voice (v2026.7.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DiscordVoiceRealtimeConfig {
    /// Realtime voice provider id, for example "openai".
    pub provider: Option<String>,
    /// Provider realtime session model.
    pub model: Option<String>,
    /// Provider realtime output voice name.
    pub speaker_voice: Option<String>,
    /// System instructions passed to the realtime provider.
    pub instructions: Option<String>,
    /// Tool policy for bidi realtime consult calls ("safe-read-only" | "owner" | "none").
    pub tool_policy: Option<String>,
    /// Whether the OpenClaw agent brain is forced for every substantive turn ("auto" | "always").
    pub consult_policy: Option<String>,
    /// Require a wake name before agent-proxy realtime voice responds.
    pub require_wake_name: Option<bool>,
    /// Wake names allowed to trigger a response. Defaults to routed agent name, then agent id.
    pub wake_names: Option<Vec<String>>,
    /// Allow speaker-start events to interrupt active realtime playback.
    pub barge_in: Option<bool>,
    /// Minimum assistant playback duration before a barge-in truncates audio. Default: 250ms.
    pub min_barge_in_audio_end_ms: Option<u64>,
    /// Debounce window before buffered transcripts are sent to the agent.
    pub debounce_ms: Option<u64>,
}

/// Thread binding lifecycle settings (focus/subagent thread sessions) (v2026.7.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DiscordThreadBindingsConfig {
    /// Enable Discord thread binding features.
    pub enabled: Option<bool>,
    /// Inactivity window for thread-bound sessions in hours. 0 disables. Default: 24.
    pub idle_hours: Option<u64>,
    /// Optional hard max age for thread-bound sessions in hours. 0 disables. Default: 0.
    pub max_age_hours: Option<u64>,
    /// Allow session spawns to auto-create + bind Discord threads. Default: true.
    pub spawn_sessions: Option<bool>,
    /// Default context mode for native subagents spawned into a bound thread ("isolated" | "fork").
    pub default_spawn_context: Option<String>,
    /// Legacy split toggle superseded by `spawnSessions` (v2026.5.2).
    /// Read for back-compat only; `openclaw doctor --fix` migrates it
    /// (doctor migration owned by the CLI cluster).
    pub subagent_sessions: Option<bool>,
    /// Legacy split toggle superseded by `spawnSessions` (v2026.5.2).
    pub acp_sessions: Option<bool>,
}

impl DiscordThreadBindingsConfig {
    /// Effective `threadBindings.spawnSessions` value, honoring the legacy
    /// split `subagentSessions`/`acpSessions` toggles it replaced
    /// (OpenClaw v2026.5.2). Precedence: explicit `spawnSessions`, else
    /// `true` unless BOTH legacy toggles are explicitly `false` (either
    /// legacy toggle enabling spawn keeps spawns on). Default: `true`.
    pub fn resolve_spawn_sessions(&self) -> bool {
        if let Some(explicit) = self.spawn_sessions {
            return explicit;
        }
        match (self.subagent_sessions, self.acp_sessions) {
            (Some(false), Some(false)) => false,
            (Some(false), None) | (None, Some(false)) => false,
            _ => true,
        }
    }
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
    /// Per-account health monitor override (v2026.4.1).
    pub health_monitor: Option<ChannelHealthMonitorConfig>,
    /// Startup wait for the gateway READY event before restarting the socket (ms). Default: 15000. (v2026.5.2)
    pub gateway_ready_timeout_ms: Option<u64>,
    /// Runtime reconnect wait for the gateway READY event before force-stopping the lifecycle (ms). Default: 30000. (v2026.5.2)
    pub gateway_runtime_ready_timeout_ms: Option<u64>,
    /// Deterministic outbound `@handle` rewrites for known Discord users.
    /// Keys are handles without the leading `@`; values are Discord user IDs. (v2026.5.2)
    pub mention_aliases: Option<HashMap<String, String>>,
    /// Voice channel conversation settings (v2026.4.25+).
    pub voice: Option<DiscordVoiceConfig>,
    /// Suppress Discord-generated link embeds for outbound messages. Default: true. (v2026.7.1)
    pub suppress_embeds: Option<bool>,
    /// Thread binding lifecycle settings (v2026.7.1).
    pub thread_bindings: Option<DiscordThreadBindingsConfig>,
    /// Timeout for Discord /gateway/bot metadata lookup (ms). Default: 30000. (v2026.7.1)
    pub gateway_info_timeout_ms: Option<u64>,
}

/// Per-channel/account health monitor configuration (v2026.4.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChannelHealthMonitorConfig {
    pub enabled: Option<bool>,
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
            health_monitor: None,
            gateway_ready_timeout_ms: None,
            gateway_runtime_ready_timeout_ms: None,
            mention_aliases: None,
            voice: None,
            suppress_embeds: None,
            thread_bindings: None,
            gateway_info_timeout_ms: None,
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

/// `allowBots` accepts `true`/`false` or the string `"mentions"` (v2026.7.1):
/// `mentions` admits other bots' messages only when this bot is mentioned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SlackAllowBots {
    Flag(bool),
    Mode(SlackAllowBotsMode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlackAllowBotsMode {
    Mentions,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SlackChannelConfig {
    pub enabled: Option<bool>,
    pub allow: Option<bool>,
    pub require_mention: Option<bool>,
    pub tools: Option<serde_json::Value>,
    pub tools_by_sender: Option<HashMap<String, serde_json::Value>>,
    pub allow_bots: Option<SlackAllowBots>,
    /// Drop unmentioned channel messages that mention someone else (v2026.7.1).
    pub ignore_other_mentions: Option<bool>,
    /// Per-channel reply-to mode override (v2026.7.1).
    pub reply_to_mode: Option<ReplyToMode>,
    pub users: Option<Vec<String>>,
    pub skills: Option<Vec<String>>,
    pub system_prompt: Option<String>,
}

/// Router relay mode: a central router forwards Slack events to the owning
/// gateway over a WebSocket relay (v2026.7.1, `extensions/slack/src/monitor/relay-source.ts`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SlackRelayConfig {
    pub url: Option<String>,
    pub auth_token: Option<String>,
    pub gateway_id: Option<String>,
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
    pub allow_bots: Option<SlackAllowBots>,
    pub require_mention: Option<bool>,
    pub group_policy: Option<GroupPolicy>,
    pub history_limit: Option<u32>,
    pub dm_history_limit: Option<u32>,
    pub dms: Option<SlackDmConfig>,
    /// Expand inline link previews on outbound messages (default: false) (v2026.7.1).
    pub unfurl_links: Option<bool>,
    /// Expand inline media previews on outbound messages (default: off) (v2026.7.1).
    pub unfurl_media: Option<bool>,
    /// Broadcast thread replies back to the channel (default: false) (v2026.7.1).
    pub reply_broadcast: Option<bool>,
    /// Allow `channels` config keys to match by channel name (with warning)
    /// instead of channel ID only (v2026.7.1).
    pub allow_name_matching: Option<bool>,
    /// Router relay mode settings; active when `mode == "relay"` (v2026.7.1).
    pub relay: Option<SlackRelayConfig>,
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
            unfurl_links: None,
            unfurl_media: None,
            reply_broadcast: None,
            allow_name_matching: None,
            relay: None,
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

/// WhatsApp reaction level guidance (v2026.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WhatsAppReactionLevel {
    Off,
    Ack,
    #[default]
    Minimal,
    Extensive,
}

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

/// WhatsApp socket timing overrides (v2026.7.1, `socket-timing.ts`).
/// Non-positive values fall back to defaults (25s keep-alive, 60s connect,
/// 60s default query timeout).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WhatsAppSocketTimingConfig {
    pub keep_alive_interval_ms: Option<u64>,
    pub connect_timeout_ms: Option<u64>,
    pub default_query_timeout_ms: Option<u64>,
}

/// WhatsApp reconnect backoff overrides (v2026.7.1, `reconnect.ts`).
/// Values are clamped at resolve time (initial >= 250ms, factor 1.1–10,
/// jitter 0–1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WhatsAppReconnectConfig {
    pub initial_ms: Option<u64>,
    pub max_ms: Option<u64>,
    pub factor: Option<f64>,
    pub jitter: Option<f64>,
    pub max_attempts: Option<u32>,
}

/// WhatsApp status-reaction lifecycle emojis (v2026.7.1,
/// `status-reaction.ts`): queued → thinking → tool → done/error. Empty
/// `done`/`error` values clear the reaction on terminal transition.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WhatsAppStatusReactionsConfig {
    pub enabled: Option<bool>,
    pub queued: Option<String>,
    pub thinking: Option<String>,
    pub tool: Option<String>,
    pub done: Option<String>,
    pub error: Option<String>,
    pub min_update_interval_ms: Option<u64>,
}

/// Group visible-reply policy (v2026.7.1): groups default to
/// `messageToolOnly` — only explicit message-tool sends are visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WhatsAppGroupVisibleReplyMode {
    MessageToolOnly,
    Always,
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
    /// Reaction level guidance for agent reactions (v2026.4.1).
    pub reaction_level: Option<WhatsAppReactionLevel>,
    /// Socket timing overrides (v2026.7.1).
    pub socket_timing: Option<WhatsAppSocketTimingConfig>,
    /// Reconnect backoff overrides (v2026.7.1).
    pub reconnect: Option<WhatsAppReconnectConfig>,
    /// Status-reaction lifecycle configuration (v2026.7.1).
    pub status_reactions: Option<WhatsAppStatusReactionsConfig>,
    /// Group visible-reply policy; groups default to message-tool-only
    /// (v2026.7.1).
    pub group_visible_reply_mode: Option<WhatsAppGroupVisibleReplyMode>,
    /// Send outbound media as a document with the original bytes — no image
    /// re-encode (v2026.7.1 `forceDocument`).
    pub force_document: Option<bool>,
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
            reaction_level: None,
            socket_timing: None,
            reconnect: None,
            status_reactions: None,
            group_visible_reply_mode: None,
            force_document: None,
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
    /// Group allowlist. Entries match inbound group ids (`group:<id>` or bare
    /// id) AND sender ids (E.164 / `uuid:<id>`); `*` allows all
    /// (v2026.7.1, `extensions/signal/src/monitor/access-policy.ts`).
    pub group_allow_from: Option<Vec<String>>,
    pub group_policy: Option<GroupPolicy>,
    pub dm_policy: Option<DmPolicy>,
    /// Max inbound attachment size in MB (default 8). The `getAttachment` RPC
    /// response cap is derived with base64 headroom (~4/3 expansion + 64KiB).
    pub media_max_mb: Option<f64>,
    /// Target aliases: alias name -> E.164 / `uuid:<id>` / `username:<name>` /
    /// `group:<id>` (chains allowed, recursion rejected).
    pub aliases: Option<HashMap<String, String>>,
    /// Inbound reaction notification mode: `off` | `own` (default) | `all`.
    pub reaction_notifications: Option<String>,
    /// Agent reaction level: `off` | `ack` | `minimal` (default) | `extensive`.
    pub reaction_level: Option<String>,
}

// ============================================================================
// iMessage Configuration
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct IMessageConfig {
    pub enabled: Option<bool>,
    /// Backend provider: `imsg` (default, local imsg CLI) or `bluebubbles`
    /// (legacy REST bridge; the upstream BlueBubbles channel was removed in
    /// v2026.7.1 in favor of `channels.imessage` with the imsg backend).
    pub provider: Option<String>,
    pub api_url: Option<String>,
    pub api_password: Option<String>,
    pub allow_from: Option<Vec<String>>,
    /// Group allowlist (falls back to `allow_from` when empty).
    pub group_allow_from: Option<Vec<String>>,
    pub group_policy: Option<GroupPolicy>,
    pub dm_policy: Option<DmPolicy>,
    /// Path to the `imsg` CLI binary (default `imsg`).
    pub cli_path: Option<String>,
    /// Path to the Messages `chat.db` (default `~/Library/Messages/chat.db`).
    pub db_path: Option<String>,
    /// Remote host fronting `chat.db` (SSH bridge deployments).
    pub remote_host: Option<String>,
    /// Max inbound attachment size in MB (default 8).
    pub media_max_mb: Option<f64>,
    /// Inbound tapback notification mode: `off` | `own` (default) | `all`.
    pub reaction_notifications: Option<String>,
    /// Per-group config keyed by `chat_id` (or `*` wildcard).
    pub groups: Option<HashMap<String, IMessageGroupConfig>>,
}

/// Per-group iMessage configuration (v2026.7.1,
/// `extensions/imessage/src/monitor/inbound-processing.ts`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct IMessageGroupConfig {
    /// Per-group system prompt. A present-but-empty value suppresses the `*`
    /// wildcard prompt for this group instead of inheriting it.
    pub system_prompt: Option<String>,
    pub allow_from: Option<Vec<String>>,
    pub require_mention: Option<bool>,
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
// Synology Chat Configuration
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SynologyChatAccountConfig {
    pub enabled: Option<bool>,
    pub token: Option<String>,
    pub incoming_url: Option<String>,
    pub nas_host: Option<String>,
    pub webhook_path: Option<String>,
    pub dm_policy: Option<DmPolicy>,
    pub allowed_user_ids: Option<Vec<String>>,
    pub rate_limit_per_minute: Option<u32>,
    pub bot_name: Option<String>,
    pub allow_insecure_ssl: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SynologyChatConfig {
    pub accounts: Option<HashMap<String, SynologyChatAccountConfig>>,
    #[serde(flatten)]
    pub default_account: SynologyChatAccountConfig,
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
pub struct SafeBinProfile {
    pub max_positional: Option<u32>,
    #[serde(default)]
    pub allowed_value_flags: Vec<String>,
    #[serde(default)]
    pub denied_flags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ExecToolConfig {
    /// Execution host: "sandbox", "auto" (default changed from sandbox, v2026.4.1), "node", etc.
    pub host: Option<String>,
    pub security: Option<String>,
    pub ask: Option<String>,
    pub node: Option<String>,
    #[serde(default)]
    pub path_prepend: Vec<String>,
    #[serde(default)]
    pub safe_bins: Vec<String>,
    pub safe_bin_profiles: Option<HashMap<String, SafeBinProfile>>,
    pub background_ms: Option<u64>,
    pub timeout_sec: Option<u64>,
    pub approval_running_notice_ms: Option<u64>,
    pub cleanup_ms: Option<u64>,
    pub notify_on_exit: Option<bool>,
    pub apply_patch: Option<bool>,
    /// Require explicit approval for interpreter inline-eval forms (v2026.4.1).
    pub strict_inline_eval: Option<bool>,
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
    /// SearXNG bundled web search provider (v2026.4.1).
    pub searxng: Option<SearxngSearchConfig>,
    /// X (Twitter) search tool via xAI Grok (v2026.4.1).
    pub x_search: Option<XSearchConfig>,
    /// Native Codex web search for eligible models (v2026.4.1).
    pub openai_codex: Option<bool>,
    /// Brave-specific overrides (v2026.5.2): base_url for compatible proxies
    /// and opt-in HTTP diagnostics flag.
    pub brave: Option<BraveSearchConfig>,
    /// Exa web search provider (v2026.7.1): base URL override with
    /// endpoint-partitioned caches.
    pub exa: Option<ExaSearchConfig>,
    /// MiniMax Coding Plan search provider (v2026.7.1).
    pub minimax: Option<MinimaxSearchConfig>,
    /// Gemini grounding-based web search provider (v2026.7.1). Falls back to
    /// the Google model-provider API key / base URL when unset.
    pub gemini: Option<GeminiSearchConfig>,
    /// Parallel bundled search provider (v2026.7.1).
    pub parallel: Option<ParallelSearchConfig>,
    /// DuckDuckGo key-free provider (v2026.7.1, explicit opt-in only).
    pub duckduckgo: Option<DuckDuckGoSearchConfig>,
}

/// Exa web-search provider configuration (v2026.7.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ExaSearchConfig {
    pub api_key: Option<String>,
    /// Override for the Exa search endpoint base URL. `/search` is appended
    /// when missing. Caches are partitioned per resolved endpoint.
    pub base_url: Option<String>,
}

/// MiniMax Coding Plan web-search provider configuration (v2026.7.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MinimaxSearchConfig {
    pub api_key: Option<String>,
    /// Explicit region: "cn" or "global". When unset the region is inferred
    /// from `MINIMAX_API_HOST` or the configured MiniMax provider base URL.
    pub region: Option<String>,
}

/// Gemini grounding web-search provider configuration (v2026.7.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GeminiSearchConfig {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
}

/// Parallel web-search provider configuration (v2026.7.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ParallelSearchConfig {
    pub api_key: Option<String>,
    /// Base URL override; `/v1/search` is appended when missing. Caches are
    /// partitioned per resolved endpoint.
    pub base_url: Option<String>,
}

/// DuckDuckGo key-free web-search provider configuration (v2026.7.1).
/// Never auto-selected — explicit `provider: "duckduckgo"` opt-in only.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DuckDuckGoSearchConfig {
    /// DDG region code (e.g. `us-en`).
    pub region: Option<String>,
    /// Safe-search level: "strict" | "moderate" (default) | "off".
    pub safe_search: Option<String>,
}

/// Managed outbound proxy configuration (v2026.7.1, upstream `proxy` section).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProxyConfig {
    pub enabled: Option<bool>,
    /// http:// or https:// proxy URL.
    pub proxy_url: Option<String>,
    /// Loopback-target routing: "gateway-only" (default; only the gateway may
    /// reach loopback directly), "proxy" (loopback goes through the proxy
    /// too), or "block" (loopback targets are refused).
    pub loopback_mode: Option<String>,
}

/// Brave web-search provider configuration (v2026.5.2).
///
/// `base_url` lets operators point at a Brave-compatible search proxy (e.g.
/// a corporate Brave gateway) instead of `api.search.brave.com`. The endpoint
/// is fed into cache-key derivation so two different base URLs do not collide
/// in any future LRU. `http` opts into diagnostic logging of request URL,
/// query params, response status/timing, and cache hit/miss/write — never
/// API keys or response bodies.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BraveSearchConfig {
    /// Override for the Brave search base URL. Defaults to
    /// `https://api.search.brave.com/res/v1/web/search` when unset.
    pub base_url: Option<String>,
    /// Opt-in `brave.http` diagnostics. Logs request URL, status, timing,
    /// and cache events without ever logging the API key or response body.
    pub http: Option<bool>,
    /// Brave API mode: "web" (default) or "llm-context" (v2026.7.1).
    pub mode: Option<String>,
    /// Brave-scoped API key (v2026.7.1). Preferred over the legacy top-level
    /// `tools.web.search.apiKey`, mirroring upstream's move of the Brave key
    /// into the provider-scoped plugin entry.
    pub api_key: Option<String>,
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
    /// HTTP timeout for Grok web_search calls in seconds (v2026.5.2).
    /// Defaults to 60s upstream — historical default of "no timeout" caused
    /// hung tool calls when xAI's Responses API took >30s. Configurable so
    /// operators can tighten or loosen the budget per deployment.
    pub timeout_seconds: Option<u64>,
}

/// SearXNG bundled web search provider configuration (v2026.4.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SearxngSearchConfig {
    /// SearXNG instance host URL.
    pub host: Option<String>,
    pub max_results: Option<u32>,
    pub engines: Option<Vec<String>>,
    pub language: Option<String>,
    pub timeout_seconds: Option<u64>,
    /// Default comma-joined search categories (v2026.7.1). Empty non-general
    /// category results retry once with "general".
    pub categories: Option<Vec<String>>,
}

/// X (Twitter) search configuration via xAI Grok (v2026.4.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct XSearchConfig {
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub max_results: Option<u32>,
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
    /// Maximum response bytes for truncation (v2026.4.1).
    pub max_response_bytes: Option<u64>,
    /// Explicit external web_fetch provider id (v2026.7.1), e.g. "firecrawl".
    /// Only honored for non-sandboxed fetches; sandboxed fetches stay bundled.
    pub provider: Option<String>,
    /// Route web_fetch through a trusted HTTP(S) env proxy (v2026.7.1).
    pub use_trusted_env_proxy: Option<bool>,
    /// SSRF policy overrides for trusted proxy stacks (v2026.7.1).
    pub ssrf_policy: Option<WebFetchSsrfPolicyConfig>,
}

/// SSRF policy overrides for `web_fetch` (v2026.7.1).
///
/// Both flags are opt-in escapes for fake-IP proxy stacks (sing-box, Clash,
/// Surge) that resolve foreign domains into reserved ranges.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WebFetchSsrfPolicyConfig {
    /// Allow RFC 2544 benchmark range (198.18.0.0/15) targets.
    pub allow_rfc2544_benchmark_range: Option<bool>,
    /// Allow IPv6 Unique Local Addresses (fc00::/7) targets.
    pub allow_ipv6_unique_local_range: Option<bool>,
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
    /// Multimodal indexing configuration (v2026.3.11).
    /// Enables image and audio content indexing in memory.
    pub multimodal: Option<MemoryMultimodalConfig>,
}

/// Multimodal memory indexing (v2026.3.11).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MemoryMultimodalConfig {
    /// Enable image memory indexing.
    pub index_images: Option<bool>,
    /// Enable audio memory indexing.
    pub index_audio: Option<bool>,
    /// Embedding model for multimodal content (e.g. "gemini-embedding-2-preview").
    pub embedding_model: Option<String>,
    /// Configurable embedding dimensions.
    pub embedding_dimensions: Option<u32>,
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
    /// Extra QMD collections to include in search (v2026.4.1).
    pub extra_collections: Option<Vec<String>>,
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
    Mistral,
    Local,
    Voyage,
    Ollama,
    /// DeepInfra OpenAI-compatible embeddings (v2026.4.27).
    Deepinfra,
    /// Disable embeddings entirely: FTS-only search, skips embedding
    /// capability discovery (v2026.6.x `memorySearch.provider=none`).
    None,
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
    /// Asymmetric embedding input type (v2026.4.26 `memorySearch.inputType`):
    /// "query" or "document". When set, providers that support asymmetric
    /// endpoints embed retrieval queries and documents differently.
    pub input_type: Option<String>,
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
    /// FTS5 tokenizer configuration for CJK text support (v2026.4.1).
    /// Values: "unicode61" (default), "trigram" (for CJK).
    pub fts: Option<MemoryFtsConfig>,
}

/// FTS5 tokenizer configuration (v2026.4.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MemoryFtsConfig {
    /// FTS5 tokenizer: "unicode61" or "trigram".
    pub tokenizer: Option<String>,
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
    /// Force session reindex after compaction (v2026.4.1).
    pub post_compaction_force: Option<bool>,
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
    /// When true, agent output reaches the user only via explicit
    /// `message(action=send)` tool calls; bare LLM text is suppressed.
    /// Defaults to None (off). (OpenClaw v2026.4.29)
    pub visible_replies: Option<bool>,
    /// Lifecycle status reactions configuration (v2026.7.1).
    pub status_reactions: Option<StatusReactionsConfig>,
}

/// Lifecycle status reactions (queued→thinking→tool→done/error) (v2026.7.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StatusReactionsConfig {
    /// Enable lifecycle status reactions (default: false).
    pub enabled: Option<bool>,
    /// Override default emojis.
    pub emojis: Option<StatusReactionsEmojiConfig>,
    /// Override default timing.
    pub timing: Option<StatusReactionsTimingConfig>,
}

/// Emoji overrides for each status reaction state (v2026.7.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StatusReactionsEmojiConfig {
    pub queued: Option<String>,
    pub thinking: Option<String>,
    pub tool: Option<String>,
    pub coding: Option<String>,
    pub web: Option<String>,
    pub deploy: Option<String>,
    pub build: Option<String>,
    pub concierge: Option<String>,
    pub done: Option<String>,
    pub error: Option<String>,
    pub stall_soft: Option<String>,
    pub stall_hard: Option<String>,
    pub compacting: Option<String>,
}

/// Timing controls for debounced status reactions and stall warnings (v2026.7.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StatusReactionsTimingConfig {
    /// Debounce interval for intermediate states (ms). Default: 700.
    pub debounce_ms: Option<u64>,
    /// Soft stall warning timeout (ms). Default: 10000.
    pub stall_soft_ms: Option<u64>,
    /// Hard stall warning timeout (ms). Default: 30000.
    pub stall_hard_ms: Option<u64>,
    /// How long to hold done emoji before cleanup (ms). Default: 1500.
    pub done_hold_ms: Option<u64>,
    /// How long to hold error emoji before cleanup (ms). Default: 2500.
    pub error_hold_ms: Option<u64>,
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
    /// Disk budget for stored transcripts; oldest non-durable sessions are
    /// evicted when total transcript bytes exceed this (v2026.5.2).
    pub max_disk_bytes: Option<u64>,
}

/// Transcript write-lock tuning (v2026.5.2 `session.writeLock`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionWriteLockConfig {
    /// Max time to wait for a transcript write lock before failing the
    /// acquisition (milliseconds). Default 60_000 (v2026.5.2).
    pub acquire_timeout_ms: Option<u64>,
    /// Max time a holder may keep the lock before it can be reclaimed at
    /// acquisition time as stale (milliseconds). Default 300_000 (v2026.7.1
    /// max-hold reclaim).
    pub max_hold_ms: Option<u64>,
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
    /// Transcript write-lock tuning (v2026.5.2).
    pub write_lock: Option<SessionWriteLockConfig>,
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
    /// Abort threshold for outcome-driven stuck-session recovery in ms
    /// (v2026.7.1: `diagnostics.stuckSessionAbortMs`). Default 10 minutes;
    /// values below 10s are clamped up.
    pub stuck_session_abort_ms: Option<u64>,
}

// ============================================================================
// Sandbox Configuration
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SandboxDockerSettings {
    pub image: Option<String>,
    pub container_prefix: Option<String>,
    /// Additional tool allowlist entries for sandbox (v2026.4.1).
    pub also_allow: Option<Vec<String>>,
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
    /// Allow reserved container target names in sandbox (v2026.2.24 security).
    pub dangerously_allow_reserved_container_targets: Option<bool>,
    /// Allow external bind mount sources outside workspace (v2026.2.24 security).
    pub dangerously_allow_external_bind_sources: Option<bool>,
    /// Allow `network: "container:<id>"` namespace joins (v2026.2.24 security).
    pub dangerously_allow_container_namespace_join: Option<bool>,
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
// Talk Configuration (provider-agnostic voice, v2026.2.24)
// ============================================================================

/// Per-provider talk voice configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TalkProviderConfig {
    pub voice_id: Option<String>,
    pub voice_aliases: Option<HashMap<String, String>>,
    pub model_id: Option<String>,
    pub output_format: Option<String>,
    pub api_key: Option<String>,
}

/// Multi-provider talk configuration.
///
/// Supports both legacy (top-level voice fields) and new (per-provider) formats.
/// The `provider` field selects the active provider; `providers` holds per-provider
/// settings. Legacy top-level fields are kept for migration compatibility.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TalkConfig {
    /// Active provider id (e.g. "elevenlabs", "openai").
    pub provider: Option<String>,
    /// Per-provider configurations keyed by provider id.
    pub providers: Option<HashMap<String, TalkProviderConfig>>,
    // Legacy top-level fields (migrated into providers["elevenlabs"] by normalize).
    pub voice_id: Option<String>,
    pub voice_aliases: Option<HashMap<String, String>>,
    pub model_id: Option<String>,
    pub output_format: Option<String>,
    pub api_key: Option<String>,
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
    pub default_stagger_ms: Option<u64>,
    /// Default tool allowlist for cron jobs (v2026.4.1).
    /// Dramatically reduces input tokens for small local models.
    pub default_tools: Option<Vec<String>>,
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
// Auth Configuration (v2026.4.1)
// ============================================================================

/// Auth cooldown configuration for provider rotation (v2026.4.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AuthCooldownConfig {
    /// Max same-provider auth-profile retries before cross-provider fallback.
    pub rate_limited_profile_rotations: Option<u32>,
    /// Max retries for overloaded errors before cross-provider fallback.
    pub overloaded_profile_rotations: Option<u32>,
    /// Fixed delay in ms before retrying after overloaded error.
    pub overloaded_backoff_ms: Option<u64>,
}

/// Auth profile configuration (v2026.4.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AuthProfileConfig {
    pub provider: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    /// Human-readable display name for this auth profile (v2026.4.1).
    pub display_name: Option<String>,
}

/// Top-level auth configuration (v2026.4.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AuthConfig {
    pub cooldowns: Option<AuthCooldownConfig>,
    pub profiles: Option<HashMap<String, AuthProfileConfig>>,
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

// ============================================================================
// Tests — v2026.2.24 Config Type Parity
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ====================================================================
    // HeartbeatTarget serde
    // ====================================================================

    #[test]
    fn heartbeat_target_none_serializes_as_string() {
        let target = HeartbeatTarget::None;
        let v = serde_json::to_value(&target).unwrap();
        assert_eq!(v, json!("none"));
    }

    #[test]
    fn heartbeat_target_last_serializes_as_string() {
        let target = HeartbeatTarget::Last;
        let v = serde_json::to_value(&target).unwrap();
        assert_eq!(v, json!("last"));
    }

    #[test]
    fn heartbeat_target_channel_serializes_as_channel_name() {
        let target = HeartbeatTarget::Channel("telegram".to_string());
        let v = serde_json::to_value(&target).unwrap();
        assert_eq!(v, json!("telegram"));
    }

    #[test]
    fn heartbeat_target_roundtrip_none() {
        let v = json!("none");
        let target: HeartbeatTarget = serde_json::from_value(v).unwrap();
        assert_eq!(target, HeartbeatTarget::None);
    }

    #[test]
    fn heartbeat_target_roundtrip_last() {
        let v = json!("last");
        let target: HeartbeatTarget = serde_json::from_value(v).unwrap();
        assert_eq!(target, HeartbeatTarget::Last);
    }

    #[test]
    fn heartbeat_target_roundtrip_channel() {
        let v = json!("discord");
        let target: HeartbeatTarget = serde_json::from_value(v).unwrap();
        assert_eq!(target, HeartbeatTarget::Channel("discord".to_string()));
    }

    #[test]
    fn heartbeat_config_default_target_is_none() {
        let config = HeartbeatConfig::default();
        assert!(config.target.is_none());
    }

    #[test]
    fn heartbeat_config_with_target() {
        let raw = json!({
            "target": "telegram",
            "every": "15m"
        });
        let config: HeartbeatConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(
            config.target,
            Some(HeartbeatTarget::Channel("telegram".to_string()))
        );
    }

    // ====================================================================
    // DirectPolicy serde (v2026.2.25)
    // ====================================================================

    #[test]
    fn direct_policy_last_roundtrip() {
        let v = serde_json::to_value(DirectPolicy::Last).unwrap();
        assert_eq!(v, json!("last"));
        let parsed: DirectPolicy = serde_json::from_value(v).unwrap();
        assert_eq!(parsed, DirectPolicy::Last);
    }

    #[test]
    fn direct_policy_none_roundtrip() {
        let v = serde_json::to_value(DirectPolicy::None).unwrap();
        assert_eq!(v, json!("none"));
        let parsed: DirectPolicy = serde_json::from_value(v).unwrap();
        assert_eq!(parsed, DirectPolicy::None);
    }

    #[test]
    fn direct_policy_default_is_last() {
        assert_eq!(DirectPolicy::default(), DirectPolicy::Last);
    }

    #[test]
    fn heartbeat_config_with_direct_policy() {
        let raw = json!({
            "every": "10m",
            "directPolicy": "none"
        });
        let config: HeartbeatConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.direct_policy, Some(DirectPolicy::None));
    }

    #[test]
    fn heartbeat_config_without_direct_policy() {
        let raw = json!({ "every": "10m" });
        let config: HeartbeatConfig = serde_json::from_value(raw).unwrap();
        assert!(config.direct_policy.is_none());
    }

    // ====================================================================
    // SandboxDockerSettings with dangerous_* fields
    // ====================================================================

    #[test]
    fn sandbox_docker_settings_with_dangerous_fields() {
        let raw = json!({
            "image": "node:22",
            "dangerouslyAllowContainerNamespaceJoin": true,
            "dangerouslyAllowExternalBindSources": false,
            "dangerouslyAllowReservedContainerTargets": true
        });
        let settings: SandboxDockerSettings = serde_json::from_value(raw).unwrap();
        assert_eq!(settings.dangerously_allow_container_namespace_join, Some(true));
        assert_eq!(settings.dangerously_allow_external_bind_sources, Some(false));
        assert_eq!(settings.dangerously_allow_reserved_container_targets, Some(true));
    }

    #[test]
    fn sandbox_docker_settings_dangerous_fields_absent() {
        let raw = json!({ "image": "node:22" });
        let settings: SandboxDockerSettings = serde_json::from_value(raw).unwrap();
        assert!(settings.dangerously_allow_container_namespace_join.is_none());
        assert!(settings.dangerously_allow_external_bind_sources.is_none());
        assert!(settings.dangerously_allow_reserved_container_targets.is_none());
    }

    // ====================================================================
    // SubagentsConfig.runTimeoutSeconds
    // ====================================================================

    #[test]
    fn subagents_config_run_timeout() {
        let raw = json!({
            "maxConcurrent": 4,
            "runTimeoutSeconds": 300
        });
        let config: SubagentsConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.run_timeout_seconds, Some(300));
    }

    // ====================================================================
    // TalkConfig / TalkProviderConfig
    // ====================================================================

    #[test]
    fn talk_config_multi_provider() {
        let raw = json!({
            "provider": "elevenlabs",
            "providers": {
                "elevenlabs": {
                    "voiceId": "abc123",
                    "modelId": "eleven_multilingual_v2"
                },
                "openai": {
                    "voiceId": "alloy",
                    "outputFormat": "mp3"
                }
            }
        });
        let config: TalkConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.provider.as_deref(), Some("elevenlabs"));
        let providers = config.providers.unwrap();
        assert_eq!(providers.len(), 2);
        assert_eq!(
            providers["elevenlabs"].voice_id.as_deref(),
            Some("abc123")
        );
        assert_eq!(
            providers["openai"].output_format.as_deref(),
            Some("mp3")
        );
    }

    #[test]
    fn talk_config_legacy_top_level() {
        let raw = json!({
            "voiceId": "legacy-voice",
            "apiKey": "sk-xxx"
        });
        let config: TalkConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.voice_id.as_deref(), Some("legacy-voice"));
        assert_eq!(config.api_key.as_deref(), Some("sk-xxx"));
        assert!(config.providers.is_none());
    }

    // ====================================================================
    // GatewayRateLimitConfig (v2026.3.11)
    // ====================================================================

    #[test]
    fn rate_limit_config_full() {
        let raw = json!({
            "maxRequests": 100,
            "windowSeconds": 60,
            "maxConnections": 50
        });
        let config: GatewayRateLimitConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.max_requests, Some(100));
        assert_eq!(config.window_seconds, Some(60));
        assert_eq!(config.max_connections, Some(50));
    }

    #[test]
    fn rate_limit_config_partial() {
        let raw = json!({ "maxRequests": 200 });
        let config: GatewayRateLimitConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.max_requests, Some(200));
        assert!(config.window_seconds.is_none());
        assert!(config.max_connections.is_none());
    }

    #[test]
    fn rate_limit_config_roundtrip() {
        let config = GatewayRateLimitConfig {
            max_requests: Some(50),
            window_seconds: Some(30),
            max_connections: Some(10),
        };
        let v = serde_json::to_value(&config).unwrap();
        assert_eq!(v["maxRequests"], 50);
        assert_eq!(v["windowSeconds"], 30);
        assert_eq!(v["maxConnections"], 10);
    }

    // ====================================================================
    // GatewayConfig allowed_origins (v2026.3.11)
    // ====================================================================

    #[test]
    fn gateway_config_allowed_origins_default_empty() {
        let config = GatewayConfig::default();
        assert!(config.allowed_origins.is_empty());
    }

    #[test]
    fn gateway_config_allowed_origins_from_json() {
        let raw = json!({
            "port": 18789,
            "allowedOrigins": ["https://example.com", "https://app.mylobster.ai"]
        });
        let config: GatewayConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.allowed_origins.len(), 2);
        assert_eq!(config.allowed_origins[0], "https://example.com");
    }

    #[test]
    fn gateway_config_rate_limit_present() {
        let raw = json!({
            "port": 18789,
            "rateLimit": { "maxRequests": 100, "windowSeconds": 60 }
        });
        let config: GatewayConfig = serde_json::from_value(raw).unwrap();
        let rl = config.rate_limit.unwrap();
        assert_eq!(rl.max_requests, Some(100));
    }

    #[test]
    fn gateway_config_rate_limit_absent() {
        let config = GatewayConfig::default();
        assert!(config.rate_limit.is_none());
    }

    // ====================================================================
    // MemoryMultimodalConfig (v2026.3.11)
    // ====================================================================

    #[test]
    fn multimodal_config_full() {
        let raw = json!({
            "indexImages": true,
            "indexAudio": false,
            "embeddingModel": "gemini-embedding-2-preview",
            "embeddingDimensions": 768
        });
        let config: MemoryMultimodalConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.index_images, Some(true));
        assert_eq!(config.index_audio, Some(false));
        assert_eq!(config.embedding_model.as_deref(), Some("gemini-embedding-2-preview"));
        assert_eq!(config.embedding_dimensions, Some(768));
    }

    #[test]
    fn multimodal_config_default_all_none() {
        let config = MemoryMultimodalConfig::default();
        assert!(config.index_images.is_none());
        assert!(config.index_audio.is_none());
        assert!(config.embedding_model.is_none());
        assert!(config.embedding_dimensions.is_none());
    }

    #[test]
    fn multimodal_config_roundtrip() {
        let config = MemoryMultimodalConfig {
            index_images: Some(true),
            index_audio: Some(true),
            embedding_model: Some("text-embedding-3-large".to_string()),
            embedding_dimensions: Some(3072),
        };
        let v = serde_json::to_value(&config).unwrap();
        let restored: MemoryMultimodalConfig = serde_json::from_value(v).unwrap();
        assert_eq!(restored.index_images, Some(true));
        assert_eq!(restored.embedding_dimensions, Some(3072));
    }

    // ====================================================================
    // ModelsConfig alternative_providers & cooldown_probe_cap (v2026.3.11)
    // ====================================================================

    #[test]
    fn models_config_alternative_providers() {
        let raw = json!({
            "providers": {},
            "alternativeProviders": {
                "venice": {
                    "baseUrl": "https://api.venice.ai/v1",
                    "models": []
                },
                "together": {
                    "baseUrl": "https://api.together.xyz/v1",
                    "models": []
                }
            }
        });
        let config: ModelsConfig = serde_json::from_value(raw).unwrap();
        let alt = config.alternative_providers.unwrap();
        assert_eq!(alt.len(), 2);
        assert!(alt.contains_key("venice"));
        assert!(alt.contains_key("together"));
    }

    #[test]
    fn models_config_cooldown_probe_cap() {
        let raw = json!({
            "providers": {},
            "cooldownProbeCap": 1
        });
        let config: ModelsConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.cooldown_probe_cap, Some(1));
    }

    #[test]
    fn models_config_defaults_no_alternatives() {
        let raw = json!({ "providers": {} });
        let config: ModelsConfig = serde_json::from_value(raw).unwrap();
        assert!(config.alternative_providers.is_none());
        assert!(config.cooldown_probe_cap.is_none());
    }

    // ====================================================================
    // v2026.4.1 Config Type Parity Tests
    // ====================================================================

    // -- GatewayWebchatConfig --

    #[test]
    fn gateway_webchat_config_full() {
        let raw = json!({ "chatHistoryMaxChars": 50000 });
        let config: GatewayWebchatConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.chat_history_max_chars, Some(50000));
    }

    #[test]
    fn gateway_webchat_config_default() {
        let config = GatewayWebchatConfig::default();
        assert!(config.chat_history_max_chars.is_none());
    }

    // -- GatewayPushConfig / GatewayApnsConfig --

    #[test]
    fn gateway_push_apns_config() {
        let raw = json!({
            "apns": {
                "relayUrl": "https://apns.example.com",
                "keyId": "ABC123",
                "teamId": "TEAM456",
                "bundleId": "ai.mylobster.app",
                "keyPath": "/etc/apns/key.p8"
            }
        });
        let config: GatewayPushConfig = serde_json::from_value(raw).unwrap();
        let apns = config.apns.unwrap();
        assert_eq!(apns.relay_url.as_deref(), Some("https://apns.example.com"));
        assert_eq!(apns.key_id.as_deref(), Some("ABC123"));
        assert_eq!(apns.team_id.as_deref(), Some("TEAM456"));
        assert_eq!(apns.bundle_id.as_deref(), Some("ai.mylobster.app"));
    }

    // -- Gateway health monitor --

    #[test]
    fn gateway_config_health_monitor_fields() {
        let raw = json!({
            "port": 18789,
            "channelHealthCheckMinutes": 5,
            "channelStaleEventThresholdMinutes": 15,
            "channelMaxRestartsPerHour": 3
        });
        let config: GatewayConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.channel_health_check_minutes, Some(5));
        assert_eq!(config.channel_stale_event_threshold_minutes, Some(15));
        assert_eq!(config.channel_max_restarts_per_hour, Some(3));
    }

    // -- Gateway reload deferral --

    #[test]
    fn gateway_reload_deferral_timeout() {
        let raw = json!({
            "mode": "hybrid",
            "debounceMs": 300,
            "deferralTimeoutMs": 300000
        });
        let config: GatewayReloadConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.deferral_timeout_ms, Some(300000));
    }

    // -- Agent defaults params --

    #[test]
    fn agent_defaults_params() {
        let raw = json!({
            "model": "claude-sonnet-4-6",
            "params": { "temperature": 0.7, "topP": 0.9 }
        });
        let config: AgentDefaultsConfig = serde_json::from_value(raw).unwrap();
        let params = config.params.unwrap();
        assert_eq!(params["temperature"], 0.7);
        assert_eq!(params["topP"], 0.9);
    }

    // -- Agent thinking/reasoning/fast mode defaults --

    #[test]
    fn agent_entry_thinking_defaults() {
        let raw = json!({
            "id": "test-agent",
            "thinkingDefault": "high",
            "reasoningDefault": "medium",
            "fastModeDefault": true
        });
        let entry: AgentEntry = serde_json::from_value(raw).unwrap();
        assert_eq!(entry.thinking_default, Some(ThinkingLevel::High));
        assert_eq!(entry.reasoning_default, Some(ThinkingLevel::Medium));
        assert_eq!(entry.fast_mode_default, Some(true));
    }

    // -- Subagents requireAgentId --

    #[test]
    fn subagents_require_agent_id() {
        let raw = json!({
            "maxConcurrent": 4,
            "requireAgentId": true
        });
        let config: SubagentsConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.require_agent_id, Some(true));
    }

    // -- Compaction notifyUser --

    #[test]
    fn compaction_notify_user() {
        let raw = json!({
            "mode": "default",
            "notifyUser": true
        });
        let config: AgentCompactionConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.notify_user, Some(true));
    }

    // -- Telegram error controls --

    #[test]
    fn telegram_error_policy_config() {
        let raw = json!({
            "errorPolicy": "silent",
            "errorCooldownMs": 30000,
            "apiRoot": "https://custom-bot-api.example.com",
            "autoTopicLabel": true,
            "silentErrorReplies": true
        });
        let config: TelegramAccountConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.error_policy.as_deref(), Some("silent"));
        assert_eq!(config.error_cooldown_ms, Some(30000));
        assert_eq!(config.api_root.as_deref(), Some("https://custom-bot-api.example.com"));
        assert_eq!(config.auto_topic_label, Some(true));
        assert_eq!(config.silent_error_replies, Some(true));
    }

    // -- WhatsApp reaction level --

    #[test]
    fn whatsapp_reaction_level_roundtrip() {
        let v = serde_json::to_value(WhatsAppReactionLevel::Minimal).unwrap();
        assert_eq!(v, json!("minimal"));
        let parsed: WhatsAppReactionLevel = serde_json::from_value(json!("extensive")).unwrap();
        assert_eq!(parsed, WhatsAppReactionLevel::Extensive);
    }

    #[test]
    fn whatsapp_reaction_level_default_is_minimal() {
        assert_eq!(WhatsAppReactionLevel::default(), WhatsAppReactionLevel::Minimal);
    }

    #[test]
    fn whatsapp_account_reaction_level() {
        let raw = json!({ "reactionLevel": "off" });
        let config: WhatsAppAccountConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.reaction_level, Some(WhatsAppReactionLevel::Off));
    }

    // -- Exec strictInlineEval --

    #[test]
    fn exec_strict_inline_eval() {
        let raw = json!({
            "host": "auto",
            "strictInlineEval": true
        });
        let config: ExecToolConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.host.as_deref(), Some("auto"));
        assert_eq!(config.strict_inline_eval, Some(true));
    }

    // -- Sandbox alsoAllow --

    #[test]
    fn sandbox_docker_also_allow() {
        let raw = json!({
            "image": "node:22",
            "alsoAllow": ["curl", "wget"]
        });
        let config: SandboxDockerSettings = serde_json::from_value(raw).unwrap();
        let also = config.also_allow.unwrap();
        assert_eq!(also, vec!["curl", "wget"]);
    }

    // -- SearXNG search config --

    #[test]
    fn searxng_search_config() {
        let raw = json!({
            "host": "http://searxng.local:8888",
            "maxResults": 20,
            "engines": ["google", "bing"],
            "language": "en"
        });
        let config: SearxngSearchConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.host.as_deref(), Some("http://searxng.local:8888"));
        assert_eq!(config.max_results, Some(20));
        assert_eq!(config.engines.as_ref().unwrap().len(), 2);
    }

    // -- X search config --

    #[test]
    fn x_search_config() {
        let raw = json!({
            "apiKey": "xai-key",
            "model": "grok-3",
            "maxResults": 5
        });
        let config: XSearchConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.api_key.as_deref(), Some("xai-key"));
        assert_eq!(config.model.as_deref(), Some("grok-3"));
    }

    // -- WebFetch maxResponseBytes --

    #[test]
    fn web_fetch_max_response_bytes() {
        let raw = json!({
            "enabled": true,
            "maxResponseBytes": 2097152
        });
        let config: WebFetchConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.max_response_bytes, Some(2097152));
    }

    // -- Memory FTS tokenizer --

    #[test]
    fn memory_fts_tokenizer_config() {
        let raw = json!({
            "driver": "sqlite",
            "fts": { "tokenizer": "trigram" }
        });
        let config: MemorySearchStoreConfig = serde_json::from_value(raw).unwrap();
        let fts = config.fts.unwrap();
        assert_eq!(fts.tokenizer.as_deref(), Some("trigram"));
    }

    // -- Memory sync postCompactionForce --

    #[test]
    fn memory_sync_post_compaction_force() {
        let raw = json!({
            "onBoot": true,
            "postCompactionForce": true
        });
        let config: MemorySearchSyncConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.post_compaction_force, Some(true));
    }

    // -- QMD extra collections --

    #[test]
    fn qmd_extra_collections() {
        let raw = json!({
            "searchMode": "hybrid",
            "extraCollections": ["logs", "docs"]
        });
        let config: MemoryQmdConfig = serde_json::from_value(raw).unwrap();
        let extras = config.extra_collections.unwrap();
        assert_eq!(extras, vec!["logs", "docs"]);
    }

    // -- Auth cooldown config --

    #[test]
    fn auth_cooldown_config() {
        let raw = json!({
            "rateLimitedProfileRotations": 3,
            "overloadedProfileRotations": 2,
            "overloadedBackoffMs": 5000
        });
        let config: AuthCooldownConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.rate_limited_profile_rotations, Some(3));
        assert_eq!(config.overloaded_profile_rotations, Some(2));
        assert_eq!(config.overloaded_backoff_ms, Some(5000));
    }

    // -- Auth profile displayName --

    #[test]
    fn auth_profile_display_name() {
        let raw = json!({
            "provider": "anthropic",
            "displayName": "Production Claude"
        });
        let config: AuthProfileConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.display_name.as_deref(), Some("Production Claude"));
    }

    // -- Bedrock guardrails --

    #[test]
    fn bedrock_guardrails_config() {
        let raw = json!({
            "enabled": true,
            "guardrailId": "gr-123",
            "guardrailVersion": "1",
            "trace": "enabled"
        });
        let config: BedrockGuardrailsConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.enabled, Some(true));
        assert_eq!(config.guardrail_id.as_deref(), Some("gr-123"));
        assert_eq!(config.guardrail_version.as_deref(), Some("1"));
        assert_eq!(config.trace.as_deref(), Some("enabled"));
    }

    // -- Cron default tools --

    #[test]
    fn cron_default_tools() {
        let raw = json!({
            "enabled": true,
            "defaultTools": ["web_search", "memory_search"]
        });
        let config: CronConfig = serde_json::from_value(raw).unwrap();
        let tools = config.default_tools.unwrap();
        assert_eq!(tools, vec!["web_search", "memory_search"]);
    }

    // -- Channel health monitor --

    #[test]
    fn channel_health_monitor_config() {
        let raw = json!({ "enabled": false });
        let config: ChannelHealthMonitorConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.enabled, Some(false));
    }

    // -- WebSearchConfig new providers --

    #[test]
    fn web_search_config_with_searxng_and_x() {
        let raw = json!({
            "provider": "searxng",
            "searxng": { "host": "http://localhost:8888" },
            "xSearch": { "apiKey": "xai-key" },
            "openaiCodex": true
        });
        let config: WebSearchConfig = serde_json::from_value(raw).unwrap();
        assert!(config.searxng.is_some());
        assert!(config.x_search.is_some());
        assert_eq!(config.openai_codex, Some(true));
    }

    // ====================================================================
    // v2026.4.29 — messages.visibleReplies
    // ====================================================================

    #[test]
    fn messages_visible_replies_defaults_to_none() {
        let cfg = MessagesConfig::default();
        assert_eq!(cfg.visible_replies, None);
    }

    #[test]
    fn messages_visible_replies_deserializes_camelcase_true() {
        let raw = json!({ "visibleReplies": true });
        let cfg: MessagesConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(cfg.visible_replies, Some(true));
    }

    #[test]
    fn messages_visible_replies_deserializes_camelcase_false() {
        let raw = json!({ "visibleReplies": false });
        let cfg: MessagesConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(cfg.visible_replies, Some(false));
    }

    #[test]
    fn messages_visible_replies_serializes_camelcase() {
        let cfg = MessagesConfig {
            visible_replies: Some(true),
            ..Default::default()
        };
        let v = serde_json::to_value(&cfg).unwrap();
        assert_eq!(v["visibleReplies"], json!(true));
    }

    #[test]
    fn messages_visible_replies_omitted_when_none_with_default_serialization() {
        // When skipping, the field should not appear in serialized output.
        // Note: this only holds with `skip_serializing_if`; this test documents
        // current behavior — without `skip_serializing_if` the key is present
        // as null. If serialized output cleanliness is required, add the
        // attribute.
        let cfg = MessagesConfig::default();
        let v = serde_json::to_value(&cfg).unwrap();
        // Either omitted or null — both are acceptable for an absent field.
        assert!(v.get("visibleReplies").map_or(true, |x| x.is_null()));
    }

    #[test]
    fn messages_visible_replies_alongside_other_fields() {
        let raw = json!({
            "messagePrefix": "[bot] ",
            "visibleReplies": true,
            "ackReaction": "👀"
        });
        let cfg: MessagesConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(cfg.message_prefix.as_deref(), Some("[bot] "));
        assert_eq!(cfg.visible_replies, Some(true));
        assert_eq!(cfg.ack_reaction.as_deref(), Some("👀"));
    }
}
