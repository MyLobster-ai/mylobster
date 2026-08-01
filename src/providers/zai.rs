//! Z.AI (Zhipu GLM) provider helpers (v2026.5.2).
//!
//! Two parity surfaces live here:
//!
//! 1. **Manifest-driven catalog** — the bundled GLM catalog and auth env
//!    metadata moved into the provider manifest (see
//!    `providers::manifest::ZAI_MANIFEST_MODELS`); this module exposes them
//!    for the model picker / `models list --all --provider zai`.
//! 2. **Keep-prior-context classification** — z.ai-style providers silently
//!    truncate on context overflow ("silent overflow", openclaw#75799), so the
//!    compaction layer must keep prior context on consecutive turns instead of
//!    resetting runtime state. `keeps_prior_context` is the provider-side
//!    classifier; compaction integration consumes it (owned by the agents
//!    cluster).

use super::manifest::{self, ManifestModel};

/// Default Z.AI native endpoint (v2026.5.2 manifest).
pub const ZAI_DEFAULT_BASE_URL: &str = "https://api.z.ai/api/paas/v4";

/// Hostnames classified as the `zai-native` endpoint class.
const ZAI_NATIVE_HOSTS: &[&str] = &["api.z.ai", "open.bigmodel.cn"];

/// Manifest-driven Z.AI model catalog (no duplicated runtime seed data).
pub fn zai_catalog() -> &'static [ManifestModel] {
    manifest::manifest_models("zai")
}

/// Zhipu overload classification (v2026.6.x): Z.AI signals concurrency /
/// system overload with dedicated error codes (`1302` concurrency limit,
/// `1305` frequency limit) or an "overloaded" message. These must classify
/// as retryable overload for correct failover, not generic failures.
pub fn is_zhipu_overload(status: Option<u16>, body: &str) -> bool {
    if matches!(status, Some(429) | Some(503) | Some(529)) {
        return true;
    }
    let lower = body.to_ascii_lowercase();
    lower.contains("\"1302\"")
        || lower.contains("\"code\":1302")
        || lower.contains("\"1305\"")
        || lower.contains("\"code\":1305")
        || lower.contains("overloaded")
        || lower.contains("concurrency limit")
        || lower.contains("high system load")
}

/// Graded GLM thinking (v2026.6.x): `high`/`max` thinking levels map to a
/// reasoning effort instead of the binary enable flag; lower levels use the
/// plain enabled state; `off` disables thinking.
pub fn zai_thinking_payload(level: &str) -> serde_json::Value {
    match level.trim().to_ascii_lowercase().as_str() {
        "" | "off" | "none" => serde_json::json!({"thinking": {"type": "disabled"}}),
        "high" => serde_json::json!({
            "thinking": {"type": "enabled"},
            "reasoning_effort": "high"
        }),
        "max" | "xhigh" => serde_json::json!({
            "thinking": {"type": "enabled"},
            "reasoning_effort": "max"
        }),
        _ => serde_json::json!({"thinking": {"type": "enabled"}}),
    }
}

/// Auth env vars accepted for Z.AI, from the manifest.
pub fn zai_auth_env_vars() -> &'static [&'static str] {
    manifest::manifest_auth_env_vars("zai")
}

fn base_url_is_zai_native(base_url: &str) -> bool {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return false;
    }
    match url::Url::parse(trimmed) {
        Ok(u) => u
            .host_str()
            .map(|h| {
                let host = h.to_ascii_lowercase();
                ZAI_NATIVE_HOSTS.iter().any(|native| *native == host)
            })
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Classify providers whose silent context overflow means the runtime must
/// keep prior context on consecutive turns (no state reset before the
/// provider call). Port of upstream `isSilentOverflowProneModel`
/// (openclaw#75799), current v2026.7.1 shape:
///
/// True on any of:
/// * normalized provider id `zai`
/// * a base URL whose endpoint class is `zai-native`
/// * a `z-ai/` or `openrouter/z-ai/` model-id namespace prefix
/// * a bare `glm-` model id (no namespace prefix) — covers in-house gateways
///   exposing Zhipu's GLM family directly.
///
/// Intentionally narrow: namespaced GLM ids routed through other providers
/// (`ollama/glm-*`, `opencode-go/glm-*`) are NOT included — their hosts have
/// their own overflow accounting.
pub fn keeps_prior_context(
    provider: Option<&str>,
    model_id: Option<&str>,
    base_url: Option<&str>,
) -> bool {
    if let Some(provider) = provider {
        if provider.trim().to_ascii_lowercase() == "zai" {
            return true;
        }
    }
    if let Some(base_url) = base_url {
        if base_url_is_zai_native(base_url) {
            return true;
        }
    }
    if let Some(model_id) = model_id {
        let normalized = model_id.trim().to_ascii_lowercase();
        if !normalized.is_empty()
            && (normalized.starts_with("z-ai/")
                || normalized.starts_with("openrouter/z-ai/")
                || normalized.starts_with("glm-"))
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zai_provider_id_keeps_prior_context() {
        assert!(keeps_prior_context(Some("zai"), None, None));
        assert!(keeps_prior_context(Some("  ZAI "), None, None));
    }

    #[test]
    fn zai_native_base_url_keeps_prior_context() {
        assert!(keeps_prior_context(
            None,
            None,
            Some("https://api.z.ai/api/paas/v4")
        ));
        assert!(keeps_prior_context(
            None,
            None,
            Some("https://open.bigmodel.cn/api/paas/v4")
        ));
    }

    #[test]
    fn z_ai_namespace_prefixes_keep_prior_context() {
        assert!(keeps_prior_context(None, Some("z-ai/glm-5.1"), None));
        assert!(keeps_prior_context(
            Some("openrouter"),
            Some("openrouter/z-ai/glm-5"),
            None
        ));
    }

    #[test]
    fn bare_glm_model_id_keeps_prior_context() {
        assert!(keeps_prior_context(None, Some("glm-4.7"), None));
        assert!(keeps_prior_context(Some("in-house"), Some("GLM-5"), None));
    }

    #[test]
    fn namespaced_glm_via_other_providers_does_not_keep_prior_context() {
        assert!(!keeps_prior_context(Some("ollama"), Some("ollama/glm-4.7"), None));
        assert!(!keeps_prior_context(
            None,
            Some("opencode-go/glm-5"),
            None
        ));
    }

    #[test]
    fn ordinary_providers_do_not_keep_prior_context() {
        assert!(!keeps_prior_context(
            Some("anthropic"),
            Some("claude-sonnet-4-6"),
            Some("https://api.anthropic.com")
        ));
        assert!(!keeps_prior_context(None, None, None));
        assert!(!keeps_prior_context(None, Some(""), Some("not a url")));
    }

    #[test]
    fn zai_catalog_is_manifest_driven() {
        let catalog = zai_catalog();
        assert!(!catalog.is_empty());
        assert!(catalog.iter().any(|m| m.id == "glm-5.2"));
        assert_eq!(
            catalog.len(),
            super::super::manifest::ZAI_MANIFEST_MODELS.len()
        );
    }

    #[test]
    fn zai_auth_env_vars_from_manifest() {
        assert_eq!(zai_auth_env_vars(), &["ZAI_API_KEY", "Z_AI_API_KEY"]);
    }

    // ------------------------------------------------------------------
    // v2026.6.x: overload classification + graded thinking
    // ------------------------------------------------------------------

    #[test]
    fn zhipu_overload_codes_classified_retryable() {
        assert!(is_zhipu_overload(None, r#"{"error":{"code":1302,"message":"limit"}}"#));
        assert!(is_zhipu_overload(None, "system is overloaded, retry later"));
        assert!(is_zhipu_overload(Some(429), ""));
        assert!(is_zhipu_overload(Some(503), ""));
        assert!(!is_zhipu_overload(Some(401), "invalid api key"));
        assert!(!is_zhipu_overload(None, "bad request"));
    }

    #[test]
    fn graded_thinking_maps_high_and_max_to_reasoning_effort() {
        let high = zai_thinking_payload("high");
        assert_eq!(high["thinking"]["type"], "enabled");
        assert_eq!(high["reasoning_effort"], "high");
        let max = zai_thinking_payload("max");
        assert_eq!(max["reasoning_effort"], "max");
        let medium = zai_thinking_payload("medium");
        assert_eq!(medium["thinking"]["type"], "enabled");
        assert!(medium.get("reasoning_effort").is_none());
        let off = zai_thinking_payload("off");
        assert_eq!(off["thinking"]["type"], "disabled");
    }
}
