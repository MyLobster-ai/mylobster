use super::*;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::warn;

/// v2026.2.26: Risk warning for Gemini CLI OAuth.
///
/// Gemini CLI OAuth grants broad Google account access. This constant
/// provides the warning text that should be shown to users before
/// initiating OAuth flows.
pub const GEMINI_OAUTH_RISK_WARNING: &str =
    "WARNING: Gemini CLI OAuth grants access to your Google account. \
     Only proceed if you trust this application and understand the \
     permissions being requested. This is NOT recommended for shared \
     or untrusted environments.";

/// Check if Gemini OAuth should require confirmation.
///
/// Returns `true` if the environment suggests this is a CLI or
/// unattended context where OAuth risks should be highlighted.
pub fn should_warn_oauth() -> bool {
    // Warn in non-interactive or shared environments
    std::env::var("GEMINI_SKIP_OAUTH_WARNING").is_err()
}

/// Default Generative Language API base URL (no trailing slash).
pub const DEFAULT_GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

const GOOGLE_GENERATIVE_LANGUAGE_HOST: &str = "generativelanguage.googleapis.com";

/// Normalize a Google Generative Language API base URL (v2026.7.1 parity
/// with upstream `normalizeGoogleApiBaseUrl`): trims trailing slashes,
/// strips query/hash, and gives a bare `generativelanguage.googleapis.com`
/// origin its `/v1beta` path.
pub fn normalize_google_api_base_url(base_url: Option<&str>) -> String {
    let raw = base_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_GEMINI_BASE_URL)
        .trim_end_matches('/')
        .to_string();
    match url::Url::parse(&raw) {
        Ok(mut url) => {
            url.set_fragment(None);
            url.set_query(None);
            let is_google_host = url
                .host_str()
                .map(|h| h.eq_ignore_ascii_case(GOOGLE_GENERATIVE_LANGUAGE_HOST))
                .unwrap_or(false);
            if is_google_host && url.path().trim_end_matches('/').is_empty() {
                url.set_path("/v1beta");
            }
            url.to_string().trim_end_matches('/').to_string()
        }
        Err(_) => raw,
    }
}

// ============================================================================
// Model-id migration (v2026.7.1)
// ============================================================================

const GOOGLE_PROVIDER_PREFIX: &str = "google/";
/// Provider ids whose model suffixes take Google model-id normalization.
const GOOGLE_PROVIDER_IDS: [&str; 3] = ["google", "google-gemini-cli", "google-vertex"];

/// Strip a leading `google/` prefix from a model id.
pub fn strip_google_provider_prefix(id: &str) -> &str {
    id.strip_prefix(GOOGLE_PROVIDER_PREFIX).unwrap_or(id)
}

/// Canonicalize retired Google model ids (v2026.7.1 parity with upstream
/// `normalizeGoogleModelId`): the retired `gemini-3-pro-preview` (and the
/// bare `gemini-3-pro` / `gemini-3.1-pro` aliases) map to
/// `gemini-3.1-pro-preview`; retired Flash/Flash-Lite preview ids map to
/// their current API ids; `gemma-4-26b` maps to its full instruction id.
pub fn normalize_google_model_id(id: &str) -> String {
    if let Some(model_id) = id.strip_prefix(GOOGLE_PROVIDER_PREFIX) {
        let normalized = normalize_google_model_id(model_id);
        return if normalized == model_id {
            id.to_string()
        } else {
            format!("{GOOGLE_PROVIDER_PREFIX}{normalized}")
        };
    }
    match id {
        "gemini-3-pro" | "gemini-3-pro-preview" | "gemini-3.1-pro" => {
            "gemini-3.1-pro-preview".to_string()
        }
        "gemini-3-flash" => "gemini-3-flash-preview".to_string(),
        // Gemini 3.1 Flash Lite graduated to GA; the -preview endpoint is
        // deprecated. Map the old preview name to the stable GA id.
        "gemini-3.1-flash-lite-preview" => "gemini-3.1-flash-lite".to_string(),
        "gemini-3.1-flash" | "gemini-3.1-flash-preview" => "gemini-3-flash-preview".to_string(),
        "gemma-4-26b" => "gemma-4-26b-a4b-it".to_string(),
        other => other.to_string(),
    }
}

/// Canonicalize a full `provider/model` ref, including nested proxy forms
/// like `kilocode/google/gemini-3-pro-preview` and
/// `openrouter/google/gemini-3-pro-preview`: whenever the model suffix is
/// itself a `google/...` ref, or the direct provider is a Google provider,
/// the Google model id migrates.
pub fn normalize_google_model_ref(model_ref: &str) -> String {
    let trimmed = model_ref.trim();
    let Some((provider, suffix)) = trimmed.split_once('/') else {
        return trimmed.to_string();
    };
    if GOOGLE_PROVIDER_IDS.contains(&provider) || suffix.starts_with(GOOGLE_PROVIDER_PREFIX) {
        return format!("{provider}/{}", normalize_google_model_id(suffix));
    }
    // Nested proxy forms: peel one provider layer and retry (e.g.
    // `kilocode/openrouter/google/gemini-3-pro-preview`).
    if suffix.contains('/') {
        let normalized_suffix = normalize_google_model_ref(suffix);
        if normalized_suffix != suffix {
            return format!("{provider}/{normalized_suffix}");
        }
    }
    trimmed.to_string()
}

// ============================================================================
// Vertex multi-region hosts (v2026.7.1)
// ============================================================================

/// Resolve the Vertex AI base host for a location (v2026.7.1 parity with
/// upstream Google transport): `global` uses the plain host; `eu`/`us`
/// multi-region locations use the dedicated `.rep.googleapis.com` host with
/// the location embedded (a regional prefix like
/// `eu-aiplatform.googleapis.com` returns an HTML 404); other locations use
/// the standard `{location}-aiplatform` prefix.
pub fn resolve_vertex_base_host(location: &str) -> String {
    match location {
        "global" => "https://aiplatform.googleapis.com".to_string(),
        "eu" | "us" => format!("https://aiplatform.{location}.rep.googleapis.com"),
        other => format!("https://{other}-aiplatform.googleapis.com"),
    }
}

// ============================================================================
// Thought signatures (v2026.7.1)
// ============================================================================

/// Thought signatures must be base64 for Google APIs (TYPE_BYTES).
/// Compaction-truncated signatures fail this check and are dropped before
/// replay instead of aborting the next assistant turn with a malformed
/// Base64 400.
pub fn is_valid_thought_signature(signature: &str) -> bool {
    if signature.is_empty() {
        return false;
    }
    let bytes = signature.as_bytes();
    let padding = bytes.iter().rev().take_while(|b| **b == b'=').count();
    if padding > 2 {
        return false;
    }
    // `=` padding is only meaningful when it completes a 4-character group.
    if padding > 0 && bytes.len() % 4 != 0 {
        return false;
    }
    let body = &bytes[..bytes.len() - padding];
    // Unpadded base64 is legal and common here, so length need not be a
    // multiple of 4 — but a body of length ≡ 1 (mod 4) encodes no whole byte
    // and cannot decode. That residue is exactly the compaction-truncated
    // signature we must drop.
    if body.is_empty() || body.len() % 4 == 1 {
        return false;
    }
    body.iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/')
}

/// Preserve the last non-empty signature for the current streamed block:
/// some backends only send `thoughtSignature` on the first delta and omit it
/// later — never overwrite a captured signature with nothing.
pub fn retain_thought_signature(existing: Option<String>, incoming: Option<&str>) -> Option<String> {
    match incoming {
        Some(sig) if !sig.is_empty() => Some(sig.to_string()),
        _ => existing,
    }
}

/// Only replay signatures from the same provider/model and with valid base64.
pub fn resolve_thought_signature(
    is_same_provider_and_model: bool,
    signature: Option<&str>,
) -> Option<String> {
    match signature {
        Some(sig) if is_same_provider_and_model && is_valid_thought_signature(sig) => {
            Some(sig.to_string())
        }
        _ => None,
    }
}

// ============================================================================
// Function-declaration normalization (v2026.7.1)
// ============================================================================

/// JSON-Schema meta declarations Gemini's function-declaration schema
/// rejects; stripped recursively (upstream `sanitizeForOpenApi`).
const JSON_SCHEMA_META_DECLARATIONS: [&str; 8] = [
    "$schema",
    "$id",
    "$anchor",
    "$dynamicAnchor",
    "$vocabulary",
    "$comment",
    "$defs",
    "definitions",
];

/// Recursively strip JSON-Schema meta declarations from a tool parameter
/// schema so Gemini (incl. the Live API) accepts the function declaration.
pub fn sanitize_function_declaration_schema(schema: &serde_json::Value) -> serde_json::Value {
    match schema {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, value) in map {
                if JSON_SCHEMA_META_DECLARATIONS.contains(&key.as_str()) {
                    continue;
                }
                out.insert(key.clone(), sanitize_function_declaration_schema(value));
            }
            serde_json::Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Convert provider-neutral tool definitions into a Gemini `tools` payload
/// (`[{functionDeclarations: [...]}]`) with normalized parameter schemas.
pub fn convert_tools_to_gemini(tools: &[serde_json::Value]) -> Option<serde_json::Value> {
    if tools.is_empty() {
        return None;
    }
    let declarations: Vec<serde_json::Value> = tools
        .iter()
        .filter_map(|tool| {
            let name = tool
                .get("name")
                .and_then(|n| n.as_str())
                .or_else(|| tool.pointer("/function/name").and_then(|n| n.as_str()))?;
            let description = tool
                .get("description")
                .and_then(|d| d.as_str())
                .or_else(|| tool.pointer("/function/description").and_then(|d| d.as_str()))
                .unwrap_or("");
            let parameters = tool
                .get("input_schema")
                .or_else(|| tool.get("parameters"))
                .or_else(|| tool.pointer("/function/parameters"));
            let mut decl = serde_json::json!({
                "name": name,
                "description": description,
            });
            if let Some(parameters) = parameters {
                decl["parameters"] = sanitize_function_declaration_schema(parameters);
            }
            Some(decl)
        })
        .collect();
    if declarations.is_empty() {
        return None;
    }
    Some(serde_json::json!([{ "functionDeclarations": declarations }]))
}

/// Map a provider-neutral tool choice to Gemini function-calling mode.
pub fn map_tool_choice_to_gemini(choice: &serde_json::Value) -> &'static str {
    let raw = choice
        .as_str()
        .or_else(|| choice.get("type").and_then(|t| t.as_str()))
        .unwrap_or("auto");
    match raw {
        "none" => "NONE",
        "any" | "required" | "tool" => "ANY",
        _ => "AUTO",
    }
}

// ============================================================================
// Thinking config resolution (v2026.7.1)
// ============================================================================

fn is_gemini3_pro_model(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.contains("gemini-3-pro") || lower.contains("gemini-3.1-pro")
}

fn is_gemini3_flash_model(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.contains("gemini-3-flash") || lower.contains("gemini-3.1-flash")
}

fn is_gemma4_model(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.contains("gemma-4") || lower.contains("gemma4")
}

fn model_supports_reasoning(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    is_gemini3_pro_model(model)
        || is_gemini3_flash_model(model)
        || is_gemma4_model(model)
        || lower.contains("gemini-2.5")
}

/// Resolve the `generationConfig.thinkingConfig` payload (v2026.7.1).
///
/// - Thinking requested → `{includeThoughts: true, thinkingBudget}` with the
///   per-model budget floor applied.
/// - Thinking explicitly off on a reasoning model → the model-appropriate
///   disabled config: Gemini 3.x Pro cannot disable thinking (lowest level
///   `LOW`), Gemini 3 Flash / Gemma 4 use `MINIMAL`, Gemini 2.x disables via
///   `thinkingBudget: 0`. `includeThoughts` stays unset so hidden thinking
///   remains invisible (`reasoning: false` honored).
/// - Non-reasoning models → no thinkingConfig at all.
pub fn resolve_gemini_thinking_config(
    model: &str,
    thinking: Option<&crate::providers::ThinkingConfig>,
) -> Option<serde_json::Value> {
    match thinking {
        Some(config) => Some(serde_json::json!({
            "includeThoughts": true,
            "thinkingBudget": effective_thinking_budget(model, config.budget_tokens as u32),
        })),
        None => {
            if !model_supports_reasoning(model) {
                return None;
            }
            if is_gemini3_pro_model(model) {
                Some(serde_json::json!({"thinkingLevel": "LOW"}))
            } else if is_gemini3_flash_model(model) || is_gemma4_model(model) {
                Some(serde_json::json!({"thinkingLevel": "MINIMAL"}))
            } else {
                Some(serde_json::json!({"thinkingBudget": 0}))
            }
        }
    }
}

// ============================================================================
// API key rotation (v2026.7.1)
// ============================================================================

/// Rate-limit detector used by key rotation: HTTP 429 / quota errors move on
/// to the next configured key instead of failing the request.
pub fn is_api_key_rate_limit_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("429")
        || lower.contains("rate limit")
        || lower.contains("rate-limit")
        || lower.contains("resource_exhausted")
        || lower.contains("resource exhausted")
        || lower.contains("quota")
}

/// De-duplicated key list in stable order: primary first, extras after.
pub fn collect_gemini_api_keys(primary: &str, extra: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut keys = Vec::new();
    for key in std::iter::once(primary).chain(extra.iter().map(String::as_str)) {
        let trimmed = key.trim();
        if trimmed.is_empty() || !seen.insert(trimmed.to_string()) {
            continue;
        }
        keys.push(trimmed.to_string());
    }
    keys
}

pub struct GeminiProvider {
    api_key: String,
    model: String,
    base_url: String,
    client: Client,
    /// Optional cooperative cancellation handle (v2026.7.1). When set, both
    /// non-streaming and streaming fetches race against the abort signal.
    abort: Option<crate::infra::abort_signal::AbortHandle>,
    /// Additional API keys for per-request rotation on rate limits
    /// (v2026.7.1). The primary key is always tried first.
    extra_api_keys: Vec<String>,
}

impl GeminiProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            api_key,
            // v2026.7.1: retired Gemini model ids migrate at construction so
            // stale config keeps working against current API endpoints.
            model: normalize_google_model_id(&model),
            base_url: DEFAULT_GEMINI_BASE_URL.to_string(),
            client: Client::new(),
            abort: None,
            extra_api_keys: Vec::new(),
        }
    }

    /// Add fallback API keys tried in order when a request rate-limits
    /// (v2026.7.1 per-LLM-request key rotation).
    pub fn with_extra_api_keys(mut self, keys: Vec<String>) -> Self {
        self.extra_api_keys = keys;
        self
    }

    /// Override the API base URL (e.g. for Vertex/regional endpoints or test mocks).
    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    /// Route an abort handle into provider fetches (v2026.7.1). Aborting the
    /// handle cancels in-flight requests and terminates streams.
    pub fn with_abort_handle(mut self, abort: crate::infra::abort_signal::AbortHandle) -> Self {
        self.abort = Some(abort);
        self
    }
}

// ============================================================================
// Gemini API Types
// ============================================================================

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
    /// `[{functionDeclarations: [...]}]` (v2026.7.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<serde_json::Value>,
    /// `{functionCallingConfig: {mode}}` (v2026.7.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_config: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    /// Model-emitted function call (v2026.7.1). Parsed in part order so
    /// parallel tool calls replay in the order the model produced them.
    #[serde(skip_serializing_if = "Option::is_none")]
    function_call: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    /// Native reasoning mode (v2026.7.1): `{includeThoughts, thinkingBudget}`
    /// or a model-appropriate disabled config when reasoning is off.
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_config: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiResponse {
    candidates: Option<Vec<GeminiCandidate>>,
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiCandidate {
    content: Option<GeminiContent>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiUsageMetadata {
    prompt_token_count: Option<u64>,
    candidates_token_count: Option<u64>,
}

// ============================================================================
// Helper: Convert ProviderMessages to Gemini format
// ============================================================================

fn convert_messages(messages: Vec<ProviderMessage>) -> Vec<GeminiContent> {
    messages
        .into_iter()
        .map(|m| {
            let role = match m.role.as_str() {
                "assistant" => "model".to_string(),
                other => other.to_string(),
            };

            let text = if let Some(s) = m.content.as_str() {
                s.to_string()
            } else {
                m.content.to_string()
            };

            GeminiContent {
                role,
                parts: vec![GeminiPart { text: Some(text), function_call: None }],
            }
        })
        .collect()
}

// ============================================================================
// Thinking-budget policy (v2026.5.2 parity)
// ============================================================================

/// Lowest thinking budget (in output tokens) accepted by Gemini 2.5 Flash-Lite.
///
/// v2026.5.2 raised the floor for `gemini-2.5-flash-lite` because requests with
/// `reasoning: "minimal"` (which had been mapped to budget=128 or similar) were
/// being rejected by the Google API. Pro / Flash variants still accept lower
/// minimal presets, so the floor is model-specific.
pub const GEMINI_2_5_FLASH_LITE_MIN_BUDGET: u32 = 512;

/// Apply v2026.5.2 thinking-budget policy: clamp the requested budget up to
/// the per-model minimum where the upstream API rejects values below it.
///
/// Returns `requested` unchanged for models without a known floor. Pro/Flash
/// "minimal" presets are intentionally untouched — only Flash-Lite raises the
/// floor.
pub fn effective_thinking_budget(model: &str, requested: u32) -> u32 {
    let lower = model.to_ascii_lowercase();
    if lower.contains("gemini-2.5-flash-lite") || lower.contains("gemini-2.5-flashlite") {
        requested.max(GEMINI_2_5_FLASH_LITE_MIN_BUDGET)
    } else {
        requested
    }
}

// ============================================================================
// ModelProvider Implementation
// ============================================================================

impl GeminiProvider {
    /// Build the request body shared by chat and stream: contents,
    /// generation config with native thinking (v2026.7.1), normalized tool
    /// declarations, and function-calling mode.
    fn build_request_body(&self, request: &ProviderRequest) -> GeminiRequest {
        let contents = convert_messages(request.messages.clone());
        let thinking_config =
            resolve_gemini_thinking_config(&self.model, request.thinking.as_ref());
        let tools = request
            .tools
            .as_deref()
            .and_then(convert_tools_to_gemini);
        let tool_config = if tools.is_some() {
            request.tool_choice.as_ref().map(|choice| {
                serde_json::json!({
                    "functionCallingConfig": {"mode": map_tool_choice_to_gemini(choice)}
                })
            })
        } else {
            None
        };
        GeminiRequest {
            contents,
            generation_config: Some(GeminiGenerationConfig {
                max_output_tokens: request.max_tokens,
                temperature: request.temperature,
                thinking_config,
            }),
            tools,
            tool_config,
        }
    }

    /// Send a request racing the abort handle (v2026.7.1 abort routing).
    async fn send_with_abort(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let send = builder.send();
        match &self.abort {
            Some(abort) => {
                match crate::infra::abort_signal::monitor_with_abort_lifecycle(send, abort).await {
                    Ok(result) => Ok(result?),
                    Err(_) => anyhow::bail!("Gemini request aborted"),
                }
            }
            None => Ok(send.await?),
        }
    }
}

#[async_trait]
impl ModelProvider for GeminiProvider {
    async fn chat(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        let body = self.build_request_body(&request);

        // v2026.7.1: the API key travels in the `x-goog-api-key` header, never
        // in the URL — keeps keys out of logs/proxies/error messages.
        let url = format!("{}/models/{}:generateContent", self.base_url, self.model);

        // v2026.7.1: per-LLM-request API key rotation — a rate-limited key
        // moves on to the next configured key instead of failing the call.
        let keys = collect_gemini_api_keys(&self.api_key, &self.extra_api_keys);
        if keys.is_empty() {
            anyhow::bail!("No API keys configured for provider \"google\".");
        }

        let mut last_error: Option<anyhow::Error> = None;
        let mut api_resp: Option<GeminiResponse> = None;
        let key_count = keys.len();
        for (index, key) in keys.into_iter().enumerate() {
            let builder = self
                .client
                .post(&url)
                .header("Content-Type", "application/json")
                .header("x-goog-api-key", &key)
                .json(&body);
            let resp = self.send_with_abort(builder).await?;

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                let message = format!("Gemini API error ({}): {}", status, text);
                let rate_limited =
                    status.as_u16() == 429 || is_api_key_rate_limit_error(&message);
                last_error = Some(anyhow::anyhow!(message));
                if rate_limited && index + 1 < key_count {
                    continue;
                }
                return Err(last_error.unwrap());
            }

            api_resp = Some(resp.json().await?);
            break;
        }
        let api_resp = match api_resp {
            Some(resp) => resp,
            None => return Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Gemini API error"))),
        };

        let mut content = Vec::new();
        let mut stop_reason = None;
        let mut tool_call_counter = 0u32;

        if let Some(candidates) = api_resp.candidates {
            if let Some(candidate) = candidates.into_iter().next() {
                stop_reason = candidate.finish_reason;
                if let Some(c) = candidate.content {
                    // Parts are consumed strictly in order so parallel tool
                    // calls replay in the order the model produced them
                    // (v2026.7.1 ordered parallel tool responses).
                    for part in c.parts {
                        if let Some(text) = part.text {
                            content.push(ContentBlock::Text(text));
                        }
                        if let Some(call) = part.function_call {
                            let name = call
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            tool_call_counter += 1;
                            let id = call
                                .get("id")
                                .and_then(|i| i.as_str())
                                .map(String::from)
                                .unwrap_or_else(|| {
                                    format!("{}_{}", name, tool_call_counter)
                                });
                            let input = call
                                .get("args")
                                .cloned()
                                .unwrap_or(serde_json::Value::Object(Default::default()));
                            content.push(ContentBlock::ToolUse { id, name, input });
                        }
                    }
                }
            }
        }

        let usage_meta = api_resp.usage_metadata.unwrap_or(GeminiUsageMetadata {
            prompt_token_count: None,
            candidates_token_count: None,
        });

        Ok(ProviderResponse {
            content,
            stop_reason,
            usage: crate::gateway::TokenUsage {
                input_tokens: usage_meta.prompt_token_count,
                output_tokens: usage_meta.candidates_token_count,
                cache_read_tokens: None,
                cache_write_tokens: None,
            },
        })
    }

    async fn stream_chat(&self, request: ProviderRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        // v2026.7.1: real SSE streaming via `streamGenerateContent?alt=sse`.
        let body = self.build_request_body(&request);

        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            self.base_url, self.model
        );

        let builder = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("x-goog-api-key", &self.api_key)
            .json(&body);
        let response = self.send_with_abort(builder).await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!("Gemini API error ({}): {}", status, text);
        }

        let (tx, rx) = mpsc::channel(256);
        let abort = self.abort.clone();

        tokio::spawn(async move {
            use futures::StreamExt;
            let mut byte_stream = response.bytes_stream();
            let mut buffer = String::new();
            let mut usage = crate::gateway::TokenUsage {
                input_tokens: None,
                output_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
            };

            loop {
                let next_chunk = byte_stream.next();
                let chunk = match &abort {
                    Some(handle) => {
                        match crate::infra::abort_signal::monitor_with_abort_lifecycle(
                            next_chunk, handle,
                        )
                        .await
                        {
                            Ok(chunk) => chunk,
                            Err(_) => {
                                let _ = tx
                                    .send(StreamEvent::Error("Gemini stream aborted".to_string()))
                                    .await;
                                return;
                            }
                        }
                    }
                    None => next_chunk.await,
                };
                let Some(chunk) = chunk else { break };
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx
                            .send(StreamEvent::Error(format!("Gemini stream error: {}", e)))
                            .await;
                        return;
                    }
                };
                buffer.push_str(&String::from_utf8_lossy(&bytes));

                while let Some(pos) = buffer.find('\n') {
                    let line = buffer[..pos].trim_end_matches('\r').to_string();
                    buffer.drain(..=pos);
                    let Some(data) = line.strip_prefix("data: ") else {
                        continue;
                    };
                    if data == "[DONE]" {
                        continue;
                    }
                    let Ok(chunk_json) = serde_json::from_str::<serde_json::Value>(data) else {
                        continue;
                    };
                    if let Some(meta) = chunk_json.get("usageMetadata") {
                        if let Some(v) = meta.get("promptTokenCount").and_then(|v| v.as_u64()) {
                            usage.input_tokens = Some(v);
                        }
                        if let Some(v) =
                            meta.get("candidatesTokenCount").and_then(|v| v.as_u64())
                        {
                            usage.output_tokens = Some(v);
                        }
                    }
                    if let Some(parts) = chunk_json
                        .pointer("/candidates/0/content/parts")
                        .and_then(|p| p.as_array())
                    {
                        for part in parts {
                            for event in classify_stream_part(part) {
                                if tx.send(event).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
            }

            let _ = tx.send(StreamEvent::Done(usage)).await;
        });

        Ok(rx)
    }

    fn name(&self) -> &str {
        "google"
    }
}

/// Classify one streamed content part into stream events (v2026.7.1 parity
/// with upstream `extensions/google/transport-stream.ts`, #76080).
///
/// - text parts → `Delta` (or `Thinking` when `thought: true`);
/// - functionCall parts → `ToolCall`;
/// - **thinking-signature-only parts** (a `thoughtSignature` with no text and
///   no functionCall) → an empty `Thinking` delta. Gemini 3.1 Pro Preview
///   emits these during long reasoning phases before any visible text;
///   emitting an event keeps idle-timeout wrappers from killing the stream.
pub fn classify_stream_part(part: &serde_json::Value) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    let has_text = part.get("text").and_then(|t| t.as_str());
    let has_signature = part
        .get("thoughtSignature")
        .and_then(|s| s.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let function_call = part.get("functionCall").filter(|f| !f.is_null());
    let is_thought = part.get("thought").and_then(|t| t.as_bool()).unwrap_or(false);

    if let Some(text) = has_text {
        if is_thought {
            events.push(StreamEvent::Thinking(text.to_string()));
        } else {
            events.push(StreamEvent::Delta(text.to_string()));
        }
    } else if has_signature && function_call.is_none() {
        // Signature-only keep-alive: an empty thinking delta marks model
        // activity without altering accumulated content.
        events.push(StreamEvent::Thinking(String::new()));
    }

    if let Some(call) = function_call {
        events.push(StreamEvent::ToolCall(call.clone()));
    }
    events
}

// ============================================================================
// Native PDF analysis (v2026.7.1)
// ============================================================================

/// Analyze PDFs with Gemini's native `generateContent` PDF support (v2026.7.1
/// parity with upstream `geminiAnalyzePdf`). The API key is sent via the
/// `x-goog-api-key` header — never as a URL query parameter.
pub async fn gemini_analyze_pdf(
    api_key: &str,
    model_id: &str,
    prompt: &str,
    pdfs_base64: &[String],
    base_url: Option<&str>,
) -> Result<String> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        anyhow::bail!("Gemini PDF: apiKey required");
    }
    let base = normalize_google_api_base_url(base_url);
    // Upstream normalizes away a trailing /v1beta before re-appending, so a
    // configured origin and a configured versioned base behave identically.
    let origin = base
        .strip_suffix("/v1beta")
        .unwrap_or(&base)
        .trim_end_matches('/');
    let url = format!("{}/v1beta/models/{}:generateContent", origin, model_id);

    let mut parts: Vec<serde_json::Value> = pdfs_base64
        .iter()
        .map(|data| {
            serde_json::json!({
                "inline_data": {"mime_type": "application/pdf", "data": data}
            })
        })
        .collect();
    parts.push(serde_json::json!({"text": prompt}));

    let client = Client::new();
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("x-goog-api-key", api_key)
        .json(&serde_json::json!({
            "contents": [{"role": "user", "parts": parts}]
        }))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Gemini PDF request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("Gemini PDF request failed ({}): {}", status, text);
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|_| anyhow::anyhow!("Gemini PDF response was not JSON."))?;

    let candidates = json
        .get("candidates")
        .and_then(|c| c.as_array())
        .filter(|c| !c.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Gemini PDF returned no candidates."))?;
    let text: String = candidates[0]
        .pointer("/content/parts")
        .and_then(|p| p.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    if text.trim().is_empty() {
        anyhow::bail!("Gemini PDF returned no text.");
    }
    Ok(text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderMessage;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ------------------------------------------------------------------------
    // Test helpers
    // ------------------------------------------------------------------------

    fn user_msg(text: &str) -> ProviderMessage {
        ProviderMessage {
            role: "user".to_string(),
            content: serde_json::Value::String(text.to_string()),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn req(model: &str) -> ProviderRequest {
        ProviderRequest {
            model: model.to_string(),
            messages: vec![user_msg("hi")],
            max_tokens: None,
            temperature: None,
            stream: false,
            tools: None,
            tool_choice: None,
            thinking: None,
        }
    }

    fn ok_response_json() -> serde_json::Value {
        json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "hello"}]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5}
        })
    }

    async fn mock_with_body(model: &str, body: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/models/{}:generateContent", model)))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    async fn captured(
        server: &MockServer,
    ) -> (
        std::collections::HashMap<String, String>,
        serde_json::Value,
    ) {
        let reqs = server.received_requests().await.expect("requests recorded");
        let r = reqs.first().expect("at least one request");
        let mut hdrs = std::collections::HashMap::new();
        for (name, val) in r.headers.iter() {
            hdrs.insert(
                name.as_str().to_ascii_lowercase(),
                val.to_str().unwrap_or("").to_string(),
            );
        }
        let body: serde_json::Value =
            serde_json::from_slice(&r.body).expect("body is valid JSON");
        (hdrs, body)
    }

    async fn collect_stream(
        mut rx: mpsc::Receiver<StreamEvent>,
    ) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            let done = matches!(ev, StreamEvent::Done(_) | StreamEvent::Error(_));
            out.push(ev);
            if done {
                break;
            }
        }
        out
    }

    fn make_provider(server: &MockServer, model: &str) -> GeminiProvider {
        GeminiProvider::new("test-key".into(), model.into()).with_base_url(server.uri())
    }

    // ------------------------------------------------------------------------
    // OAuth warning constants
    // ------------------------------------------------------------------------

    #[test]
    fn oauth_warning_constant_mentions_google_account() {
        assert!(GEMINI_OAUTH_RISK_WARNING.contains("Google account"));
        assert!(GEMINI_OAUTH_RISK_WARNING.to_lowercase().contains("warning"));
    }

    #[test]
    fn should_warn_oauth_returns_true_by_default() {
        std::env::remove_var("GEMINI_SKIP_OAUTH_WARNING");
        assert!(should_warn_oauth());
    }

    #[test]
    fn should_warn_oauth_returns_false_when_env_set() {
        // SAFETY: tests share env vars. Set, assert, unset within the test.
        std::env::set_var("GEMINI_SKIP_OAUTH_WARNING", "1");
        let result = should_warn_oauth();
        std::env::remove_var("GEMINI_SKIP_OAUTH_WARNING");
        assert!(!result);
    }

    // ------------------------------------------------------------------------
    // Message conversion
    // ------------------------------------------------------------------------

    #[test]
    fn convert_messages_remaps_assistant_to_model() {
        let msgs = vec![ProviderMessage {
            role: "assistant".to_string(),
            content: serde_json::Value::String("hi".to_string()),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];
        let converted = convert_messages(msgs);
        assert_eq!(converted[0].role, "model");
    }

    #[test]
    fn convert_messages_preserves_user_role() {
        let converted = convert_messages(vec![user_msg("q")]);
        assert_eq!(converted[0].role, "user");
    }

    #[test]
    fn convert_messages_extracts_text_from_string_content() {
        let converted = convert_messages(vec![user_msg("plain")]);
        assert_eq!(converted[0].parts[0].text.as_deref(), Some("plain"));
    }

    #[test]
    fn convert_messages_serializes_non_string_content_to_json_string() {
        let msg = ProviderMessage {
            role: "user".to_string(),
            content: json!({"complex": [1, 2, 3]}),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        };
        let converted = convert_messages(vec![msg]);
        let text = converted[0].parts[0].text.as_deref().unwrap();
        assert!(
            text.contains("complex") && text.contains('1'),
            "non-string content should fall back to JSON serialization, got: {}",
            text
        );
    }

    #[test]
    fn convert_messages_preserves_order() {
        let msgs = vec![
            user_msg("first"),
            ProviderMessage {
                role: "assistant".to_string(),
                content: serde_json::Value::String("middle".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            user_msg("third"),
        ];
        let converted = convert_messages(msgs);
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0].parts[0].text.as_deref(), Some("first"));
        assert_eq!(converted[1].parts[0].text.as_deref(), Some("middle"));
        assert_eq!(converted[2].parts[0].text.as_deref(), Some("third"));
    }

    // ------------------------------------------------------------------------
    // Endpoint + auth
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn chat_sends_api_key_in_header_not_url() {
        // v2026.7.1: `x-goog-api-key` header replaces the `?key=` query param.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/gemini-1.5-pro:generateContent"))
            .and(wiremock::matchers::header("x-goog-api-key", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response_json()))
            .mount(&server)
            .await;
        let p = make_provider(&server, "gemini-1.5-pro");
        p.chat(req("gemini-1.5-pro")).await.unwrap();

        let requests = server.received_requests().await.unwrap();
        let request_url = requests.first().unwrap().url.as_str().to_string();
        assert!(
            !request_url.contains("key="),
            "API key must not appear in the URL: {request_url}"
        );
    }

    #[tokio::test]
    async fn chat_sets_content_type_json() {
        let server = mock_with_body("m", ok_response_json()).await;
        let p = make_provider(&server, "m");
        p.chat(req("m")).await.unwrap();
        let (h, _) = captured(&server).await;
        assert_eq!(
            h.get("content-type").map(String::as_str),
            Some("application/json")
        );
    }

    #[tokio::test]
    async fn chat_honors_custom_base_url() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1beta/models/m:generateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response_json()))
            .mount(&server)
            .await;
        let p = GeminiProvider::new("k".into(), "m".into())
            .with_base_url(format!("{}/v1beta", server.uri()));
        p.chat(req("m")).await.unwrap();
    }

    // ------------------------------------------------------------------------
    // Request body shaping
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn chat_sends_generation_config_with_max_output_tokens() {
        let server = mock_with_body("m", ok_response_json()).await;
        let p = make_provider(&server, "m");
        let mut r = req("m");
        r.max_tokens = Some(2048);
        p.chat(r).await.unwrap();
        let (_, b) = captured(&server).await;
        assert_eq!(b["generationConfig"]["maxOutputTokens"], 2048);
    }

    #[tokio::test]
    async fn chat_sends_generation_config_with_temperature() {
        let server = mock_with_body("m", ok_response_json()).await;
        let p = make_provider(&server, "m");
        let mut r = req("m");
        r.temperature = Some(0.4);
        p.chat(r).await.unwrap();
        let (_, b) = captured(&server).await;
        assert_eq!(b["generationConfig"]["temperature"], 0.4);
    }

    #[tokio::test]
    async fn chat_omits_optional_generation_config_fields() {
        let server = mock_with_body("m", ok_response_json()).await;
        let p = make_provider(&server, "m");
        p.chat(req("m")).await.unwrap();
        let (_, b) = captured(&server).await;
        let cfg = &b["generationConfig"];
        assert!(cfg.get("maxOutputTokens").is_none());
        assert!(cfg.get("temperature").is_none());
    }

    #[tokio::test]
    async fn chat_sends_contents_array_with_role_and_parts() {
        let server = mock_with_body("m", ok_response_json()).await;
        let p = make_provider(&server, "m");
        p.chat(req("m")).await.unwrap();
        let (_, b) = captured(&server).await;
        assert!(b["contents"].is_array());
        assert_eq!(b["contents"][0]["role"], "user");
        assert_eq!(b["contents"][0]["parts"][0]["text"], "hi");
    }

    // ------------------------------------------------------------------------
    // Response parsing
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn chat_parses_text_from_first_candidate() {
        let server = mock_with_body(
            "m",
            json!({
                "candidates": [{
                    "content": {"role": "model", "parts": [{"text": "world"}]},
                    "finishReason": "STOP"
                }]
            }),
        )
        .await;
        let p = make_provider(&server, "m");
        let r = p.chat(req("m")).await.unwrap();
        assert_eq!(r.content_text(), "world");
        assert_eq!(r.stop_reason.as_deref(), Some("STOP"));
    }

    #[tokio::test]
    async fn chat_concatenates_multiple_parts() {
        let server = mock_with_body(
            "m",
            json!({
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [{"text": "hel"}, {"text": "lo"}]
                    }
                }]
            }),
        )
        .await;
        let p = make_provider(&server, "m");
        let r = p.chat(req("m")).await.unwrap();
        assert_eq!(r.content_text(), "hello");
    }

    #[tokio::test]
    async fn chat_skips_parts_without_text() {
        let server = mock_with_body(
            "m",
            json!({
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [{"text": "kept"}, {}]
                    }
                }]
            }),
        )
        .await;
        let p = make_provider(&server, "m");
        let r = p.chat(req("m")).await.unwrap();
        assert_eq!(r.content_text(), "kept");
        assert_eq!(r.content.len(), 1);
    }

    #[tokio::test]
    async fn chat_handles_missing_candidates() {
        let server = mock_with_body("m", json!({})).await;
        let p = make_provider(&server, "m");
        let r = p.chat(req("m")).await.unwrap();
        assert!(r.content.is_empty());
        assert!(r.stop_reason.is_none());
    }

    #[tokio::test]
    async fn chat_parses_usage_metadata() {
        let server = mock_with_body(
            "m",
            json!({
                "candidates": [{"content": {"role": "model", "parts": [{"text": "x"}]}}],
                "usageMetadata": {"promptTokenCount": 33, "candidatesTokenCount": 11}
            }),
        )
        .await;
        let p = make_provider(&server, "m");
        let r = p.chat(req("m")).await.unwrap();
        assert_eq!(r.usage.input_tokens, Some(33));
        assert_eq!(r.usage.output_tokens, Some(11));
        assert!(r.usage.cache_read_tokens.is_none());
        assert!(r.usage.cache_write_tokens.is_none());
    }

    #[tokio::test]
    async fn chat_handles_missing_usage_metadata() {
        let server = mock_with_body(
            "m",
            json!({
                "candidates": [{"content": {"role": "model", "parts": [{"text": "x"}]}}]
            }),
        )
        .await;
        let p = make_provider(&server, "m");
        let r = p.chat(req("m")).await.unwrap();
        assert!(r.usage.input_tokens.is_none());
        assert!(r.usage.output_tokens.is_none());
    }

    #[tokio::test]
    async fn chat_returns_error_on_non_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/m:generateContent"))
            .respond_with(ResponseTemplate::new(403).set_body_string("permission denied"))
            .mount(&server)
            .await;
        let p = make_provider(&server, "m");
        let err = p.chat(req("m")).await.unwrap_err().to_string();
        assert!(err.contains("403"), "should mention status: {}", err);
        assert!(err.to_lowercase().contains("gemini"), "should mention provider: {}", err);
    }

    // ------------------------------------------------------------------------
    // Streaming (SSE via :streamGenerateContent, v2026.7.1)
    // ------------------------------------------------------------------------

    fn sse_body(chunks: &[serde_json::Value]) -> String {
        chunks
            .iter()
            .map(|c| format!("data: {}\n\n", c))
            .collect::<String>()
    }

    async fn mock_stream(model: &str, body: String) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/models/{}:streamGenerateContent", model)))
            .and(query_param("alt", "sse"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn stream_chat_emits_deltas_then_done_with_usage() {
        let server = mock_stream(
            "m",
            sse_body(&[
                json!({"candidates": [{"content": {"parts": [{"text": "hel"}]}}]}),
                json!({
                    "candidates": [{"content": {"parts": [{"text": "lo"}]}}],
                    "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 3}
                }),
            ]),
        )
        .await;
        let p = make_provider(&server, "m");
        let rx = p.stream_chat(req("m")).await.unwrap();
        let events = collect_stream(rx).await;
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["hel", "lo"]);
        match events.last() {
            Some(StreamEvent::Done(u)) => {
                assert_eq!(u.input_tokens, Some(7));
                assert_eq!(u.output_tokens, Some(3));
            }
            _ => panic!("last event should be Done"),
        }
    }

    #[tokio::test]
    async fn stream_chat_keeps_signature_only_chunks_active() {
        // v2026.7.1 (#76080): Gemini 3.1 Pro Preview emits
        // thoughtSignature-only parts during reasoning. They must produce a
        // stream event (empty thinking delta) so idle-timeout wrappers see
        // activity — but no visible text delta.
        let server = mock_stream(
            "gemini-3.1-pro-preview",
            sse_body(&[
                json!({"candidates": [{"content": {"parts": [
                    {"thought": true, "text": "draft", "thoughtSignature": "sig_1"}
                ]}}]}),
                json!({"candidates": [{"content": {"parts": [
                    {"thoughtSignature": "sig_2"}
                ]}}]}),
                json!({"candidates": [{"content": {"parts": [{"text": "answer"}]}}]}),
            ]),
        )
        .await;
        let p = make_provider(&server, "gemini-3.1-pro-preview");
        let rx = p.stream_chat(req("gemini-3.1-pro-preview")).await.unwrap();
        let events = collect_stream(rx).await;

        let thinking: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Thinking(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        // "draft" from the thought part, "" keep-alive from the
        // signature-only part.
        assert_eq!(thinking, vec!["draft", ""]);
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["answer"]);
    }

    #[tokio::test]
    async fn stream_chat_errors_on_http_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/m:streamGenerateContent"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let p = make_provider(&server, "m");
        let err = p.stream_chat(req("m")).await.unwrap_err().to_string();
        assert!(err.contains("500"), "err: {err}");
    }

    // ------------------------------------------------------------------------
    // Stream part classification (v2026.7.1 keep-alive)
    // ------------------------------------------------------------------------

    #[test]
    fn classify_text_part_as_delta() {
        let events = classify_stream_part(&json!({"text": "hi"}));
        assert!(matches!(&events[..], [StreamEvent::Delta(t)] if t == "hi"));
    }

    #[test]
    fn classify_thought_text_as_thinking() {
        let events = classify_stream_part(&json!({"text": "reasoning", "thought": true}));
        assert!(matches!(&events[..], [StreamEvent::Thinking(t)] if t == "reasoning"));
    }

    #[test]
    fn classify_signature_only_part_as_empty_thinking_keepalive() {
        let events = classify_stream_part(&json!({"thoughtSignature": "sig_abc"}));
        assert!(
            matches!(&events[..], [StreamEvent::Thinking(t)] if t.is_empty()),
            "signature-only part must emit an empty thinking keep-alive"
        );
    }

    #[test]
    fn classify_empty_signature_emits_nothing() {
        assert!(classify_stream_part(&json!({"thoughtSignature": ""})).is_empty());
        assert!(classify_stream_part(&json!({})).is_empty());
    }

    #[test]
    fn classify_signature_with_function_call_is_tool_call_only() {
        // Upstream: signature-only keep-alive excludes functionCall parts —
        // the tool-call event itself is the activity signal.
        let events = classify_stream_part(&json!({
            "thoughtSignature": "sig",
            "functionCall": {"name": "f", "args": {}}
        }));
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::ToolCall(_)));
    }

    // ------------------------------------------------------------------------
    // Abort routing (v2026.7.1)
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn pre_aborted_handle_cancels_chat() {
        let server = mock_with_body("m", ok_response_json()).await;
        let abort = crate::infra::abort_signal::AbortHandle::new();
        abort.abort();
        let p = make_provider(&server, "m").with_abort_handle(abort);
        let err = p.chat(req("m")).await.unwrap_err().to_string();
        assert!(err.to_lowercase().contains("abort"), "err: {err}");
    }

    #[tokio::test]
    async fn abort_during_slow_request_cancels_chat() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/m:generateContent"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(ok_response_json())
                    .set_delay(std::time::Duration::from_secs(30)),
            )
            .mount(&server)
            .await;
        let abort = crate::infra::abort_signal::AbortHandle::new();
        let p = make_provider(&server, "m").with_abort_handle(abort.clone());
        let handle = tokio::spawn(async move { p.chat(req("m")).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        abort.abort();
        let err = handle.await.unwrap().unwrap_err().to_string();
        assert!(err.to_lowercase().contains("abort"), "err: {err}");
    }

    #[tokio::test]
    async fn pre_aborted_handle_cancels_stream() {
        let server = mock_stream("m", sse_body(&[json!({"candidates": []})])).await;
        let abort = crate::infra::abort_signal::AbortHandle::new();
        abort.abort();
        let p = make_provider(&server, "m").with_abort_handle(abort);
        assert!(p.stream_chat(req("m")).await.is_err());
    }

    // ------------------------------------------------------------------------
    // Base URL normalization (v2026.7.1)
    // ------------------------------------------------------------------------

    #[test]
    fn normalize_defaults_when_unset() {
        assert_eq!(normalize_google_api_base_url(None), DEFAULT_GEMINI_BASE_URL);
        assert_eq!(normalize_google_api_base_url(Some("  ")), DEFAULT_GEMINI_BASE_URL);
    }

    #[test]
    fn normalize_adds_v1beta_to_bare_google_origin() {
        assert_eq!(
            normalize_google_api_base_url(Some("https://generativelanguage.googleapis.com")),
            DEFAULT_GEMINI_BASE_URL
        );
        assert_eq!(
            normalize_google_api_base_url(Some("https://generativelanguage.googleapis.com/")),
            DEFAULT_GEMINI_BASE_URL
        );
    }

    #[test]
    fn normalize_strips_query_hash_and_trailing_slashes() {
        assert_eq!(
            normalize_google_api_base_url(Some("https://proxy.corp/v1beta/?x=1#frag")),
            "https://proxy.corp/v1beta"
        );
    }

    #[test]
    fn normalize_leaves_custom_paths_alone() {
        assert_eq!(
            normalize_google_api_base_url(Some("https://proxy.corp/custom")),
            "https://proxy.corp/custom"
        );
    }

    // ------------------------------------------------------------------------
    // Model-id migration (v2026.7.1)
    // ------------------------------------------------------------------------

    #[test]
    fn retired_gemini_3_pro_ids_migrate_to_3_1_preview() {
        for id in ["gemini-3-pro", "gemini-3-pro-preview", "gemini-3.1-pro"] {
            assert_eq!(normalize_google_model_id(id), "gemini-3.1-pro-preview", "{id}");
        }
    }

    #[test]
    fn flash_and_gemma_ids_migrate() {
        assert_eq!(normalize_google_model_id("gemini-3-flash"), "gemini-3-flash-preview");
        assert_eq!(
            normalize_google_model_id("gemini-3.1-flash-lite-preview"),
            "gemini-3.1-flash-lite"
        );
        assert_eq!(normalize_google_model_id("gemini-3.1-flash"), "gemini-3-flash-preview");
        assert_eq!(normalize_google_model_id("gemma-4-26b"), "gemma-4-26b-a4b-it");
        // Current ids pass through untouched.
        assert_eq!(normalize_google_model_id("gemini-3.1-pro-preview"), "gemini-3.1-pro-preview");
        assert_eq!(normalize_google_model_id("gemini-2.5-flash"), "gemini-2.5-flash");
    }

    #[test]
    fn google_prefixed_ids_migrate_in_place() {
        assert_eq!(
            normalize_google_model_id("google/gemini-3-pro-preview"),
            "google/gemini-3.1-pro-preview"
        );
        assert_eq!(
            normalize_google_model_id("google/gemini-2.5-pro"),
            "google/gemini-2.5-pro"
        );
    }

    #[test]
    fn nested_proxy_model_refs_migrate() {
        assert_eq!(
            normalize_google_model_ref("openrouter/google/gemini-3-pro-preview"),
            "openrouter/google/gemini-3.1-pro-preview"
        );
        assert_eq!(
            normalize_google_model_ref("kilocode/google/gemini-3-pro-preview"),
            "kilocode/google/gemini-3.1-pro-preview"
        );
        assert_eq!(
            normalize_google_model_ref("kilocode/openrouter/google/gemini-3-pro"),
            "kilocode/openrouter/google/gemini-3.1-pro-preview"
        );
        assert_eq!(
            normalize_google_model_ref("google-vertex/gemini-3-pro"),
            "google-vertex/gemini-3.1-pro-preview"
        );
        // Non-Google refs pass through.
        assert_eq!(
            normalize_google_model_ref("anthropic/claude-sonnet-5"),
            "anthropic/claude-sonnet-5"
        );
    }

    #[test]
    fn provider_constructor_migrates_retired_model_id() {
        let provider = GeminiProvider::new("k".into(), "gemini-3-pro-preview".into());
        assert_eq!(provider.model, "gemini-3.1-pro-preview");
    }

    // ------------------------------------------------------------------------
    // Vertex multi-region hosts (v2026.7.1)
    // ------------------------------------------------------------------------

    #[test]
    fn vertex_hosts_map_locations() {
        assert_eq!(resolve_vertex_base_host("global"), "https://aiplatform.googleapis.com");
        assert_eq!(
            resolve_vertex_base_host("eu"),
            "https://aiplatform.eu.rep.googleapis.com"
        );
        assert_eq!(
            resolve_vertex_base_host("us"),
            "https://aiplatform.us.rep.googleapis.com"
        );
        assert_eq!(
            resolve_vertex_base_host("europe-west4"),
            "https://europe-west4-aiplatform.googleapis.com"
        );
    }

    #[test]
    fn vertex_model_path_strips_provider_prefix() {
        assert_eq!(strip_google_provider_prefix("google/gemini-3.1-pro-preview"), "gemini-3.1-pro-preview");
        assert_eq!(strip_google_provider_prefix("gemini-3.1-pro-preview"), "gemini-3.1-pro-preview");
    }

    // ------------------------------------------------------------------------
    // Thought signatures (v2026.7.1)
    // ------------------------------------------------------------------------

    #[test]
    fn thought_signature_base64_validation() {
        assert!(is_valid_thought_signature("QUJDRA=="));
        assert!(is_valid_thought_signature("QUJDRA"));
        // Compaction-truncated (length not a multiple of 4) → dropped.
        assert!(!is_valid_thought_signature("QUJDR"));
        assert!(!is_valid_thought_signature(""));
        assert!(!is_valid_thought_signature("not base64!"));
        assert!(!is_valid_thought_signature("QUJD===="));
    }

    #[test]
    fn retain_signature_never_overwrites_with_nothing() {
        assert_eq!(
            retain_thought_signature(Some("sig1".into()), None).as_deref(),
            Some("sig1")
        );
        assert_eq!(
            retain_thought_signature(Some("sig1".into()), Some("")).as_deref(),
            Some("sig1")
        );
        assert_eq!(
            retain_thought_signature(Some("sig1".into()), Some("sig2")).as_deref(),
            Some("sig2")
        );
        assert_eq!(retain_thought_signature(None, None), None);
    }

    #[test]
    fn resolve_signature_requires_same_provider_and_valid_base64() {
        assert_eq!(
            resolve_thought_signature(true, Some("QUJDRA==")).as_deref(),
            Some("QUJDRA==")
        );
        assert_eq!(resolve_thought_signature(false, Some("QUJDRA==")), None);
        assert_eq!(resolve_thought_signature(true, Some("trunc")), None);
        assert_eq!(resolve_thought_signature(true, None), None);
    }

    // ------------------------------------------------------------------------
    // Function-declaration normalization + tools (v2026.7.1)
    // ------------------------------------------------------------------------

    #[test]
    fn schema_sanitizer_strips_meta_declarations_recursively() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": {"x": {}},
            "definitions": {"y": {}},
            "type": "object",
            "properties": {
                "a": {"$comment": "note", "type": "string"}
            }
        });
        let sanitized = sanitize_function_declaration_schema(&schema);
        assert!(sanitized.get("$schema").is_none());
        assert!(sanitized.get("$defs").is_none());
        assert!(sanitized.get("definitions").is_none());
        assert!(sanitized["properties"]["a"].get("$comment").is_none());
        assert_eq!(sanitized["properties"]["a"]["type"], "string");
    }

    #[test]
    fn tools_convert_to_function_declarations() {
        let tools = vec![json!({
            "name": "web_search",
            "description": "Search",
            "input_schema": {"$schema": "x", "type": "object"}
        })];
        let converted = convert_tools_to_gemini(&tools).unwrap();
        let decl = &converted[0]["functionDeclarations"][0];
        assert_eq!(decl["name"], "web_search");
        assert!(decl["parameters"].get("$schema").is_none());
        assert_eq!(decl["parameters"]["type"], "object");
        assert_eq!(convert_tools_to_gemini(&[]), None);
    }

    #[test]
    fn openai_style_tools_also_convert() {
        let tools = vec![json!({
            "type": "function",
            "function": {"name": "f", "description": "d", "parameters": {"type": "object"}}
        })];
        let converted = convert_tools_to_gemini(&tools).unwrap();
        assert_eq!(converted[0]["functionDeclarations"][0]["name"], "f");
    }

    #[test]
    fn tool_choice_maps_to_gemini_modes() {
        assert_eq!(map_tool_choice_to_gemini(&json!("auto")), "AUTO");
        assert_eq!(map_tool_choice_to_gemini(&json!("none")), "NONE");
        assert_eq!(map_tool_choice_to_gemini(&json!("any")), "ANY");
        assert_eq!(map_tool_choice_to_gemini(&json!("required")), "ANY");
        assert_eq!(map_tool_choice_to_gemini(&json!({"type": "tool"})), "ANY");
        assert_eq!(map_tool_choice_to_gemini(&json!(42)), "AUTO");
    }

    #[tokio::test]
    async fn chat_sends_tools_and_parses_ordered_function_calls() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/m:generateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "candidates": [{
                    "content": {"role": "model", "parts": [
                        {"functionCall": {"name": "tool_b", "args": {"x": 1}}},
                        {"text": "between"},
                        {"functionCall": {"name": "tool_a", "args": {"y": 2}}}
                    ]},
                    "finishReason": "STOP"
                }]
            })))
            .mount(&server)
            .await;
        let p = make_provider(&server, "m");
        let mut r = req("m");
        r.tools = Some(vec![json!({"name": "tool_a", "description": "", "input_schema": {}})]);
        r.tool_choice = Some(json!("auto"));
        let resp = p.chat(r).await.unwrap();

        // Part order preserved: call(tool_b), text, call(tool_a).
        assert_eq!(resp.content.len(), 3);
        match &resp.content[0] {
            ContentBlock::ToolUse { name, input, .. } => {
                assert_eq!(name, "tool_b");
                assert_eq!(input["x"], 1);
            }
            other => panic!("expected ToolUse first, got {other:?}"),
        }
        assert!(matches!(&resp.content[1], ContentBlock::Text(t) if t == "between"));
        match &resp.content[2] {
            ContentBlock::ToolUse { name, .. } => assert_eq!(name, "tool_a"),
            other => panic!("expected ToolUse last, got {other:?}"),
        }

        let (_, body) = captured(&server).await;
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["name"],
            "tool_a"
        );
        assert_eq!(
            body["toolConfig"]["functionCallingConfig"]["mode"],
            "AUTO"
        );
    }

    // ------------------------------------------------------------------------
    // Thinking config (v2026.7.1)
    // ------------------------------------------------------------------------

    #[test]
    fn thinking_enabled_sends_budget_with_floor() {
        let cfg = crate::providers::ThinkingConfig { budget_tokens: 128 };
        let resolved =
            resolve_gemini_thinking_config("gemini-2.5-flash-lite", Some(&cfg)).unwrap();
        assert_eq!(resolved["includeThoughts"], true);
        assert_eq!(resolved["thinkingBudget"], GEMINI_2_5_FLASH_LITE_MIN_BUDGET);
    }

    #[test]
    fn thinking_disabled_uses_model_appropriate_config() {
        // Gemini 3.x Pro cannot disable thinking → lowest level, no
        // includeThoughts (hidden thinking stays invisible).
        let pro = resolve_gemini_thinking_config("gemini-3.1-pro-preview", None).unwrap();
        assert_eq!(pro["thinkingLevel"], "LOW");
        assert!(pro.get("includeThoughts").is_none());
        let flash = resolve_gemini_thinking_config("gemini-3-flash-preview", None).unwrap();
        assert_eq!(flash["thinkingLevel"], "MINIMAL");
        let g2 = resolve_gemini_thinking_config("gemini-2.5-flash", None).unwrap();
        assert_eq!(g2["thinkingBudget"], 0);
        // Non-reasoning models get no thinkingConfig at all.
        assert_eq!(resolve_gemini_thinking_config("gemini-1.5-pro", None), None);
    }

    #[tokio::test]
    async fn chat_sends_thinking_config_for_reasoning_models() {
        let server = mock_with_body("gemini-2.5-flash", ok_response_json()).await;
        let p = make_provider(&server, "gemini-2.5-flash");
        let mut r = req("gemini-2.5-flash");
        r.thinking = Some(crate::providers::ThinkingConfig { budget_tokens: 2048 });
        p.chat(r).await.unwrap();
        let (_, body) = captured(&server).await;
        assert_eq!(body["generationConfig"]["thinkingConfig"]["includeThoughts"], true);
        assert_eq!(body["generationConfig"]["thinkingConfig"]["thinkingBudget"], 2048);
    }

    // ------------------------------------------------------------------------
    // API key rotation (v2026.7.1)
    // ------------------------------------------------------------------------

    #[test]
    fn rate_limit_detection() {
        assert!(is_api_key_rate_limit_error("Gemini API error (429): slow down"));
        assert!(is_api_key_rate_limit_error("RESOURCE_EXHAUSTED: quota"));
        assert!(is_api_key_rate_limit_error("Rate limit exceeded"));
        assert!(!is_api_key_rate_limit_error("Gemini API error (403): denied"));
    }

    #[test]
    fn key_collection_dedupes_preserving_order() {
        let keys = collect_gemini_api_keys(
            "primary",
            &["primary".to_string(), " second ".to_string(), "".to_string()],
        );
        assert_eq!(keys, vec!["primary".to_string(), "second".to_string()]);
    }

    #[tokio::test]
    async fn chat_rotates_to_next_key_on_429() {
        use wiremock::matchers::header;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/m:generateContent"))
            .and(header("x-goog-api-key", "key-a"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/models/m:generateContent"))
            .and(header("x-goog-api-key", "key-b"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response_json()))
            .mount(&server)
            .await;

        let p = GeminiProvider::new("key-a".into(), "m".into())
            .with_base_url(server.uri())
            .with_extra_api_keys(vec!["key-b".to_string()]);
        let resp = p.chat(req("m")).await.unwrap();
        assert_eq!(resp.content_text(), "hello");
    }

    #[tokio::test]
    async fn chat_does_not_rotate_on_non_rate_limit_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/m:generateContent"))
            .respond_with(ResponseTemplate::new(403).set_body_string("denied"))
            .expect(1)
            .mount(&server)
            .await;
        let p = GeminiProvider::new("key-a".into(), "m".into())
            .with_base_url(server.uri())
            .with_extra_api_keys(vec!["key-b".to_string()]);
        let err = p.chat(req("m")).await.unwrap_err().to_string();
        assert!(err.contains("403"), "{err}");
    }

    // ------------------------------------------------------------------------
    // Native PDF analysis (v2026.7.1)
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn pdf_analysis_sends_key_header_not_url() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1beta/models/gemini-2.5-flash:generateContent"))
            .and(wiremock::matchers::header("x-goog-api-key", "pdf-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "candidates": [{"content": {"parts": [{"text": "pdf summary"}]}}]
            })))
            .mount(&server)
            .await;

        let text = gemini_analyze_pdf(
            "pdf-key",
            "gemini-2.5-flash",
            "Summarize",
            &["aGVsbG8=".to_string()],
            Some(&server.uri()),
        )
        .await
        .unwrap();
        assert_eq!(text, "pdf summary");

        let requests = server.received_requests().await.unwrap();
        let r = requests.first().unwrap();
        assert!(
            !r.url.as_str().contains("key="),
            "PDF API key must not be in URL: {}",
            r.url
        );
        let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(
            body["contents"][0]["parts"][0]["inline_data"]["mime_type"],
            "application/pdf"
        );
    }

    #[tokio::test]
    async fn pdf_analysis_rejects_empty_key_and_empty_candidates() {
        assert!(gemini_analyze_pdf("", "m", "p", &[], None).await.is_err());

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1beta/models/m:generateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"candidates": []})))
            .mount(&server)
            .await;
        let err = gemini_analyze_pdf("k", "m", "p", &[], Some(&server.uri()))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no candidates"), "{err}");
    }

    // ------------------------------------------------------------------------
    // Provider name
    // ------------------------------------------------------------------------

    #[test]
    fn name_is_google() {
        let p = GeminiProvider::new("k".into(), "m".into());
        assert_eq!(p.name(), "google");
    }

    #[test]
    fn default_base_url_points_to_generative_language_v1beta() {
        assert_eq!(
            DEFAULT_GEMINI_BASE_URL,
            "https://generativelanguage.googleapis.com/v1beta"
        );
    }

    // ------------------------------------------------------------------------
    // Thinking budget floor (v2026.5.2)
    // ------------------------------------------------------------------------

    #[test]
    fn flash_lite_raises_low_budget_to_floor() {
        assert_eq!(
            effective_thinking_budget("gemini-2.5-flash-lite", 128),
            GEMINI_2_5_FLASH_LITE_MIN_BUDGET
        );
        assert_eq!(
            effective_thinking_budget("gemini-2.5-flash-lite", 0),
            GEMINI_2_5_FLASH_LITE_MIN_BUDGET
        );
    }

    #[test]
    fn flash_lite_preserves_higher_budget() {
        assert_eq!(
            effective_thinking_budget("gemini-2.5-flash-lite", 4096),
            4096
        );
    }

    #[test]
    fn flash_lite_match_is_case_insensitive_and_tolerates_separator_variants() {
        assert_eq!(
            effective_thinking_budget("Gemini-2.5-Flash-Lite", 32),
            GEMINI_2_5_FLASH_LITE_MIN_BUDGET
        );
        assert_eq!(
            effective_thinking_budget("gemini-2.5-flashlite-preview", 32),
            GEMINI_2_5_FLASH_LITE_MIN_BUDGET
        );
    }

    #[test]
    fn pro_and_flash_minimal_budgets_unchanged() {
        // The fix is Flash-Lite-specific; Pro and Flash still accept low presets.
        assert_eq!(effective_thinking_budget("gemini-2.5-pro", 32), 32);
        assert_eq!(effective_thinking_budget("gemini-2.5-flash", 64), 64);
    }

    #[test]
    fn unknown_model_passes_budget_through() {
        assert_eq!(effective_thinking_budget("some-other-model", 17), 17);
    }
}
