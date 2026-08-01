//! Brave Search provider (v2026.7.1 parity).
//!
//! Ports `extensions/brave/src/brave-web-search-provider.runtime.ts`:
//! endpoint-aware base URL handling (web + LLM Context endpoints),
//! self-hosted vs strict endpoint modes, freshness/date-range filters,
//! endpoint-partitioned cache keys backed by the shared LRU cache layer, and
//! opt-in `brave.http` diagnostics.

use super::cache::{build_search_cache_key, read_cached_search_payload, write_cached_search_payload};
use super::common::{
    parse_web_search_time_filters, resolve_search_count, resolve_site_name, search_error_payload,
    today_iso_date, FreshnessProvider, DEFAULT_SEARCH_COUNT,
};
use crate::agents::tools::web_fetch::{
    hostname_resolves_only_to_private_ips, is_blocked_hostname,
};
use anyhow::Result;
use serde_json::json;
use tracing::debug;
use url::Url;

/// Default Brave API origin (upstream `DEFAULT_BRAVE_BASE_URL`).
pub const BRAVE_DEFAULT_ORIGIN: &str = "https://api.search.brave.com";
pub const BRAVE_SEARCH_ENDPOINT_PATH: &str = "/res/v1/web/search";
pub const BRAVE_LLM_CONTEXT_ENDPOINT_PATH: &str = "/res/v1/llm/context";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BraveEndpointMode {
    SelfHosted,
    Strict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BraveMode {
    Web,
    LlmContext,
}

impl BraveMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            BraveMode::Web => "web",
            BraveMode::LlmContext => "llm-context",
        }
    }
}

/// Resolve web vs llm-context mode from `tools.web.search.brave.mode`.
pub fn resolve_brave_mode(mode: Option<&str>) -> BraveMode {
    if mode == Some("llm-context") {
        BraveMode::LlmContext
    } else {
        BraveMode::Web
    }
}

/// Normalize the configured base URL into an origin+path base with no
/// trailing slash. A legacy full endpoint URL (ending in the web-search or
/// llm-context path) is reduced to its base so endpoint paths append cleanly.
pub fn resolve_brave_base_url(configured: Option<&str>) -> String {
    let raw = configured
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(BRAVE_DEFAULT_ORIGIN);
    let trimmed = raw.trim_end_matches('/');
    for endpoint in [BRAVE_SEARCH_ENDPOINT_PATH, BRAVE_LLM_CONTEXT_ENDPOINT_PATH] {
        if let Some(stripped) = trimmed.strip_suffix(endpoint) {
            let stripped = stripped.trim_end_matches('/');
            return if stripped.is_empty() {
                BRAVE_DEFAULT_ORIGIN.to_string()
            } else {
                stripped.to_string()
            };
        }
    }
    trimmed.to_string()
}

/// Build the endpoint URL by appending the endpoint path to the base path.
pub fn build_brave_endpoint_url(base_url: &str, endpoint_path: &str) -> Result<Url> {
    let mut url = Url::parse(base_url)
        .map_err(|_| anyhow::anyhow!("Brave Search base URL must be a valid http:// or https:// URL."))?;
    let base_path = url.path().trim_end_matches('/').to_string();
    url.set_path(&format!("{base_path}{endpoint_path}"));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

/// Validate the base URL and classify the endpoint trust mode.
///
/// http:// URLs must target private/loopback hosts (self-hosted proxies);
/// https:// URLs targeting only private space are also self-hosted; public
/// https:// endpoints run in strict mode.
pub async fn validate_brave_base_url(base_url: &str) -> Result<BraveEndpointMode, String> {
    let parsed = Url::parse(base_url)
        .map_err(|_| "Brave Search base URL must be a valid http:// or https:// URL.".to_string())?;
    match parsed.scheme() {
        "http" | "https" => {}
        _ => return Err("Brave Search base URL must use http:// or https://.".to_string()),
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "Brave Search base URL must be a valid http:// or https:// URL.".to_string())?;
    let targets_private =
        is_blocked_hostname(host) || hostname_resolves_only_to_private_ips(host).await;
    if parsed.scheme() == "http" {
        if targets_private {
            return Ok(BraveEndpointMode::SelfHosted);
        }
        return Err(
            "Brave Search HTTP base URL must target a trusted private or loopback host. Use https:// for public hosts."
                .to_string(),
        );
    }
    Ok(if targets_private {
        BraveEndpointMode::SelfHosted
    } else {
        BraveEndpointMode::Strict
    })
}

/// Apply shared Brave query params: query, locale filters, and the
/// freshness/date-range mapping (upstream `setBraveSearchUrlParams`).
#[derive(Debug, Default)]
pub struct BraveUrlParams<'a> {
    pub query: &'a str,
    pub country: Option<&'a str>,
    pub search_lang: Option<&'a str>,
    pub freshness: Option<&'a str>,
    pub date_after: Option<&'a str>,
    pub date_before: Option<&'a str>,
    pub allow_date_before_only: bool,
}

pub fn set_brave_search_url_params(url: &mut Url, params: &BraveUrlParams<'_>) {
    let mut pairs = url.query_pairs_mut();
    pairs.append_pair("q", params.query);
    if let Some(country) = params.country {
        pairs.append_pair("country", country);
    }
    if let Some(lang) = params.search_lang {
        pairs.append_pair("search_lang", lang);
    }
    if let Some(freshness) = params.freshness {
        pairs.append_pair("freshness", freshness);
    } else if let (Some(after), Some(before)) = (params.date_after, params.date_before) {
        pairs.append_pair("freshness", &format!("{after}to{before}"));
    } else if let Some(after) = params.date_after {
        pairs.append_pair("freshness", &format!("{after}to{}", today_iso_date()));
    } else if params.allow_date_before_only {
        if let Some(before) = params.date_before {
            pairs.append_pair("freshness", &format!("1970-01-01to{before}"));
        }
    }
}

/// Everything `execute_brave_search` needs, resolved by the tool layer.
pub struct BraveSearchRequest<'a> {
    pub query: &'a str,
    pub count: Option<u64>,
    pub api_key: &'a str,
    pub base_url: Option<&'a str>,
    pub mode: Option<&'a str>,
    pub freshness: Option<&'a str>,
    pub date_after: Option<&'a str>,
    pub date_before: Option<&'a str>,
    pub country: Option<&'a str>,
    pub search_lang: Option<&'a str>,
    pub ui_lang: Option<&'a str>,
    pub timeout_seconds: u64,
    pub cache_ttl_ms: u64,
    pub http_diag: bool,
}

fn log_brave_http(enabled: bool, event: &str, detail: &str) {
    if enabled {
        debug!(target: "brave.http", "brave http {}: {}", event, detail);
    }
}

/// Execute one Brave Search request in web or llm-context mode.
///
/// Returns a payload object; provider-visible failures come back as
/// `{error, message, docs}` payloads matching upstream behavior.
pub async fn execute_brave_search(req: BraveSearchRequest<'_>) -> Result<serde_json::Value> {
    if req.api_key.is_empty() {
        return Ok(json!({
            "error": "missing_brave_api_key",
            "message": "web_search (brave) needs a Brave Search API key. Set BRAVE_API_KEY in the Gateway environment or configure tools.web.search.apiKey.",
            "docs": super::common::WEB_TOOLS_DOCS_URL,
        }));
    }

    let brave_mode = resolve_brave_mode(req.mode);
    let base_url = resolve_brave_base_url(req.base_url);
    let endpoint_mode = match validate_brave_base_url(&base_url).await {
        Ok(mode) => mode,
        Err(message) => return Ok(search_error_payload("invalid_base_url", &message)),
    };

    if req.ui_lang.is_some() && brave_mode == BraveMode::LlmContext {
        return Ok(search_error_payload(
            "unsupported_ui_lang",
            "ui_lang is not supported by Brave llm-context mode. Remove ui_lang or use Brave web mode for locale-based UI hints.",
        ));
    }

    let filters = match parse_web_search_time_filters(
        req.freshness,
        req.date_after,
        req.date_before,
        FreshnessProvider::Brave,
        "freshness must be day, week, month, or year.",
    ) {
        Ok(f) => f,
        Err(payload) => return Ok(payload),
    };

    let (freshness, date_after, date_before) =
        (filters.freshness, filters.date_after, filters.date_before);

    if brave_mode == BraveMode::LlmContext {
        let today = today_iso_date();
        if let Some(after) = &date_after {
            if date_before.is_none() && after.as_str() > today.as_str() {
                return Ok(search_error_payload(
                    "invalid_date_range",
                    "date_after cannot be in the future for Brave llm-context mode.",
                ));
            }
        }
        if date_before.is_some() && date_after.is_none() {
            return Ok(search_error_payload(
                "unsupported_date_filter",
                "Brave llm-context mode requires date_after when date_before is set. Use a bounded date range or freshness.",
            ));
        }
    }

    let resolved_count = resolve_search_count(req.count, DEFAULT_SEARCH_COUNT);
    let llm_context_date_end = if brave_mode == BraveMode::LlmContext && date_after.is_some() {
        Some(date_before.clone().unwrap_or_else(today_iso_date))
    } else {
        date_before.clone()
    };

    let count_str = resolved_count.to_string();
    let cache_key = match brave_mode {
        BraveMode::LlmContext => build_search_cache_key(&[
            Some("brave"),
            Some(brave_mode.as_str()),
            Some(&base_url),
            Some(req.query),
            req.country,
            req.search_lang,
            freshness.as_deref(),
            date_after.as_deref(),
            llm_context_date_end.as_deref(),
        ]),
        BraveMode::Web => build_search_cache_key(&[
            Some("brave"),
            Some(brave_mode.as_str()),
            Some(&base_url),
            Some(req.query),
            Some(&count_str),
            req.country,
            req.search_lang,
            req.ui_lang,
            freshness.as_deref(),
            date_after.as_deref(),
            date_before.as_deref(),
        ]),
    };

    if let Some(cached) = read_cached_search_payload(&cache_key) {
        log_brave_http(req.http_diag, "cache hit", &format!("mode={} key={}", brave_mode.as_str(), cache_key));
        return Ok(cached);
    }
    log_brave_http(req.http_diag, "cache miss", &format!("mode={} key={}", brave_mode.as_str(), cache_key));

    let endpoint_path = match brave_mode {
        BraveMode::Web => BRAVE_SEARCH_ENDPOINT_PATH,
        BraveMode::LlmContext => BRAVE_LLM_CONTEXT_ENDPOINT_PATH,
    };
    let mut endpoint = build_brave_endpoint_url(&base_url, endpoint_path)?;
    let url_params = BraveUrlParams {
        query: req.query,
        country: req.country,
        search_lang: req.search_lang,
        freshness: freshness.as_deref(),
        date_after: date_after.as_deref(),
        date_before: date_before.as_deref(),
        allow_date_before_only: brave_mode == BraveMode::Web,
    };
    set_brave_search_url_params(&mut endpoint, &url_params);
    if brave_mode == BraveMode::Web {
        endpoint
            .query_pairs_mut()
            .append_pair("count", &count_str);
        if let Some(ui_lang) = req.ui_lang {
            endpoint.query_pairs_mut().append_pair("ui_lang", ui_lang);
        }
    }

    // Endpoint mode currently gates only diagnostics phrasing in the Rust
    // port; both modes issue the request directly (the SSRF-sensitive branch
    // is base-URL validation above).
    let _ = endpoint_mode;

    log_brave_http(
        req.http_diag,
        "request",
        &format!("mode={} url={}", brave_mode.as_str(), endpoint),
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(req.timeout_seconds))
        .build()?;
    let started = std::time::Instant::now();
    let response = client
        .get(endpoint.clone())
        .header("Accept", "application/json")
        .header("Accept-Encoding", "gzip")
        .header("X-Subscription-Token", req.api_key)
        .send()
        .await?;

    log_brave_http(
        req.http_diag,
        "response",
        &format!(
            "mode={} status={} durationMs={}",
            brave_mode.as_str(),
            response.status(),
            started.elapsed().as_millis()
        ),
    );

    if !response.status().is_success() {
        let label = match brave_mode {
            BraveMode::Web => "Brave Search API error",
            BraveMode::LlmContext => "Brave LLM Context API error",
        };
        return Ok(search_error_payload(
            "brave_api_error",
            &format!("{} ({})", label, response.status()),
        ));
    }

    let data: serde_json::Value = response.json().await?;
    let took_ms = started.elapsed().as_millis() as u64;

    let payload = match brave_mode {
        BraveMode::LlmContext => {
            let results = map_brave_llm_context_results(&data);
            json!({
                "query": req.query,
                "provider": "brave",
                "mode": "llm-context",
                "count": results.len(),
                "tookMs": took_ms,
                "results": results,
                "sources": data.get("sources").cloned().unwrap_or(serde_json::Value::Null),
            })
        }
        BraveMode::Web => {
            let results = map_brave_web_results(&data, resolved_count as usize);
            json!({
                "query": req.query,
                "provider": "brave",
                "count": results.len(),
                "tookMs": took_ms,
                "results": results,
            })
        }
    };

    write_cached_search_payload(&cache_key, &payload, req.cache_ttl_ms);
    log_brave_http(
        req.http_diag,
        "cache write",
        &format!(
            "mode={} key={} ttlMs={}",
            brave_mode.as_str(),
            cache_key,
            req.cache_ttl_ms
        ),
    );
    Ok(payload)
}

/// Map Brave web-search results into result rows.
pub fn map_brave_web_results(data: &serde_json::Value, max: usize) -> Vec<serde_json::Value> {
    data["web"]["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .take(max)
                .map(|entry| {
                    let url = entry["url"].as_str().unwrap_or("");
                    json!({
                        "title": entry["title"].as_str().unwrap_or(""),
                        "url": url,
                        "description": entry["description"].as_str().unwrap_or(""),
                        "published": entry.get("age").and_then(|v| v.as_str()),
                        "siteName": resolve_site_name(url),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Map Brave LLM Context API grounding results into web-search result rows
/// (upstream `mapBraveLlmContextResults`).
pub fn map_brave_llm_context_results(data: &serde_json::Value) -> Vec<serde_json::Value> {
    data["grounding"]["generic"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|entry| {
                    let url = entry["url"].as_str().unwrap_or("");
                    let snippets: Vec<&str> = entry["snippets"]
                        .as_array()
                        .map(|s| {
                            s.iter()
                                .filter_map(|v| v.as_str())
                                .filter(|v| !v.is_empty())
                                .collect()
                        })
                        .unwrap_or_default();
                    json!({
                        "title": entry["title"].as_str().unwrap_or(""),
                        "url": url,
                        "snippets": snippets,
                        "siteName": resolve_site_name(url),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_defaults_to_origin() {
        assert_eq!(resolve_brave_base_url(None), BRAVE_DEFAULT_ORIGIN);
        assert_eq!(resolve_brave_base_url(Some("  ")), BRAVE_DEFAULT_ORIGIN);
    }

    #[test]
    fn base_url_strips_legacy_full_endpoint() {
        // The pre-v2026.7.1 Rust config default stored the full web-search
        // endpoint; reduce it so endpoint paths append cleanly.
        assert_eq!(
            resolve_brave_base_url(Some("https://api.search.brave.com/res/v1/web/search")),
            BRAVE_DEFAULT_ORIGIN
        );
        assert_eq!(
            resolve_brave_base_url(Some("https://proxy.corp/res/v1/llm/context/")),
            "https://proxy.corp"
        );
    }

    #[test]
    fn endpoint_url_appends_path_to_base_path() {
        let url =
            build_brave_endpoint_url("https://proxy.corp/brave", BRAVE_SEARCH_ENDPOINT_PATH)
                .unwrap();
        assert_eq!(url.as_str(), "https://proxy.corp/brave/res/v1/web/search");
        let url = build_brave_endpoint_url(BRAVE_DEFAULT_ORIGIN, BRAVE_LLM_CONTEXT_ENDPOINT_PATH)
            .unwrap();
        assert_eq!(url.as_str(), "https://api.search.brave.com/res/v1/llm/context");
    }

    #[test]
    fn freshness_wins_over_date_range_in_url() {
        let mut url = Url::parse("https://x.example/search").unwrap();
        set_brave_search_url_params(
            &mut url,
            &BraveUrlParams {
                query: "q",
                freshness: Some("pw"),
                date_after: Some("2026-01-01"),
                ..Default::default()
            },
        );
        let q = url.query().unwrap();
        assert!(q.contains("freshness=pw"), "query: {q}");
    }

    #[test]
    fn bounded_date_range_maps_to_freshness_range() {
        let mut url = Url::parse("https://x.example/search").unwrap();
        set_brave_search_url_params(
            &mut url,
            &BraveUrlParams {
                query: "q",
                date_after: Some("2026-01-01"),
                date_before: Some("2026-02-01"),
                ..Default::default()
            },
        );
        assert!(url.query().unwrap().contains("freshness=2026-01-01to2026-02-01"));
    }

    #[test]
    fn date_after_only_extends_to_today() {
        let mut url = Url::parse("https://x.example/search").unwrap();
        set_brave_search_url_params(
            &mut url,
            &BraveUrlParams {
                query: "q",
                date_after: Some("2026-01-01"),
                ..Default::default()
            },
        );
        let expected = format!("freshness=2026-01-01to{}", today_iso_date());
        assert!(url.query().unwrap().contains(&expected));
    }

    #[test]
    fn date_before_only_requires_web_mode_allowance() {
        let mut url = Url::parse("https://x.example/search").unwrap();
        set_brave_search_url_params(
            &mut url,
            &BraveUrlParams {
                query: "q",
                date_before: Some("2026-02-01"),
                allow_date_before_only: false,
                ..Default::default()
            },
        );
        assert!(!url.query().unwrap().contains("freshness"));

        let mut url = Url::parse("https://x.example/search").unwrap();
        set_brave_search_url_params(
            &mut url,
            &BraveUrlParams {
                query: "q",
                date_before: Some("2026-02-01"),
                allow_date_before_only: true,
                ..Default::default()
            },
        );
        assert!(url.query().unwrap().contains("freshness=1970-01-01to2026-02-01"));
    }

    #[test]
    fn llm_context_results_map_grounding_generic() {
        let data = serde_json::json!({
            "grounding": {
                "generic": [
                    {
                        "url": "https://www.example.com/a",
                        "title": "A",
                        "snippets": ["s1", "", "s2"]
                    }
                ]
            },
            "sources": [{"url": "https://example.com", "date": "2026-01-01"}]
        });
        let mapped = map_brave_llm_context_results(&data);
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0]["title"], "A");
        assert_eq!(mapped[0]["snippets"], serde_json::json!(["s1", "s2"]));
        assert_eq!(mapped[0]["siteName"], "example.com");
    }

    #[tokio::test]
    async fn validate_base_url_rejects_public_http() {
        let err = validate_brave_base_url("http://api.search.brave.com").await.unwrap_err();
        assert!(err.contains("https://"), "err: {err}");
    }

    #[tokio::test]
    async fn validate_base_url_accepts_loopback_http_as_self_hosted() {
        assert_eq!(
            validate_brave_base_url("http://127.0.0.1:8080").await.unwrap(),
            BraveEndpointMode::SelfHosted
        );
        assert_eq!(
            validate_brave_base_url("http://localhost:3000").await.unwrap(),
            BraveEndpointMode::SelfHosted
        );
    }

    #[tokio::test]
    async fn validate_base_url_classifies_public_https_as_strict() {
        assert_eq!(
            validate_brave_base_url(BRAVE_DEFAULT_ORIGIN).await.unwrap(),
            BraveEndpointMode::Strict
        );
    }

    #[tokio::test]
    async fn validate_base_url_rejects_non_http_scheme() {
        assert!(validate_brave_base_url("ftp://example.com").await.is_err());
    }

    #[tokio::test]
    async fn llm_context_rejects_date_before_only() {
        let payload = execute_brave_search(BraveSearchRequest {
            query: "q",
            count: None,
            api_key: "k",
            base_url: None,
            mode: Some("llm-context"),
            freshness: None,
            date_after: None,
            date_before: Some("2026-01-01"),
            country: None,
            search_lang: None,
            ui_lang: None,
            timeout_seconds: 5,
            cache_ttl_ms: 0,
            http_diag: false,
        })
        .await
        .unwrap();
        assert_eq!(payload["error"], "unsupported_date_filter");
    }

    #[tokio::test]
    async fn llm_context_rejects_ui_lang() {
        let payload = execute_brave_search(BraveSearchRequest {
            query: "q",
            count: None,
            api_key: "k",
            base_url: None,
            mode: Some("llm-context"),
            freshness: None,
            date_after: None,
            date_before: None,
            country: None,
            search_lang: None,
            ui_lang: Some("en-US"),
            timeout_seconds: 5,
            cache_ttl_ms: 0,
            http_diag: false,
        })
        .await
        .unwrap();
        assert_eq!(payload["error"], "unsupported_ui_lang");
    }

    #[tokio::test]
    async fn llm_context_rejects_future_date_after() {
        let payload = execute_brave_search(BraveSearchRequest {
            query: "q",
            count: None,
            api_key: "k",
            base_url: None,
            mode: Some("llm-context"),
            freshness: None,
            date_after: Some("2999-01-01"),
            date_before: None,
            country: None,
            search_lang: None,
            ui_lang: None,
            timeout_seconds: 5,
            cache_ttl_ms: 0,
            http_diag: false,
        })
        .await
        .unwrap();
        assert_eq!(payload["error"], "invalid_date_range");
    }

    #[tokio::test]
    async fn missing_api_key_returns_structured_payload() {
        let payload = execute_brave_search(BraveSearchRequest {
            query: "q",
            count: None,
            api_key: "",
            base_url: None,
            mode: None,
            freshness: None,
            date_after: None,
            date_before: None,
            country: None,
            search_lang: None,
            ui_lang: None,
            timeout_seconds: 5,
            cache_ttl_ms: 0,
            http_diag: false,
        })
        .await
        .unwrap();
        assert_eq!(payload["error"], "missing_brave_api_key");
    }

    #[tokio::test]
    async fn web_mode_hits_web_endpoint_and_parses_results() {
        use wiremock::matchers::{header, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/res/v1/web/search"))
            .and(query_param("q", "rustlang"))
            .and(header("X-Subscription-Token", "key-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "web": {"results": [{
                    "title": "Rust",
                    "url": "https://www.rust-lang.org",
                    "description": "lang",
                    "age": "3 days ago"
                }]}
            })))
            .mount(&server)
            .await;

        // 127.0.0.1 base → selfHosted mode; cache disabled via ttl 0.
        let payload = execute_brave_search(BraveSearchRequest {
            query: "rustlang",
            count: Some(5),
            api_key: "key-1",
            base_url: Some(&server.uri()),
            mode: None,
            freshness: None,
            date_after: None,
            date_before: None,
            country: None,
            search_lang: None,
            ui_lang: None,
            timeout_seconds: 5,
            cache_ttl_ms: 0,
            http_diag: false,
        })
        .await
        .unwrap();
        assert_eq!(payload["provider"], "brave");
        assert_eq!(payload["count"], 1);
        assert_eq!(payload["results"][0]["siteName"], "rust-lang.org");
        assert_eq!(payload["results"][0]["published"], "3 days ago");
    }

    #[tokio::test]
    async fn llm_context_mode_hits_llm_endpoint() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/res/v1/llm/context"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "grounding": {"generic": [{
                    "url": "https://docs.example.com/x",
                    "title": "X",
                    "snippets": ["snippet"]
                }]},
                "sources": [{"url": "https://docs.example.com"}]
            })))
            .mount(&server)
            .await;

        let payload = execute_brave_search(BraveSearchRequest {
            query: "docs",
            count: None,
            api_key: "key-2",
            base_url: Some(&server.uri()),
            mode: Some("llm-context"),
            freshness: None,
            date_after: None,
            date_before: None,
            country: None,
            search_lang: None,
            ui_lang: None,
            timeout_seconds: 5,
            cache_ttl_ms: 0,
            http_diag: false,
        })
        .await
        .unwrap();
        assert_eq!(payload["mode"], "llm-context");
        assert_eq!(payload["results"][0]["snippets"][0], "snippet");
        assert!(payload["sources"].is_array());
    }

    #[tokio::test]
    async fn cache_serves_second_identical_request() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/res/v1/web/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "web": {"results": []}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let make = |query: &'static str, ttl: u64, base: String| async move {
            execute_brave_search(BraveSearchRequest {
                query,
                count: None,
                api_key: "key-3",
                base_url: Some(&base),
                mode: None,
                freshness: None,
                date_after: None,
                date_before: None,
                country: None,
                search_lang: None,
                ui_lang: None,
                timeout_seconds: 5,
                cache_ttl_ms: ttl,
                http_diag: false,
            })
            .await
            .unwrap()
        };

        let first = make("cache-test-query", 60_000, server.uri()).await;
        assert!(first.get("cached").is_none());
        let second = make("cache-test-query", 60_000, server.uri()).await;
        assert_eq!(second["cached"], true, "second call must be served from cache");
    }
}
