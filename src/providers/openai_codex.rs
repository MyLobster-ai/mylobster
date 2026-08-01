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

// ============================================================================
// Native Codex Responses backend detection + payload sanitization
// (v2026.5.2 #75111: strip native-Codex-only unsupported payload fields
// without touching custom compatible endpoints; preserve existing wrapped
// Codex streams during OpenAI attribution)
// ============================================================================

/// Params rejected by the native ChatGPT/Codex Responses backend.
pub const OPENAI_CODEX_RESPONSES_UNSUPPORTED_PARAMS: &[&str] = &[
    "max_output_tokens",
    "metadata",
    "prompt_cache_retention",
    "service_tier",
    "temperature",
    "top_p",
];

/// Codex-routable OpenAI platform model ids (v2026.7.1 state —
/// `gpt-5.4-mini` restored to the routable set).
pub const OPENAI_CODEX_ROUTABLE_MODEL_IDS: &[&str] = &[
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.5-pro",
    "gpt-5.4",
    "gpt-5.4-codex",
    "gpt-5.4-pro",
    "gpt-5.4-mini",
];

// ============================================================================
// `openai-codex` folded into `openai` (v2026.5.x–6.x)
// ============================================================================

/// Doctor repair: migrate a legacy `openai-codex/<model>` ref onto the
/// canonical `openai/<model>` provider, upgrading retired model ids through
/// the codex-aware retirement map (`gpt-5.2` → `gpt-5.5`, etc.). Returns
/// `None` when the ref is already canonical.
pub fn migrate_codex_provider_ref(model_ref: &str) -> Option<String> {
    let trimmed = model_ref.trim();
    let rest = trimmed.strip_prefix("openai-codex/")?;
    let upgraded = super::catalog::upgrade_retired_model_ref("openai-codex", rest)
        .map(|m| m.to_string())
        .unwrap_or_else(|| rest.to_string());
    Some(format!("openai/{}", upgraded))
}

/// `openai/chat-latest` override (v2026.6.x): the floating chat alias
/// resolves onto the current chat-latest model.
pub const OPENAI_CHAT_LATEST_MODEL: &str = "gpt-5.3-chat-latest";

/// Resolve the `chat-latest` alias; other ids pass through unchanged.
pub fn resolve_chat_latest_alias(model_id: &str) -> &str {
    let normalized = model_id.trim();
    if normalized.eq_ignore_ascii_case("chat-latest")
        || normalized.eq_ignore_ascii_case("openai/chat-latest")
    {
        OPENAI_CHAT_LATEST_MODEL
    } else {
        normalized
    }
}

// ============================================================================
// Codex app-server harness — provider-side pieces (v2026.5.x–7.1).
// The app-server runtime itself (native threads, watchdogs, SQLite thread
// bindings) is the agents cluster's half; these are the provider-owned
// contracts it consumes.
// ============================================================================

/// Minimum managed `@openai/codex` app-server wire version.
pub const CODEX_MIN_WIRE_VERSION: &str = "0.143.0";

/// Managed Codex binary version pinned at v2026.7.1.
pub const CODEX_MANAGED_VERSION: &str = "0.144.3";

/// Migrate legacy Codex approval modes (v2026.6.x: `on-failure` retired in
/// favor of `on-request`; unknown modes fall back to `ask`).
pub fn migrate_codex_approval_mode(mode: &str) -> &'static str {
    match mode.trim().to_ascii_lowercase().as_str() {
        "on-failure" | "onfailure" => "on-request",
        "on-request" => "on-request",
        "never" => "never",
        "untrusted" => "untrusted",
        "ask" => "ask",
        _ => "ask",
    }
}

/// Per-agent `CODEX_HOME` isolation: each agent gets its own Codex state dir
/// without rewriting `HOME` (v2026.6.x).
pub fn codex_home_for_agent(state_dir: &std::path::Path, agent_id: &str) -> std::path::PathBuf {
    let sanitized: String = agent_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    state_dir.join("codex").join(sanitized)
}

/// Whether a model id can route through the ChatGPT/Codex Responses auth
/// path. Accepts optional `openai/` or `openai-codex/` prefixes.
pub fn is_codex_routable_model(model_id: &str) -> bool {
    let normalized = model_id.trim().to_ascii_lowercase();
    let normalized = normalized
        .strip_prefix("openai-codex/")
        .or_else(|| normalized.strip_prefix("openai/"))
        .unwrap_or(&normalized);
    OPENAI_CODEX_ROUTABLE_MODEL_IDS.contains(&normalized)
}

/// Whether a base URL points at the native ChatGPT/Codex Responses backend.
/// Only `chatgpt.com` `/backend-api[/codex][/v1]` paths qualify — custom
/// compatible endpoints must NOT have Codex-only field stripping applied.
pub fn is_native_codex_responses_base_url(base_url: &str) -> bool {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return false;
    }
    let Ok(parsed) = url::Url::parse(trimmed) else {
        return false;
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return false;
    }
    if parsed
        .host_str()
        .map(|h| h.to_ascii_lowercase())
        .as_deref()
        != Some("chatgpt.com")
    {
        return false;
    }
    let pathname = parsed.path().trim_end_matches('/').to_ascii_lowercase();
    matches!(
        pathname.as_str(),
        "/backend-api" | "/backend-api/v1" | "/backend-api/codex" | "/backend-api/codex/v1"
    )
}

fn strip_codex_responses_unsupported_text_fields(params: &mut serde_json::Map<String, serde_json::Value>) {
    let Some(text) = params.get("text") else {
        return;
    };
    let Some(text_obj) = text.as_object() else {
        return;
    };
    let mut sanitized = text_obj.clone();
    sanitized.remove("format");
    if sanitized.is_empty() {
        params.remove("text");
    } else {
        params.insert("text".to_string(), serde_json::Value::Object(sanitized));
    }
}

/// Strip native-Codex-only unsupported payload fields when (and only when)
/// the request targets the native ChatGPT/Codex Responses backend. Custom
/// compatible endpoints pass through untouched.
pub fn sanitize_codex_responses_params(params: &mut serde_json::Value, base_url: &str) {
    if !is_native_codex_responses_base_url(base_url) {
        return;
    }
    let Some(map) = params.as_object_mut() else {
        return;
    };
    for key in OPENAI_CODEX_RESPONSES_UNSUPPORTED_PARAMS {
        map.remove(*key);
    }
    strip_codex_responses_unsupported_text_fields(map);
}

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

        // Send the chat completion request as JSON. The payload is patched in
        // place (v2026.5.2 #75111): the existing wrapped Codex stream is
        // preserved — attribution-time sanitization mutates the outbound body
        // instead of rebuilding the transport — and native-Codex-only
        // unsupported fields are stripped only for the native backend.
        let mut body =
            serde_json::to_value(super::openai_compat::build_request(request, true))?;
        sanitize_codex_responses_params(&mut body, &self.base_url);
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

impl OpenAiCodexProvider {
    /// Clear request fields the native Codex backend rejects before the
    /// shared OpenAI-compat path serializes them. Custom endpoints keep the
    /// caller's fields.
    fn strip_unsupported_request_fields(&self, request: &mut ProviderRequest) {
        if is_native_codex_responses_base_url(&self.base_url) {
            request.temperature = None;
        }
    }
}

#[async_trait]
impl ModelProvider for OpenAiCodexProvider {
    async fn chat(&self, mut request: ProviderRequest) -> Result<ProviderResponse> {
        self.strip_unsupported_request_fields(&mut request);
        super::openai_compat::openai_compat_chat(
            &self.client,
            &self.base_url,
            &self.api_key,
            request,
            "openai-codex",
        )
        .await
    }

    async fn stream_chat(&self, mut request: ProviderRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        self.strip_unsupported_request_fields(&mut request);
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

    // ------------------------------------------------------------------
    // Native backend detection (v2026.5.2 #75111)
    // ------------------------------------------------------------------

    #[test]
    fn native_codex_base_url_detection() {
        assert!(is_native_codex_responses_base_url("https://chatgpt.com/backend-api"));
        assert!(is_native_codex_responses_base_url("https://chatgpt.com/backend-api/v1"));
        assert!(is_native_codex_responses_base_url("https://chatgpt.com/backend-api/codex"));
        assert!(is_native_codex_responses_base_url(
            "https://chatgpt.com/backend-api/codex/v1/"
        ));
    }

    #[test]
    fn custom_endpoints_are_not_native() {
        assert!(!is_native_codex_responses_base_url("https://api.openai.com/v1"));
        assert!(!is_native_codex_responses_base_url("https://chatgpt.com/other"));
        assert!(!is_native_codex_responses_base_url("https://evil.com/backend-api"));
        assert!(!is_native_codex_responses_base_url("ftp://chatgpt.com/backend-api"));
        assert!(!is_native_codex_responses_base_url(""));
        assert!(!is_native_codex_responses_base_url("not a url"));
    }

    // ------------------------------------------------------------------
    // Unsupported payload field stripping
    // ------------------------------------------------------------------

    #[test]
    fn sanitize_strips_unsupported_params_on_native_backend() {
        let mut params = serde_json::json!({
            "model": "gpt-5.5",
            "temperature": 0.4,
            "top_p": 0.9,
            "max_output_tokens": 100,
            "metadata": {"a": 1},
            "prompt_cache_retention": "24h",
            "service_tier": "auto",
            "messages": []
        });
        sanitize_codex_responses_params(&mut params, "https://chatgpt.com/backend-api/codex");
        let obj = params.as_object().unwrap();
        for key in OPENAI_CODEX_RESPONSES_UNSUPPORTED_PARAMS {
            assert!(!obj.contains_key(*key), "{} should be stripped", key);
        }
        assert_eq!(obj["model"], "gpt-5.5");
    }

    #[test]
    fn sanitize_leaves_custom_compat_endpoints_untouched() {
        let mut params = serde_json::json!({"model": "gpt-5.5", "temperature": 0.4});
        sanitize_codex_responses_params(&mut params, "https://my-proxy.example.com/v1");
        assert_eq!(params["temperature"], 0.4);
    }

    #[test]
    fn sanitize_strips_text_format_but_keeps_other_text_fields() {
        let mut params = serde_json::json!({
            "text": {"format": {"type": "json_object"}, "verbosity": "low"}
        });
        sanitize_codex_responses_params(&mut params, "https://chatgpt.com/backend-api");
        assert_eq!(params["text"], serde_json::json!({"verbosity": "low"}));
    }

    #[test]
    fn sanitize_removes_text_when_only_format_present() {
        let mut params = serde_json::json!({"text": {"format": {"type": "json_object"}}});
        sanitize_codex_responses_params(&mut params, "https://chatgpt.com/backend-api");
        assert!(params.as_object().unwrap().get("text").is_none());
    }

    // ------------------------------------------------------------------
    // Codex-routable model set (gpt-5.4-mini restored)
    // ------------------------------------------------------------------

    #[test]
    fn gpt_5_4_mini_is_codex_routable() {
        assert!(is_codex_routable_model("gpt-5.4-mini"));
        assert!(is_codex_routable_model("openai-codex/gpt-5.4-mini"));
        assert!(is_codex_routable_model("openai/gpt-5.4-mini"));
    }

    #[test]
    fn codex_routable_set_membership() {
        assert!(is_codex_routable_model("gpt-5.5"));
        assert!(is_codex_routable_model("GPT-5.6-Sol"));
        assert!(!is_codex_routable_model("gpt-4o"));
        assert!(!is_codex_routable_model("gpt-5.4-nano"));
    }

    // ------------------------------------------------------------------
    // openai-codex → openai fold (v2026.5.x–6.x)
    // ------------------------------------------------------------------

    #[test]
    fn migrates_codex_refs_onto_openai() {
        assert_eq!(
            migrate_codex_provider_ref("openai-codex/gpt-5.4-mini").as_deref(),
            Some("openai/gpt-5.4-mini")
        );
        // Retired codex models upgrade through the codex-aware map.
        assert_eq!(
            migrate_codex_provider_ref("openai-codex/gpt-5.2").as_deref(),
            Some("openai/gpt-5.5")
        );
        assert_eq!(
            migrate_codex_provider_ref("openai-codex/gpt-4.1-nano").as_deref(),
            Some("openai/gpt-5.4-mini")
        );
    }

    #[test]
    fn canonical_refs_do_not_migrate() {
        assert!(migrate_codex_provider_ref("openai/gpt-5.5").is_none());
        assert!(migrate_codex_provider_ref("anthropic/claude-sonnet-5").is_none());
    }

    #[test]
    fn chat_latest_alias_resolves() {
        assert_eq!(resolve_chat_latest_alias("chat-latest"), OPENAI_CHAT_LATEST_MODEL);
        assert_eq!(
            resolve_chat_latest_alias("openai/chat-latest"),
            OPENAI_CHAT_LATEST_MODEL
        );
        assert_eq!(resolve_chat_latest_alias("gpt-5.6"), "gpt-5.6");
    }

    // ------------------------------------------------------------------
    // Codex harness provider-side pieces (v2026.5.x–7.1)
    // ------------------------------------------------------------------

    #[test]
    fn approval_mode_migration() {
        assert_eq!(migrate_codex_approval_mode("on-failure"), "on-request");
        assert_eq!(migrate_codex_approval_mode("ON-FAILURE"), "on-request");
        assert_eq!(migrate_codex_approval_mode("on-request"), "on-request");
        assert_eq!(migrate_codex_approval_mode("never"), "never");
        assert_eq!(migrate_codex_approval_mode("bogus"), "ask");
    }

    #[test]
    fn codex_home_isolated_per_agent() {
        let home = codex_home_for_agent(std::path::Path::new("/state"), "agent one!");
        assert_eq!(home, std::path::PathBuf::from("/state/codex/agent-one-"));
        let other = codex_home_for_agent(std::path::Path::new("/state"), "beta_2");
        assert_ne!(home, other);
    }

    #[test]
    fn wire_version_floor_pinned() {
        assert_eq!(CODEX_MIN_WIRE_VERSION, "0.143.0");
        assert!(CODEX_MANAGED_VERSION.starts_with("0.144"));
    }
}
