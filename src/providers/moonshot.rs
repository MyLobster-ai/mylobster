//! Kimi / Moonshot provider helpers (v2026.5.x).
//!
//! * **Alias canonicalization** — `moonshotai/` refs canonicalize to
//!   `moonshot/`, and the retired `kimi-code` id maps to `kimi-for-coding`.
//! * **Schema hardening** — Moonshot rejects the JSON-schema `minLength`
//!   keyword; it is stripped from tool parameter schemas.
//! * **Billing classification** — Moonshot signals exhausted balance with a
//!   429 whose body mentions the account balance; that is a permanent
//!   billing failure, not a retryable rate limit.
//! * **Thinking budgets** — thinking variants need output headroom on top of
//!   the thinking budget.

use super::manifest::{self, ManifestModel};

/// Default Moonshot endpoint.
pub const MOONSHOT_DEFAULT_BASE_URL: &str = "https://api.moonshot.ai/v1";

/// Canonicalize a Kimi/Moonshot model ref (v2026.5.x):
/// `moonshotai/<id>` → `moonshot/<id>`, `kimi/<id>` → `moonshot/<id>`, and
/// the retired `kimi-code` id → `kimi-for-coding`.
pub fn canonicalize_moonshot_ref(model_ref: &str) -> String {
    let trimmed = model_ref.trim();
    let lower = trimmed.to_ascii_lowercase();
    let (provider, model) = if let Some(rest) = lower.strip_prefix("moonshotai/") {
        (Some("moonshot"), rest.to_string())
    } else if let Some(rest) = lower.strip_prefix("moonshot/") {
        (Some("moonshot"), rest.to_string())
    } else if let Some(rest) = lower.strip_prefix("kimi/") {
        (Some("moonshot"), rest.to_string())
    } else {
        (None, lower)
    };
    let model = if model == "kimi-code" {
        "kimi-for-coding".to_string()
    } else {
        model
    };
    match provider {
        Some(provider) => format!("{}/{}", provider, model),
        None => model,
    }
}

/// JSON-schema keywords Moonshot rejects in tool parameter schemas.
pub const MOONSHOT_UNSUPPORTED_SCHEMA_KEYWORDS: &[&str] = &["minLength"];

/// Strip Moonshot-rejected schema keywords from tool definitions.
pub fn strip_moonshot_schema_keywords(tools: &mut [serde_json::Value]) {
    let keywords: Vec<String> = MOONSHOT_UNSUPPORTED_SCHEMA_KEYWORDS
        .iter()
        .map(|k| k.to_string())
        .collect();
    super::openai_compat::strip_unsupported_schema_keywords(tools, &keywords);
}

/// Classify a Moonshot 429: balance-exhausted bodies are permanent billing
/// failures (do not retry/cool the profile as a rate limit).
pub fn is_moonshot_billing_429(status: u16, body: &str) -> bool {
    if status != 429 {
        return false;
    }
    let lower = body.to_ascii_lowercase();
    lower.contains("balance") || lower.contains("recharge") || lower.contains("arrears")
}

/// Output-room floor added on top of a thinking budget for Kimi thinking
/// variants (v2026.5.x thinking budgets/output room).
pub const MOONSHOT_THINKING_OUTPUT_HEADROOM_TOKENS: u64 = 8_192;

/// Effective max_tokens for a thinking-enabled Kimi call.
pub fn moonshot_thinking_max_tokens(budget_tokens: u64, requested_max: Option<u64>) -> u64 {
    let floor = budget_tokens + MOONSHOT_THINKING_OUTPUT_HEADROOM_TOKENS;
    requested_max.map_or(floor, |m| m.max(floor))
}

/// Manifest-driven Moonshot catalog.
pub fn moonshot_catalog() -> &'static [ManifestModel] {
    manifest::manifest_models("moonshot")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonicalizes_moonshotai_prefix() {
        assert_eq!(canonicalize_moonshot_ref("moonshotai/kimi-k2.5"), "moonshot/kimi-k2.5");
        assert_eq!(canonicalize_moonshot_ref("MoonshotAI/Kimi-K2.5"), "moonshot/kimi-k2.5");
        assert_eq!(canonicalize_moonshot_ref("kimi/kimi-k2.6"), "moonshot/kimi-k2.6");
    }

    #[test]
    fn maps_retired_kimi_code_alias() {
        assert_eq!(canonicalize_moonshot_ref("kimi-code"), "kimi-for-coding");
        assert_eq!(
            canonicalize_moonshot_ref("moonshotai/kimi-code"),
            "moonshot/kimi-for-coding"
        );
    }

    #[test]
    fn bare_ids_pass_through() {
        assert_eq!(canonicalize_moonshot_ref("kimi-k2-thinking"), "kimi-k2-thinking");
    }

    #[test]
    fn strips_min_length_keyword() {
        let mut tools = vec![json!({"type": "function", "function": {"name": "f",
            "parameters": {"type": "object", "properties": {
                "q": {"type": "string", "minLength": 1}}}}})];
        strip_moonshot_schema_keywords(&mut tools);
        assert!(tools[0]
            .pointer("/function/parameters/properties/q/minLength")
            .is_none());
        assert_eq!(
            tools[0].pointer("/function/parameters/properties/q/type").unwrap(),
            "string"
        );
    }

    #[test]
    fn billing_429_classification() {
        assert!(is_moonshot_billing_429(429, "account balance not enough, please recharge"));
        assert!(!is_moonshot_billing_429(429, "rate limit reached, retry later"));
        assert!(!is_moonshot_billing_429(400, "balance"));
    }

    #[test]
    fn thinking_max_tokens_gets_headroom() {
        assert_eq!(moonshot_thinking_max_tokens(4_096, None), 12_288);
        assert_eq!(moonshot_thinking_max_tokens(4_096, Some(4_096)), 12_288);
        assert_eq!(moonshot_thinking_max_tokens(4_096, Some(32_000)), 32_000);
    }

    #[test]
    fn catalog_has_current_kimi_family() {
        let ids: Vec<&str> = moonshot_catalog().iter().map(|m| m.id).collect();
        assert!(ids.contains(&"kimi-k2.5"));
        assert!(ids.contains(&"kimi-k2.6"));
        assert!(ids.contains(&"kimi-k2.7-code"));
    }
}
