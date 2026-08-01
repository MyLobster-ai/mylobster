//! Web-fetch provider resolution infrastructure (v2026.7.1 parity).
//!
//! Ports the runtime provider-selection behavior of upstream
//! `src/web-fetch/runtime.ts`: sandboxed fetches only ever use the bundled
//! fetch pipeline, while non-sandboxed fetches may resolve an external
//! `webFetchProviders` entry (explicitly configured or auto-detected from
//! available credentials).

use crate::config::Config;

/// External web-fetch providers known to the Rust port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebFetchProviderId {
    /// Firecrawl hosted or self-hosted scraping (`tools.web.fetch.firecrawl`).
    Firecrawl,
}

impl WebFetchProviderId {
    pub fn as_str(&self) -> &'static str {
        match self {
            WebFetchProviderId::Firecrawl => "firecrawl",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "firecrawl" => Some(WebFetchProviderId::Firecrawl),
            _ => None,
        }
    }
}

/// Whether web_fetch is enabled at all for this config.
pub fn resolve_web_fetch_enabled(config: &Config) -> bool {
    config
        .tools
        .web
        .fetch
        .as_ref()
        .and_then(|f| f.enabled)
        .unwrap_or(true)
}

fn provider_has_credential(
    provider: WebFetchProviderId,
    config: &Config,
    env: &impl Fn(&str) -> Option<String>,
) -> bool {
    match provider {
        WebFetchProviderId::Firecrawl => {
            let firecrawl = config.tools.web.fetch.as_ref().and_then(|f| f.firecrawl.as_ref());
            if firecrawl.and_then(|f| f.enabled) == Some(false) {
                return false;
            }
            let configured = firecrawl
                .and_then(|f| f.api_key.as_deref())
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .is_some();
            configured
                || env("FIRECRAWL_API_KEY")
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false)
        }
    }
}

/// Resolve the external web-fetch provider for a fetch.
///
/// Sandboxed fetches never resolve external providers — they stay on the
/// bundled sandbox-safe pipeline. Non-sandboxed fetches use the explicitly
/// configured `tools.web.fetch.provider` when its credentials are usable,
/// otherwise auto-detect from available credentials. Returns `None` when the
/// bundled fetcher should be used.
pub fn resolve_external_web_fetch_provider(
    config: &Config,
    sandboxed: bool,
) -> Option<WebFetchProviderId> {
    resolve_external_web_fetch_provider_with_env(config, sandboxed, &|var| std::env::var(var).ok())
}

/// Env-injectable variant for tests.
pub fn resolve_external_web_fetch_provider_with_env(
    config: &Config,
    sandboxed: bool,
    env: &impl Fn(&str) -> Option<String>,
) -> Option<WebFetchProviderId> {
    if sandboxed || !resolve_web_fetch_enabled(config) {
        return None;
    }
    let fetch = config.tools.web.fetch.as_ref();

    // Explicit provider selection first.
    if let Some(explicit) = fetch
        .and_then(|f| f.provider.as_deref())
        .and_then(WebFetchProviderId::parse)
    {
        if provider_has_credential(explicit, config, env) {
            return Some(explicit);
        }
        // Invalid/uncredentialed explicit selection falls through to
        // auto-detection, mirroring upstream resolveWebFetchProviderId.
    }

    // Auto-detect from available credentials.
    [WebFetchProviderId::Firecrawl]
        .into_iter()
        .find(|provider| provider_has_credential(*provider, config, env))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FirecrawlConfig, WebFetchConfig};

    fn config_with_fetch(fetch: WebFetchConfig) -> Config {
        let mut config = Config::default();
        config.tools.web.fetch = Some(fetch);
        config
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn sandboxed_fetches_never_use_external_providers() {
        let config = config_with_fetch(WebFetchConfig {
            provider: Some("firecrawl".to_string()),
            firecrawl: Some(FirecrawlConfig {
                api_key: Some("fc-key".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(
            resolve_external_web_fetch_provider_with_env(&config, true, &no_env),
            None
        );
        assert_eq!(
            resolve_external_web_fetch_provider_with_env(&config, false, &no_env),
            Some(WebFetchProviderId::Firecrawl)
        );
    }

    #[test]
    fn explicit_provider_requires_credentials() {
        let config = config_with_fetch(WebFetchConfig {
            provider: Some("firecrawl".to_string()),
            ..Default::default()
        });
        assert_eq!(
            resolve_external_web_fetch_provider_with_env(&config, false, &no_env),
            None
        );
        let env = |var: &str| (var == "FIRECRAWL_API_KEY").then(|| "fc-env".to_string());
        assert_eq!(
            resolve_external_web_fetch_provider_with_env(&config, false, &env),
            Some(WebFetchProviderId::Firecrawl)
        );
    }

    #[test]
    fn auto_detection_from_credentials_without_explicit_provider() {
        let config = config_with_fetch(WebFetchConfig {
            firecrawl: Some(FirecrawlConfig {
                api_key: Some("fc-key".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(
            resolve_external_web_fetch_provider_with_env(&config, false, &no_env),
            Some(WebFetchProviderId::Firecrawl)
        );
    }

    #[test]
    fn disabled_fetch_or_disabled_provider_yields_none() {
        let config = config_with_fetch(WebFetchConfig {
            enabled: Some(false),
            firecrawl: Some(FirecrawlConfig {
                api_key: Some("fc-key".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(
            resolve_external_web_fetch_provider_with_env(&config, false, &no_env),
            None
        );

        let config = config_with_fetch(WebFetchConfig {
            firecrawl: Some(FirecrawlConfig {
                enabled: Some(false),
                api_key: Some("fc-key".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(
            resolve_external_web_fetch_provider_with_env(&config, false, &no_env),
            None
        );
    }

    #[test]
    fn unknown_explicit_provider_falls_back_to_auto_detection() {
        let config = config_with_fetch(WebFetchConfig {
            provider: Some("bogus".to_string()),
            firecrawl: Some(FirecrawlConfig {
                api_key: Some("fc-key".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(
            resolve_external_web_fetch_provider_with_env(&config, false, &no_env),
            Some(WebFetchProviderId::Firecrawl)
        );
    }

    #[test]
    fn no_credentials_means_bundled_fetcher() {
        let config = Config::default();
        assert_eq!(
            resolve_external_web_fetch_provider_with_env(&config, false, &no_env),
            None
        );
    }
}
