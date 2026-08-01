//! OpenRouter provider helpers (v2026.5.2, current v2026.7.1 state).
//!
//! Ports the upstream Completions reasoning-replay contract and the
//! Anthropic-routed prefill safety net:
//!
//! * **Reasoning replay sanitization** — OpenAI Chat Completions assistant
//!   input does not define reasoning replay fields, while OpenRouter and
//!   DeepSeek-style providers document compatible pass-back contracts. Before
//!   a follow-up request hits the wire, assistant messages are sanitized in
//!   one of three modes: preserve-OpenRouter, preserve-`reasoning_content`
//!   (DeepSeek V4 / Kimi / MiMo family), or strip-everything (stock OpenAI).
//!   The current upstream state (issues #76018 → #82150) *strips empty*
//!   `reasoning_content` replay placeholders instead of filling them, so
//!   `openrouter/deepseek/deepseek-v4-*` no longer fails after tool use.
//! * **Trailing assistant prefill stripping** — verified Anthropic-routed
//!   OpenRouter requests reject assistant prefill when reasoning is enabled
//!   (issue #75395); trailing assistant turns without tool calls are dropped.

use super::openai_compat;
use super::{ModelProvider, ProviderRequest, ProviderResponse, StreamEvent};
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use tokio::sync::mpsc;

/// Default OpenRouter endpoint.
pub const OPENROUTER_DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Model ids (final path segment, lowercased) whose providers document a
/// DeepSeek-style `reasoning_content` replay contract.
pub const REASONING_CONTENT_REPLAY_MODEL_IDS: &[&str] = &[
    "deepseek-v4-flash",
    "deepseek-v4-pro",
    "kimi-for-coding",
    "kimi-k2.5",
    "kimi-k2.6",
    "kimi-k2.7-code",
    "kimi-k2-thinking",
    "kimi-k2-thinking-turbo",
    "mimo-v2-pro",
    "mimo-v2-omni",
    "mimo-v2.5",
    "mimo-v2.5-pro",
    "mimo-v2.6-pro",
];

/// Tier/access suffixes some providers append to otherwise identical model
/// ids (`deepseek-v4-flash-free`, etc.). The base id before the suffix still
/// owns the replay contract (#87575).
const REASONING_CONTENT_REPLAY_TIER_SUFFIXES: &[&str] = &["-free", "-paid", "-trial"];

const COMPLETIONS_REASONING_REPLAY_FIELDS: &[&str] = &[
    "reasoning_details",
    "reasoning_content",
    "reasoning",
    "reasoning_text",
];

fn strip_tier_suffix(model_id: &str) -> &str {
    for suffix in REASONING_CONTENT_REPLAY_TIER_SUFFIXES {
        if model_id.len() > suffix.len() && model_id.ends_with(suffix) {
            return &model_id[..model_id.len() - suffix.len()];
        }
    }
    model_id
}

/// Candidate normalized ids for reasoning-content replay matching: the final
/// `/` segment, `:`-separated variants, and tier-suffix-stripped forms.
pub fn reasoning_replay_model_id_candidates(model_id: &str) -> Vec<String> {
    let normalized = model_id.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Vec::new();
    }
    let final_part = normalized
        .split('/')
        .filter(|s| !s.is_empty())
        .next_back()
        .unwrap_or(normalized.as_str())
        .to_string();

    let mut candidates: Vec<String> = vec![final_part.clone()];
    let colon_parts: Vec<&str> = final_part.split(':').filter(|s| !s.is_empty()).collect();
    if colon_parts.len() > 1 {
        candidates.push(colon_parts[0].to_string());
        candidates.push(colon_parts[colon_parts.len() - 1].to_string());
    }
    let base_count = candidates.len();
    for i in 0..base_count {
        let stripped = strip_tier_suffix(&candidates[i]).to_string();
        if stripped != candidates[i] {
            candidates.push(stripped);
        }
    }
    candidates.retain(|c| !c.is_empty());
    candidates.dedup();
    let mut seen = std::collections::HashSet::new();
    candidates.retain(|c| seen.insert(c.clone()));
    candidates
}

/// Whether a model id owns the DeepSeek-style `reasoning_content` replay
/// contract.
pub fn is_reasoning_content_replay_model(model_id: &str) -> bool {
    reasoning_replay_model_id_candidates(model_id)
        .iter()
        .any(|c| REASONING_CONTENT_REPLAY_MODEL_IDS.contains(&c.as_str()))
}

/// Whether OpenRouter reasoning replay should be preserved for this model.
/// Anthropic- and xAI-routed OpenRouter models reject replayed reasoning
/// fields, so they are excluded.
pub fn should_preserve_openrouter_reasoning_replay(provider: &str, model_id: &str) -> bool {
    if provider.trim().to_ascii_lowercase() != "openrouter" {
        return true;
    }
    let normalized = model_id.trim().to_ascii_lowercase();
    !(normalized.starts_with("anthropic/") || normalized.starts_with("x-ai/"))
}

/// Whether an OpenRouter Anthropic-routed model id is in play (used to gate
/// prefill stripping).
pub fn is_openrouter_anthropic_model(model_id: &str) -> bool {
    model_id.trim().to_ascii_lowercase().starts_with("anthropic/")
}

fn strip_all_reasoning_replay_fields(record: &mut serde_json::Map<String, Value>) {
    for field in COMPLETIONS_REASONING_REPLAY_FIELDS {
        record.remove(*field);
    }
}

fn sanitize_openrouter_reasoning_replay_fields(record: &mut serde_json::Map<String, Value>) {
    // reasoning_details: string → promote into `reasoning`; non-array junk → drop.
    match record.get("reasoning_details").cloned() {
        Some(Value::String(details)) => {
            if !details.is_empty() && !matches!(record.get("reasoning"), Some(Value::String(_))) {
                record.insert("reasoning".to_string(), Value::String(details));
            }
            record.remove("reasoning_details");
        }
        Some(Value::Array(_)) | None => {}
        Some(_) => {
            record.remove("reasoning_details");
        }
    }

    // Empty reasoning artifacts are rejected by OpenRouter/DeepSeek replay.
    let reasoning_invalid = match record.get("reasoning") {
        Some(Value::String(s)) => s.is_empty(),
        Some(_) => true,
        None => false,
    };
    if reasoning_invalid {
        record.remove("reasoning");
    }
    let reasoning_content_invalid = match record.get("reasoning_content") {
        Some(Value::String(s)) => s.is_empty(),
        Some(_) => true,
        None => false,
    };
    if reasoning_content_invalid {
        record.remove("reasoning_content");
    }

    // reasoning_text: promote non-empty text into `reasoning` when neither
    // `reasoning` nor `reasoning_content` carry a string; then drop it.
    if let Some(Value::String(text)) = record.get("reasoning_text").cloned() {
        if !text.is_empty()
            && !matches!(record.get("reasoning"), Some(Value::String(_)))
            && !matches!(record.get("reasoning_content"), Some(Value::String(_)))
        {
            record.insert("reasoning".to_string(), Value::String(text));
        }
    }
    record.remove("reasoning_text");
}

fn sanitize_reasoning_content_replay_fields(record: &mut serde_json::Map<String, Value>) {
    if matches!(record.get("reasoning_content"), Some(v) if !v.is_string()) {
        record.remove("reasoning_content");
    }
    record.remove("reasoning_details");
    record.remove("reasoning");
    record.remove("reasoning_text");
}

/// Sanitize assistant-message reasoning replay fields ahead of a follow-up
/// Chat Completions request (port of upstream
/// `sanitizeCompletionsReasoningReplayFields`, v2026.7.1 state).
pub fn sanitize_completions_reasoning_replay_fields(
    messages: &mut [Value],
    preserve_openrouter_reasoning: bool,
    preserve_reasoning_content: bool,
) {
    for msg in messages.iter_mut() {
        let Some(record) = msg.as_object_mut() else {
            continue;
        };
        if record.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        if preserve_openrouter_reasoning {
            sanitize_openrouter_reasoning_replay_fields(record);
        } else if preserve_reasoning_content {
            sanitize_reasoning_content_replay_fields(record);
        } else {
            strip_all_reasoning_replay_fields(record);
        }
    }
}

// ============================================================================
// Trailing assistant prefill stripping (v2026.5.2, issue #75395)
// ============================================================================

fn assistant_message_has_tool_use(message: &serde_json::Map<String, Value>) -> bool {
    if message
        .get("tool_calls")
        .and_then(|tc| tc.as_array())
        .map(|arr| !arr.is_empty())
        .unwrap_or(false)
    {
        return true;
    }
    message
        .get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks.iter().any(|block| {
                matches!(
                    block.get("type").and_then(|t| t.as_str()),
                    Some("tool_use") | Some("toolCall")
                )
            })
        })
        .unwrap_or(false)
}

/// Strip trailing assistant prefill turns (assistant messages without tool
/// calls at the end of the transcript). Returns the number of stripped turns.
///
/// Used for verified Anthropic-routed OpenRouter requests when reasoning is
/// enabled — Anthropic rejects assistant-prefill payloads with thinking on.
pub fn strip_trailing_assistant_prefill_turns(messages: &mut Vec<Value>) -> usize {
    let mut stripped = 0;
    while let Some(last) = messages.last() {
        let Some(record) = last.as_object() else {
            break;
        };
        if record.get("role").and_then(|r| r.as_str()) != Some("assistant")
            || assistant_message_has_tool_use(record)
        {
            break;
        }
        messages.pop();
        stripped += 1;
    }
    stripped
}

/// Apply the OpenRouter request-shaping contract to an outbound Chat
/// Completions message list: reasoning replay sanitization plus (for verified
/// Anthropic-routed requests with reasoning enabled) trailing-prefill
/// stripping.
pub fn prepare_openrouter_messages(
    messages: &mut Vec<Value>,
    model_id: &str,
    reasoning_enabled: bool,
) -> usize {
    sanitize_completions_reasoning_replay_fields(
        messages,
        should_preserve_openrouter_reasoning_replay("openrouter", model_id),
        is_reasoning_content_replay_model(model_id),
    );
    if reasoning_enabled && is_openrouter_anthropic_model(model_id) {
        strip_trailing_assistant_prefill_turns(messages)
    } else {
        0
    }
}

// ============================================================================
// v2026.5.x–7.1 request/response helpers
// ============================================================================

/// Strip a duplicated `openrouter/` prefix from a model id (v2026.5.x:
/// `openrouter/openrouter/auto`-style double prefixes reached the wire).
pub fn normalize_openrouter_model_id(model_id: &str) -> &str {
    let trimmed = model_id.trim();
    let mut current = trimmed;
    while let Some(rest) = current
        .strip_prefix("openrouter/")
        .or_else(|| current.strip_prefix("Openrouter/"))
        .or_else(|| current.strip_prefix("OPENROUTER/"))
    {
        current = rest;
    }
    current
}

/// Opt-in OpenRouter response-cache headers (v2026.6.x `X-OpenRouter-Cache*`).
pub fn openrouter_cache_headers(cache_enabled: bool) -> Vec<(&'static str, &'static str)> {
    if cache_enabled {
        vec![("X-OpenRouter-Cache", "enabled"), ("X-OpenRouter-Cache-Control", "prompt")]
    } else {
        Vec::new()
    }
}

/// Classify an OpenRouter 403: budget/credit-limit bodies are billing
/// failures, not auth failures (v2026.6.x).
pub fn classify_openrouter_403(body: &str) -> super::ProviderErrorKind {
    let lower = body.to_ascii_lowercase();
    if lower.contains("budget") || lower.contains("credit") || lower.contains("limit exceeded") {
        super::ProviderErrorKind::Billing
    } else {
        super::ProviderErrorKind::Auth
    }
}

/// Strip non-replayable provenance tags from reasoning_details entries
/// (v2026.7.1): OpenRouter annotates reasoning details with per-request
/// provenance (`id`/`provider`/`index` fields) that upstream providers
/// reject on replay; only the replayable content fields survive.
pub fn strip_reasoning_provenance_tags(messages: &mut [Value]) {
    for msg in messages.iter_mut() {
        let Some(details) = msg
            .get_mut("reasoning_details")
            .and_then(|d| d.as_array_mut())
        else {
            continue;
        };
        for detail in details.iter_mut() {
            if let Some(obj) = detail.as_object_mut() {
                obj.remove("id");
                obj.remove("provider");
                obj.remove("index");
            }
        }
    }
}

/// Reconcile streamed generation cost from a final usage frame (v2026.7.1:
/// streamed `usage.cost` wins over estimated cost when present).
pub fn reconcile_generation_cost(estimated: Option<f64>, streamed: Option<f64>) -> Option<f64> {
    streamed.filter(|c| c.is_finite() && *c >= 0.0).or(estimated)
}

/// Bound oversized `/models` catalogs (v2026.7.1): keep the first
/// `max_entries` rows instead of buffering unbounded catalogs.
pub fn bound_catalog_rows(rows: &mut Vec<Value>, max_entries: usize) -> usize {
    let dropped = rows.len().saturating_sub(max_entries);
    rows.truncate(max_entries);
    dropped
}

// ============================================================================
// Provider
// ============================================================================

/// OpenRouter provider: OpenAI-compatible transport plus the OpenRouter
/// request-shaping contract (reasoning replay sanitization + verified
/// Anthropic-routed prefill stripping when reasoning is enabled).
pub struct OpenRouterProvider {
    api_key: String,
    base_url: String,
    model: String,
    client: Client,
}

impl OpenRouterProvider {
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        Self {
            api_key,
            base_url,
            // v2026.5.x: duplicated openrouter/ prefixes never reach the wire.
            model: normalize_openrouter_model_id(&model).to_string(),
            client: Client::new(),
        }
    }

    /// Apply the OpenRouter message contract to a typed ProviderRequest.
    fn shape_request(&self, request: &mut ProviderRequest) {
        request.model = normalize_openrouter_model_id(&request.model).to_string();
        let reasoning_enabled = request.thinking.is_some();
        if !reasoning_enabled || !is_openrouter_anthropic_model(&request.model) {
            return;
        }
        // Strip trailing assistant prefill turns (no tool calls) so verified
        // Anthropic-routed requests with reasoning do not hit Anthropic's
        // prefill rejection through the OpenAI-compatible adapter (#75395).
        while let Some(last) = request.messages.last() {
            let is_assistant = last.role == "assistant";
            let has_tool_calls = last
                .tool_calls
                .as_ref()
                .map(|tc| !tc.is_empty())
                .unwrap_or(false);
            if !is_assistant || has_tool_calls {
                break;
            }
            request.messages.pop();
        }
    }
}

#[async_trait]
impl ModelProvider for OpenRouterProvider {
    async fn chat(&self, mut request: ProviderRequest) -> Result<ProviderResponse> {
        self.shape_request(&mut request);
        openai_compat::openai_compat_chat(
            &self.client,
            &self.base_url,
            &self.api_key,
            request,
            "OpenRouter",
        )
        .await
    }

    async fn stream_chat(&self, mut request: ProviderRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        self.shape_request(&mut request);
        openai_compat::openai_compat_stream_chat(
            &self.client,
            &self.base_url,
            &self.api_key,
            request,
            "OpenRouter",
        )
        .await
    }

    fn name(&self) -> &str {
        "openrouter"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assistant(fields: Value) -> Value {
        let mut base = json!({"role": "assistant", "content": "hi"});
        if let (Some(base_map), Some(extra)) = (base.as_object_mut(), fields.as_object()) {
            for (k, v) in extra {
                base_map.insert(k.clone(), v.clone());
            }
        }
        base
    }

    // ------------------------------------------------------------------
    // Model id candidates / replay classification
    // ------------------------------------------------------------------

    #[test]
    fn candidates_use_final_path_segment() {
        assert!(is_reasoning_content_replay_model("openrouter/deepseek/deepseek-v4-flash"));
        assert!(is_reasoning_content_replay_model("deepseek/deepseek-v4-pro"));
    }

    #[test]
    fn candidates_strip_tier_suffixes() {
        assert!(is_reasoning_content_replay_model("deepseek-v4-flash-free"));
        assert!(is_reasoning_content_replay_model("deepseek-v4-pro-trial"));
    }

    #[test]
    fn candidates_handle_colon_variants() {
        assert!(is_reasoning_content_replay_model("deepseek/deepseek-v4-flash:free"));
    }

    #[test]
    fn non_replay_models_not_classified() {
        assert!(!is_reasoning_content_replay_model("gpt-4o"));
        assert!(!is_reasoning_content_replay_model(""));
        assert!(!is_reasoning_content_replay_model("free"));
    }

    #[test]
    fn openrouter_anthropic_and_xai_replay_excluded() {
        assert!(!should_preserve_openrouter_reasoning_replay(
            "openrouter",
            "anthropic/claude-sonnet-4-6"
        ));
        assert!(!should_preserve_openrouter_reasoning_replay(
            "openrouter",
            "x-ai/grok-4.3"
        ));
        assert!(should_preserve_openrouter_reasoning_replay(
            "openrouter",
            "deepseek/deepseek-v4-pro"
        ));
        assert!(should_preserve_openrouter_reasoning_replay(
            "someproxy",
            "anthropic/claude-sonnet-4-6"
        ));
    }

    // ------------------------------------------------------------------
    // Sanitization modes
    // ------------------------------------------------------------------

    #[test]
    fn strip_mode_removes_all_replay_fields() {
        let mut msgs = vec![assistant(json!({
            "reasoning": "r",
            "reasoning_content": "rc",
            "reasoning_details": [{"x": 1}],
            "reasoning_text": "rt"
        }))];
        sanitize_completions_reasoning_replay_fields(&mut msgs, false, false);
        let obj = msgs[0].as_object().unwrap();
        for f in COMPLETIONS_REASONING_REPLAY_FIELDS {
            assert!(!obj.contains_key(*f), "{} should be stripped", f);
        }
    }

    #[test]
    fn strip_mode_ignores_non_assistant_messages() {
        let mut msgs = vec![json!({"role": "user", "content": "x", "reasoning": "keep"})];
        sanitize_completions_reasoning_replay_fields(&mut msgs, false, false);
        assert_eq!(msgs[0]["reasoning"], "keep");
    }

    #[test]
    fn openrouter_mode_drops_empty_reasoning_content_placeholder() {
        // Current upstream state (#82150): empty replay placeholders are
        // stripped so openrouter/deepseek/deepseek-v4-* does not fail after
        // tool use.
        let mut msgs = vec![assistant(json!({"reasoning_content": ""}))];
        sanitize_completions_reasoning_replay_fields(&mut msgs, true, false);
        assert!(!msgs[0].as_object().unwrap().contains_key("reasoning_content"));
    }

    #[test]
    fn openrouter_mode_keeps_non_empty_reasoning_content() {
        let mut msgs = vec![assistant(json!({"reasoning_content": "thought"}))];
        sanitize_completions_reasoning_replay_fields(&mut msgs, true, false);
        assert_eq!(msgs[0]["reasoning_content"], "thought");
    }

    #[test]
    fn openrouter_mode_promotes_string_reasoning_details() {
        let mut msgs = vec![assistant(json!({"reasoning_details": "detail"}))];
        sanitize_completions_reasoning_replay_fields(&mut msgs, true, false);
        let obj = msgs[0].as_object().unwrap();
        assert_eq!(obj.get("reasoning").unwrap(), "detail");
        assert!(!obj.contains_key("reasoning_details"));
    }

    #[test]
    fn openrouter_mode_keeps_array_reasoning_details() {
        let mut msgs = vec![assistant(json!({"reasoning_details": [{"type": "text"}]}))];
        sanitize_completions_reasoning_replay_fields(&mut msgs, true, false);
        assert!(msgs[0]["reasoning_details"].is_array());
    }

    #[test]
    fn openrouter_mode_promotes_reasoning_text() {
        let mut msgs = vec![assistant(json!({"reasoning_text": "rt"}))];
        sanitize_completions_reasoning_replay_fields(&mut msgs, true, false);
        let obj = msgs[0].as_object().unwrap();
        assert_eq!(obj.get("reasoning").unwrap(), "rt");
        assert!(!obj.contains_key("reasoning_text"));
    }

    #[test]
    fn reasoning_content_mode_keeps_only_string_reasoning_content() {
        let mut msgs = vec![assistant(json!({
            "reasoning_content": "rc",
            "reasoning": "r",
            "reasoning_details": [],
            "reasoning_text": "rt"
        }))];
        sanitize_completions_reasoning_replay_fields(&mut msgs, false, true);
        let obj = msgs[0].as_object().unwrap();
        assert_eq!(obj.get("reasoning_content").unwrap(), "rc");
        assert!(!obj.contains_key("reasoning"));
        assert!(!obj.contains_key("reasoning_details"));
        assert!(!obj.contains_key("reasoning_text"));
    }

    #[test]
    fn reasoning_content_mode_drops_non_string_reasoning_content() {
        let mut msgs = vec![assistant(json!({"reasoning_content": {"bad": true}}))];
        sanitize_completions_reasoning_replay_fields(&mut msgs, false, true);
        assert!(!msgs[0].as_object().unwrap().contains_key("reasoning_content"));
    }

    // ------------------------------------------------------------------
    // Prefill stripping
    // ------------------------------------------------------------------

    #[test]
    fn strips_trailing_assistant_prefill() {
        let mut msgs = vec![
            json!({"role": "user", "content": "q"}),
            json!({"role": "assistant", "content": "partial"}),
        ];
        assert_eq!(strip_trailing_assistant_prefill_turns(&mut msgs), 1);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn strips_multiple_trailing_assistant_turns() {
        let mut msgs = vec![
            json!({"role": "user", "content": "q"}),
            json!({"role": "assistant", "content": "a"}),
            json!({"role": "assistant", "content": "b"}),
        ];
        assert_eq!(strip_trailing_assistant_prefill_turns(&mut msgs), 2);
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn keeps_assistant_turn_with_tool_calls() {
        let mut msgs = vec![
            json!({"role": "user", "content": "q"}),
            json!({"role": "assistant", "content": null, "tool_calls": [{"id": "c1"}]}),
        ];
        assert_eq!(strip_trailing_assistant_prefill_turns(&mut msgs), 0);
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn keeps_assistant_turn_with_tool_use_content_block() {
        let mut msgs = vec![json!({
            "role": "assistant",
            "content": [{"type": "tool_use", "id": "t1"}]
        })];
        assert_eq!(strip_trailing_assistant_prefill_turns(&mut msgs), 0);
    }

    #[test]
    fn does_not_strip_user_final_turn() {
        let mut msgs = vec![json!({"role": "user", "content": "q"})];
        assert_eq!(strip_trailing_assistant_prefill_turns(&mut msgs), 0);
        assert_eq!(msgs.len(), 1);
    }

    // ------------------------------------------------------------------
    // prepare_openrouter_messages integration
    // ------------------------------------------------------------------

    #[test]
    fn prepare_strips_prefill_for_anthropic_routed_reasoning_requests() {
        let mut msgs = vec![
            json!({"role": "user", "content": "q"}),
            json!({"role": "assistant", "content": "prefill", "reasoning": "r"}),
        ];
        let stripped =
            prepare_openrouter_messages(&mut msgs, "anthropic/claude-sonnet-4-6", true);
        assert_eq!(stripped, 1);
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn prepare_keeps_prefill_without_reasoning() {
        let mut msgs = vec![
            json!({"role": "user", "content": "q"}),
            json!({"role": "assistant", "content": "prefill"}),
        ];
        let stripped =
            prepare_openrouter_messages(&mut msgs, "anthropic/claude-sonnet-4-6", false);
        assert_eq!(stripped, 0);
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn prepare_keeps_prefill_for_non_anthropic_models() {
        let mut msgs = vec![
            json!({"role": "user", "content": "q"}),
            json!({"role": "assistant", "content": "prefill"}),
        ];
        let stripped =
            prepare_openrouter_messages(&mut msgs, "deepseek/deepseek-v4-pro", true);
        assert_eq!(stripped, 0);
    }

    #[test]
    fn prepare_strips_reasoning_replay_for_anthropic_routed() {
        // anthropic/ prefix excludes OpenRouter reasoning preservation → strip mode.
        let mut msgs = vec![
            json!({"role": "user", "content": "q"}),
            json!({"role": "assistant", "content": "a", "reasoning": "r",
                   "tool_calls": [{"id": "c1"}]}),
        ];
        prepare_openrouter_messages(&mut msgs, "anthropic/claude-sonnet-4-6", true);
        assert!(!msgs[1].as_object().unwrap().contains_key("reasoning"));
    }

    // ------------------------------------------------------------------
    // v2026.5.x–7.1 helpers
    // ------------------------------------------------------------------

    #[test]
    fn duplicated_openrouter_prefix_stripped() {
        assert_eq!(normalize_openrouter_model_id("openrouter/auto"), "auto");
        assert_eq!(
            normalize_openrouter_model_id("openrouter/openrouter/deepseek/deepseek-v4-pro"),
            "deepseek/deepseek-v4-pro"
        );
        assert_eq!(normalize_openrouter_model_id("deepseek/deepseek-v4-pro"),
            "deepseek/deepseek-v4-pro");
    }

    #[test]
    fn cache_headers_opt_in_only() {
        assert!(openrouter_cache_headers(false).is_empty());
        let headers = openrouter_cache_headers(true);
        assert!(headers.iter().any(|(k, _)| *k == "X-OpenRouter-Cache"));
    }

    #[test]
    fn budget_403_classifies_as_billing() {
        assert_eq!(
            classify_openrouter_403("monthly budget limit exceeded"),
            crate::providers::ProviderErrorKind::Billing
        );
        assert_eq!(
            classify_openrouter_403("invalid token"),
            crate::providers::ProviderErrorKind::Auth
        );
    }

    #[test]
    fn provenance_tags_stripped_from_reasoning_details() {
        let mut msgs = vec![json!({"role": "assistant", "content": "x",
            "reasoning_details": [{"type": "text", "text": "t", "id": "gen-1",
                                   "provider": "deepseek", "index": 0}]})];
        strip_reasoning_provenance_tags(&mut msgs);
        let detail = &msgs[0]["reasoning_details"][0];
        assert!(detail.get("id").is_none());
        assert!(detail.get("provider").is_none());
        assert!(detail.get("index").is_none());
        assert_eq!(detail["text"], "t");
    }

    #[test]
    fn streamed_cost_wins_over_estimate() {
        assert_eq!(reconcile_generation_cost(Some(0.5), Some(0.42)), Some(0.42));
        assert_eq!(reconcile_generation_cost(Some(0.5), None), Some(0.5));
        assert_eq!(reconcile_generation_cost(Some(0.5), Some(f64::NAN)), Some(0.5));
        assert_eq!(reconcile_generation_cost(None, None), None);
    }

    #[test]
    fn oversized_catalogs_bounded() {
        let mut rows: Vec<serde_json::Value> = (0..10).map(|i| json!({"id": i})).collect();
        let dropped = bound_catalog_rows(&mut rows, 4);
        assert_eq!(dropped, 6);
        assert_eq!(rows.len(), 4);
    }
}
