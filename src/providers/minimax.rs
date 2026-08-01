//! MiniMax provider helpers (v2026.5.2).
//!
//! Chat routing flows through the shared OpenAI/Anthropic-compatible paths
//! (see `providers/mod.rs`). This module carries the v2026.5.2 credential and
//! endpoint-derivation behavior:
//!
//! * **Credential auto-detection** — `MINIMAX_API_KEY` participates in
//!   MiniMax auto-detection, and `MINIMAX_OAUTH_TOKEN` also satisfies
//!   credentials so OAuth-authorized Token Plan setups do not need a separate
//!   key (issues #65828 / #65768).
//! * **Coding Plan usage polling derives from the configured base URL** —
//!   global setups no longer query the CN usage host (issue #65054).

/// Global (default) MiniMax OpenAI-compatible endpoint.
pub const MINIMAX_DEFAULT_BASE_URL: &str = "https://api.minimaxi.chat/v1";

/// CN MiniMax endpoint host.
pub const MINIMAX_CN_HOST: &str = "api.minimax.chat";

/// Where a MiniMax credential came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MinimaxCredential {
    ApiKey(String),
    OauthToken(String),
}

impl MinimaxCredential {
    pub fn secret(&self) -> &str {
        match self {
            MinimaxCredential::ApiKey(s) | MinimaxCredential::OauthToken(s) => s,
        }
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// Resolve MiniMax credentials from an explicit config value, then
/// `MINIMAX_API_KEY`, then `MINIMAX_OAUTH_TOKEN` (v2026.5.2 auto-detection
/// order). Returns `None` when nothing is configured.
pub fn resolve_minimax_credentials(configured: Option<&str>) -> Option<MinimaxCredential> {
    if let Some(key) = configured.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(MinimaxCredential::ApiKey(key.to_string()));
    }
    if let Some(key) = non_empty(std::env::var("MINIMAX_API_KEY").ok()) {
        return Some(MinimaxCredential::ApiKey(key));
    }
    if let Some(token) = non_empty(std::env::var("MINIMAX_OAUTH_TOKEN").ok()) {
        return Some(MinimaxCredential::OauthToken(token));
    }
    None
}

/// Derive the Coding Plan usage-polling endpoint from the configured base
/// URL, so global setups poll the same host they chat against instead of the
/// hardcoded CN usage host (v2026.5.2, issue #65054).
pub fn coding_plan_usage_url(base_url: Option<&str>) -> String {
    let base = base_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(MINIMAX_DEFAULT_BASE_URL);
    let trimmed = base.trim_end_matches('/');
    // The usage route lives beside the chat API root: strip a trailing /v1
    // segment (if any), then append the usage path.
    let root = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    format!("{}/v1/coding_plan/usage", root)
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: env-var mutation tests share process env; use unique vars per
    // test via a lock to avoid cross-test flake.
    static ENV_LOCK: once_cell::sync::Lazy<std::sync::Mutex<()>> =
        once_cell::sync::Lazy::new(|| std::sync::Mutex::new(()));

    #[test]
    fn configured_key_wins() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("MINIMAX_API_KEY", "env-key");
        let cred = resolve_minimax_credentials(Some("cfg-key")).unwrap();
        assert_eq!(cred, MinimaxCredential::ApiKey("cfg-key".to_string()));
        std::env::remove_var("MINIMAX_API_KEY");
    }

    #[test]
    fn api_key_env_detected() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("MINIMAX_API_KEY", "env-key");
        std::env::remove_var("MINIMAX_OAUTH_TOKEN");
        let cred = resolve_minimax_credentials(None).unwrap();
        assert_eq!(cred, MinimaxCredential::ApiKey("env-key".to_string()));
        std::env::remove_var("MINIMAX_API_KEY");
    }

    #[test]
    fn oauth_token_satisfies_credentials() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("MINIMAX_API_KEY");
        std::env::set_var("MINIMAX_OAUTH_TOKEN", "oauth-tok");
        let cred = resolve_minimax_credentials(None).unwrap();
        assert_eq!(cred, MinimaxCredential::OauthToken("oauth-tok".to_string()));
        assert_eq!(cred.secret(), "oauth-tok");
        std::env::remove_var("MINIMAX_OAUTH_TOKEN");
    }

    #[test]
    fn no_credentials_returns_none() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("MINIMAX_API_KEY");
        std::env::remove_var("MINIMAX_OAUTH_TOKEN");
        assert!(resolve_minimax_credentials(None).is_none());
        assert!(resolve_minimax_credentials(Some("  ")).is_none());
    }

    #[test]
    fn usage_url_derives_from_configured_base() {
        assert_eq!(
            coding_plan_usage_url(Some("https://api.minimaxi.chat/v1")),
            "https://api.minimaxi.chat/v1/coding_plan/usage"
        );
        assert_eq!(
            coding_plan_usage_url(Some("https://api.minimax.chat/v1/")),
            "https://api.minimax.chat/v1/coding_plan/usage"
        );
        assert_eq!(
            coding_plan_usage_url(Some("https://api.minimax.io")),
            "https://api.minimax.io/v1/coding_plan/usage"
        );
    }

    #[test]
    fn usage_url_defaults_to_global_host_not_cn() {
        let url = coding_plan_usage_url(None);
        assert!(url.starts_with("https://api.minimaxi.chat/"));
        assert!(!url.contains(MINIMAX_CN_HOST));
    }
}
