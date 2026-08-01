//! Miscellaneous provider shims (v2026.5.x–6.x, tracker "Misc providers"
//! row). Small provider-specific behavioral contracts that don't warrant a
//! full module each.

/// Vercel AI Gateway (v2026.6.x): catalogs list live-only model IDs — retired
/// or preview-window ids are filtered out before presentation.
pub fn vercel_gateway_live_model_ids<'a>(ids: &[&'a str]) -> Vec<&'a str> {
    ids.iter()
        .copied()
        .filter(|id| {
            let lower = id.to_ascii_lowercase();
            !lower.contains("-retired") && !lower.contains("-deprecated") && !lower.is_empty()
        })
        .collect()
}

/// Cloudflare AI Gateway (v2026.5.x): Anthropic-style upstreams need the
/// original `x-api-key` header preserved through the gateway instead of
/// being rewritten to a bearer Authorization header.
pub fn cloudflare_gateway_auth_header(upstream_provider: &str) -> &'static str {
    match upstream_provider.trim().to_ascii_lowercase().as_str() {
        "anthropic" => "x-api-key",
        _ => "authorization",
    }
}

/// Together (v2026.5.x): reasoning is toggled via a `reasoning.enabled`
/// request field rather than `reasoning_effort`.
pub fn together_reasoning_body_field(enabled: bool) -> serde_json::Value {
    serde_json::json!({ "reasoning": { "enabled": enabled } })
}

/// Arcee (v2026.6.x): Trinity Large is flagged tool-incompatible.
pub const ARCEE_TOOL_INCOMPATIBLE_MODELS: &[&str] = &["trinity-large"];

/// Whether an Arcee model must have tool payloads stripped.
pub fn arcee_model_tool_incompatible(model_id: &str) -> bool {
    let normalized = model_id
        .trim()
        .to_ascii_lowercase()
        .split('/')
        .next_back()
        .unwrap_or_default()
        .to_string();
    ARCEE_TOOL_INCOMPATIBLE_MODELS.contains(&normalized.as_str())
}

/// Fireworks/BytePlus-hosted Kimi (v2026.6.x): thinking must be disabled —
/// these hosts reject Kimi thinking parameters.
pub fn kimi_thinking_disabled_for_host(provider: &str) -> bool {
    matches!(
        provider.trim().to_ascii_lowercase().as_str(),
        "fireworks" | "byteplus"
    )
}

/// Qwen/vLLM (v2026.6.x): preserve the configured chat template when
/// toggling thinking — only the `enable_thinking` chat-template kwarg
/// changes; the template itself is never replaced.
pub fn qwen_thinking_template_kwargs(enable_thinking: bool) -> serde_json::Value {
    serde_json::json!({ "chat_template_kwargs": { "enable_thinking": enable_thinking } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vercel_filters_retired_ids() {
        let ids = ["gpt-5.5", "gpt-4o-retired", "claude-old-deprecated", ""];
        assert_eq!(vercel_gateway_live_model_ids(&ids), vec!["gpt-5.5"]);
    }

    #[test]
    fn cloudflare_preserves_x_api_key_for_anthropic() {
        assert_eq!(cloudflare_gateway_auth_header("anthropic"), "x-api-key");
        assert_eq!(cloudflare_gateway_auth_header("openai"), "authorization");
    }

    #[test]
    fn together_reasoning_field_shape() {
        let body = together_reasoning_body_field(true);
        assert_eq!(body["reasoning"]["enabled"], true);
    }

    #[test]
    fn arcee_trinity_large_tool_incompatible() {
        assert!(arcee_model_tool_incompatible("trinity-large"));
        assert!(arcee_model_tool_incompatible("arcee/Trinity-Large"));
        assert!(!arcee_model_tool_incompatible("trinity-mini"));
    }

    #[test]
    fn kimi_thinking_off_on_fireworks_and_byteplus() {
        assert!(kimi_thinking_disabled_for_host("fireworks"));
        assert!(kimi_thinking_disabled_for_host("BytePlus"));
        assert!(!kimi_thinking_disabled_for_host("moonshot"));
    }

    #[test]
    fn qwen_template_kwargs_only_toggle_thinking() {
        let body = qwen_thinking_template_kwargs(false);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(body.as_object().unwrap().len(), 1);
    }
}
