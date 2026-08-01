//! Compaction policy helpers (OpenClaw v2026.5.2 / v2026.7.1 parity).
//!
//! Behavior core for transcript compaction decisions:
//! - Mid-turn compaction precheck (`agents.defaults.compaction.midTurnPrecheck`).
//! - `maxActiveTranscriptBytes` preflight compaction trigger (v2026.4.26
//!   carryover; `agents.defaults.compaction.maxActiveTranscriptBytes`).
//! - Keep-prior-context on consecutive turns for z.ai-style providers
//!   (delegates to `providers::zai::keeps_prior_context`).
//! - Active-session model fallback chain for implicit summarization failures
//!   (Azure content-filter 400 recovery).
//! - Non-empty runtime-event marker for pre-compaction memory-flush turns.
//! - Default compaction timeout (180s, v2026.6.x) and reserve-token clamping
//!   to the model's `maxTokens` with a small-local-model cap.

use crate::config::types::{AgentCompactionConfig, AgentModelConfig};
use crate::config::Config;
use crate::providers::ProviderMessage;

use std::time::Duration;

/// Default compaction timeout (v2026.6.x raised the default to 180s).
pub const DEFAULT_COMPACTION_TIMEOUT: Duration = Duration::from_secs(180);

/// Reserve-token cap applied to small local models (v2026.7.1 sizing pass).
pub const SMALL_MODEL_RESERVE_CAP: u64 = 1024;

/// Context windows at or below this are treated as "small local model".
pub const SMALL_MODEL_CONTEXT_WINDOW: u64 = 16_384;

// ============================================================================
// Mid-turn precheck (v2026.5.2)
// ============================================================================

/// Whether the mid-turn compaction precheck is enabled
/// (`agents.defaults.compaction.midTurnPrecheck`, default off).
pub fn mid_turn_precheck_enabled(cfg: &AgentCompactionConfig) -> bool {
    cfg.mid_turn_precheck.unwrap_or(false)
}

/// Mid-turn precheck decision: between tool-loop iterations, should the run
/// compact before issuing the next provider call?
///
/// Triggers when the precheck is enabled and either:
/// - the estimated transcript bytes exceed `maxActiveTranscriptBytes` (when
///   configured), or
/// - the estimated token usage crosses the context-window share threshold
///   (default 0.85 of `contextTokens` when configured).
pub fn should_compact_mid_turn(
    cfg: &AgentCompactionConfig,
    context_tokens: Option<u64>,
    estimated_tokens: u64,
    transcript_bytes: u64,
) -> bool {
    if !mid_turn_precheck_enabled(cfg) {
        return false;
    }
    if let Some(max_bytes) = cfg.max_active_transcript_bytes {
        if max_bytes > 0 && transcript_bytes > max_bytes {
            return true;
        }
    }
    if let Some(window) = context_tokens {
        if window > 0 {
            let share = cfg.max_history_share.unwrap_or(0.85).clamp(0.1, 1.0);
            let threshold = (window as f64 * share) as u64;
            return estimated_tokens >= threshold;
        }
    }
    false
}

// ============================================================================
// maxActiveTranscriptBytes preflight trigger (v2026.4.26 carryover)
// ============================================================================

/// Preflight check run before a turn starts: when the active transcript
/// exceeds `maxActiveTranscriptBytes`, compaction runs before the provider is
/// called. Disabled when unset or 0.
pub fn should_preflight_compact(cfg: &AgentCompactionConfig, transcript_bytes: u64) -> bool {
    match cfg.max_active_transcript_bytes {
        Some(max) if max > 0 => transcript_bytes > max,
        _ => false,
    }
}

/// Rough byte estimate of the active transcript (serialized message content).
pub fn estimate_transcript_bytes(messages: &[ProviderMessage]) -> u64 {
    messages
        .iter()
        .map(|m| {
            let content = match &m.content {
                serde_json::Value::String(s) => s.len() as u64,
                other => serde_json::to_string(other).map(|s| s.len()).unwrap_or(0) as u64,
            };
            let tools = m
                .tool_calls
                .as_ref()
                .map(|tc| {
                    tc.iter()
                        .map(|c| serde_json::to_string(c).map(|s| s.len()).unwrap_or(0) as u64)
                        .sum::<u64>()
                })
                .unwrap_or(0);
            content + tools + m.role.len() as u64
        })
        .sum()
}

/// CJK-aware token estimate (v2026.7.1 sizing pass): ASCII text ≈ 4 chars per
/// token; CJK codepoints ≈ 1 token each.
pub fn estimate_tokens(text: &str) -> u64 {
    let mut ascii_ish = 0u64;
    let mut cjk = 0u64;
    for ch in text.chars() {
        let cp = ch as u32;
        let is_cjk = (0x3000..=0x9FFF).contains(&cp)
            || (0xF900..=0xFAFF).contains(&cp)
            || (0xAC00..=0xD7AF).contains(&cp)
            || (0x20000..=0x2FA1F).contains(&cp);
        if is_cjk {
            cjk += 1;
        } else {
            ascii_ish += 1;
        }
    }
    ascii_ish / 4 + cjk
}

// ============================================================================
// Keep prior context for z.ai-style providers (v2026.5.2)
// ============================================================================

/// Whether compaction must keep prior context on consecutive turns for this
/// provider/model (z.ai direct, `openrouter z-ai/*`, in-house GLM gateways).
///
/// These providers silently reset Pi-style state on context overflow instead
/// of erroring, so dropping prior context on consecutive turns loses history.
pub fn keeps_prior_context(
    provider: Option<&str>,
    model_id: Option<&str>,
    base_url: Option<&str>,
) -> bool {
    crate::providers::zai::keeps_prior_context(provider, model_id, base_url)
}

// ============================================================================
// Summarization model fallback chain (v2026.5.2)
// ============================================================================

/// Model chain used for implicit summarization (compaction) runs.
///
/// The active session model is tried first, then the configured fallback
/// chain (deduplicated). Recovers from summarization-model-specific failures
/// such as Azure content-filter 400s.
pub fn summarization_model_chain(config: &Config, active_model: &str) -> Vec<String> {
    let mut chain: Vec<String> = Vec::new();
    let mut push = |m: &str| {
        let m = m.trim();
        if !m.is_empty() && !chain.iter().any(|existing| existing == m) {
            chain.push(m.to_string());
        }
    };

    push(active_model);
    match &config.agent.model {
        AgentModelConfig::Simple(primary) => push(primary),
        AgentModelConfig::Detailed(list) => {
            if let Some(primary) = &list.primary {
                push(primary);
            }
            for fb in &list.fallbacks {
                push(fb);
            }
        }
    }
    chain
}

/// Whether a summarization failure looks like a content-filter rejection
/// (e.g. Azure OpenAI 400 `content_filter`) that should advance the chain
/// instead of aborting compaction.
pub fn is_summarization_content_filter_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("content_filter")
        || lower.contains("content filter")
        || lower.contains("responsibleaipolicyviolation")
        || (lower.contains("400") && lower.contains("filtered"))
}

// ============================================================================
// Pre-compaction memory flush marker (v2026.5.2)
// ============================================================================

/// Non-empty runtime-event marker submitted as the user-turn text for
/// pre-compaction memory-flush turns. Some providers reject empty user
/// messages, so the marker must be non-empty; reply sanitization strips it
/// from anything user-visible.
pub fn memory_flush_turn_marker() -> String {
    format!(
        "{} pre-compaction memory flush",
        crate::agents::reply_sanitize::RUNTIME_EVENT_SENTINEL_PREFIX
    )
}

/// Whether a user-turn text is an internal runtime-event marker.
pub fn is_runtime_event_marker(text: &str) -> bool {
    text.trim_start()
        .starts_with(crate::agents::reply_sanitize::RUNTIME_EVENT_SENTINEL_PREFIX)
}

// ============================================================================
// Reserve sizing (v2026.7.1 partial)
// ============================================================================

/// Clamp the configured compaction reserve to the model's `maxTokens`, with
/// a hard cap for small local models.
pub fn clamp_reserve_tokens(
    configured_reserve: u64,
    model_max_tokens: Option<u64>,
    context_window: Option<u64>,
) -> u64 {
    let mut reserve = configured_reserve;
    if let Some(max) = model_max_tokens {
        if max > 0 {
            reserve = reserve.min(max);
        }
    }
    if let Some(window) = context_window {
        if window > 0 && window <= SMALL_MODEL_CONTEXT_WINDOW {
            reserve = reserve.min(SMALL_MODEL_RESERVE_CAP);
        }
    }
    reserve
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::AgentModelListConfig;

    fn compaction_cfg(
        precheck: Option<bool>,
        max_bytes: Option<u64>,
        share: Option<f64>,
    ) -> AgentCompactionConfig {
        AgentCompactionConfig {
            mid_turn_precheck: precheck,
            max_active_transcript_bytes: max_bytes,
            max_history_share: share,
            ..Default::default()
        }
    }

    fn msg(role: &str, text: &str) -> ProviderMessage {
        ProviderMessage {
            role: role.to_string(),
            content: serde_json::Value::String(text.to_string()),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    // ------------------------------------------------------------------
    // mid-turn precheck
    // ------------------------------------------------------------------

    #[test]
    fn precheck_disabled_by_default() {
        let cfg = AgentCompactionConfig::default();
        assert!(!mid_turn_precheck_enabled(&cfg));
        assert!(!should_compact_mid_turn(&cfg, Some(1000), 10_000, 10_000_000));
    }

    #[test]
    fn precheck_triggers_on_transcript_bytes() {
        let cfg = compaction_cfg(Some(true), Some(1000), None);
        assert!(should_compact_mid_turn(&cfg, None, 0, 1001));
        assert!(!should_compact_mid_turn(&cfg, None, 0, 1000));
    }

    #[test]
    fn precheck_triggers_on_token_share() {
        let cfg = compaction_cfg(Some(true), None, None);
        // default share 0.85 of 10_000 = 8_500
        assert!(should_compact_mid_turn(&cfg, Some(10_000), 8_500, 0));
        assert!(!should_compact_mid_turn(&cfg, Some(10_000), 8_499, 0));
    }

    #[test]
    fn precheck_honors_custom_share() {
        let cfg = compaction_cfg(Some(true), None, Some(0.5));
        assert!(should_compact_mid_turn(&cfg, Some(10_000), 5_000, 0));
        assert!(!should_compact_mid_turn(&cfg, Some(10_000), 4_999, 0));
    }

    #[test]
    fn precheck_no_window_no_bytes_never_triggers() {
        let cfg = compaction_cfg(Some(true), None, None);
        assert!(!should_compact_mid_turn(&cfg, None, u64::MAX, u64::MAX));
    }

    // ------------------------------------------------------------------
    // preflight maxActiveTranscriptBytes
    // ------------------------------------------------------------------

    #[test]
    fn preflight_disabled_when_unset_or_zero() {
        let cfg = AgentCompactionConfig::default();
        assert!(!should_preflight_compact(&cfg, u64::MAX));
        let zero = compaction_cfg(None, Some(0), None);
        assert!(!should_preflight_compact(&zero, u64::MAX));
    }

    #[test]
    fn preflight_triggers_above_threshold() {
        let cfg = compaction_cfg(None, Some(4096), None);
        assert!(should_preflight_compact(&cfg, 4097));
        assert!(!should_preflight_compact(&cfg, 4096));
    }

    #[test]
    fn transcript_bytes_counts_content_and_tool_calls() {
        let mut m = msg("assistant", "hello");
        m.tool_calls = Some(vec![serde_json::json!({"name": "x"})]);
        let msgs = vec![msg("user", "hi"), m];
        let bytes = estimate_transcript_bytes(&msgs);
        // "hi" (2) + "user" (4) + "hello" (5) + "assistant" (9) + tool json
        assert!(bytes > 20, "bytes = {bytes}");
    }

    // ------------------------------------------------------------------
    // token estimation
    // ------------------------------------------------------------------

    #[test]
    fn token_estimate_ascii() {
        assert_eq!(estimate_tokens("abcdefgh"), 2); // 8 chars / 4
    }

    #[test]
    fn token_estimate_cjk_counts_per_char() {
        assert_eq!(estimate_tokens("你好世界"), 4);
    }

    #[test]
    fn token_estimate_mixed() {
        // 8 ascii chars → 2, 2 CJK → 2.
        assert_eq!(estimate_tokens("abcdefgh你好"), 4);
    }

    // ------------------------------------------------------------------
    // z.ai keep-prior-context delegation
    // ------------------------------------------------------------------

    #[test]
    fn zai_provider_keeps_prior_context() {
        assert!(keeps_prior_context(Some("zai"), None, None));
        assert!(keeps_prior_context(None, Some("openrouter/z-ai/glm-5"), None));
        assert!(keeps_prior_context(None, Some("glm-5"), None));
        assert!(!keeps_prior_context(Some("anthropic"), Some("claude-sonnet-4-6"), None));
    }

    // ------------------------------------------------------------------
    // summarization model chain
    // ------------------------------------------------------------------

    #[test]
    fn chain_starts_with_active_model_then_fallbacks() {
        let mut config = Config::default();
        config.agent.model = AgentModelConfig::Detailed(AgentModelListConfig {
            primary: Some("claude-sonnet-4-6".into()),
            fallbacks: vec!["gpt-4.1".into(), "gemini-2.5-pro".into()],
        });
        let chain = summarization_model_chain(&config, "azure/gpt-4o");
        assert_eq!(
            chain,
            vec![
                "azure/gpt-4o".to_string(),
                "claude-sonnet-4-6".to_string(),
                "gpt-4.1".to_string(),
                "gemini-2.5-pro".to_string(),
            ]
        );
    }

    #[test]
    fn chain_dedupes_active_model() {
        let mut config = Config::default();
        config.agent.model = AgentModelConfig::Detailed(AgentModelListConfig {
            primary: Some("claude-sonnet-4-6".into()),
            fallbacks: vec!["claude-sonnet-4-6".into(), "gpt-4.1".into()],
        });
        let chain = summarization_model_chain(&config, "claude-sonnet-4-6");
        assert_eq!(chain, vec!["claude-sonnet-4-6".to_string(), "gpt-4.1".to_string()]);
    }

    #[test]
    fn chain_with_simple_model_config() {
        let config = Config::default(); // Simple("claude-sonnet-4-6")
        let chain = summarization_model_chain(&config, "other-model");
        assert_eq!(
            chain,
            vec!["other-model".to_string(), "claude-sonnet-4-6".to_string()]
        );
    }

    #[test]
    fn content_filter_errors_detected() {
        assert!(is_summarization_content_filter_error(
            "azure returned 400: content_filter triggered"
        ));
        assert!(is_summarization_content_filter_error(
            "ResponsibleAIPolicyViolation: The response was filtered"
        ));
        assert!(!is_summarization_content_filter_error("rate limit exceeded"));
    }

    // ------------------------------------------------------------------
    // memory flush marker
    // ------------------------------------------------------------------

    #[test]
    fn flush_marker_is_non_empty_and_detected() {
        let marker = memory_flush_turn_marker();
        assert!(!marker.trim().is_empty());
        assert!(is_runtime_event_marker(&marker));
        assert!(!is_runtime_event_marker("hello"));
    }

    #[test]
    fn flush_marker_is_stripped_from_user_facing_replies() {
        let marker = memory_flush_turn_marker();
        let sanitized = crate::agents::reply_sanitize::sanitize_user_facing_reply(&marker);
        assert!(sanitized.is_empty(), "marker must never be user-visible: {sanitized:?}");
    }

    // ------------------------------------------------------------------
    // reserve clamping
    // ------------------------------------------------------------------

    #[test]
    fn reserve_clamped_to_model_max_tokens() {
        assert_eq!(clamp_reserve_tokens(8000, Some(4096), None), 4096);
        assert_eq!(clamp_reserve_tokens(2000, Some(4096), None), 2000);
    }

    #[test]
    fn reserve_capped_for_small_local_models() {
        assert_eq!(clamp_reserve_tokens(8000, None, Some(8192)), SMALL_MODEL_RESERVE_CAP);
        assert_eq!(clamp_reserve_tokens(8000, None, Some(128_000)), 8000);
    }

    #[test]
    fn default_timeout_is_180s() {
        assert_eq!(DEFAULT_COMPACTION_TIMEOUT, Duration::from_secs(180));
    }
}
