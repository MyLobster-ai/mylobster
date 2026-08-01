//! MiniMax Coding Plan web-search provider (v2026.7.1 parity).
//!
//! Ports `extensions/minimax/src/minimax-web-search-provider.runtime.ts`:
//! auto-detects Coding Plan keys / OAuth tokens across the documented env
//! vars (`MINIMAX_CODE_PLAN_KEY`, `MINIMAX_CODING_API_KEY`,
//! `MINIMAX_OAUTH_TOKEN`, `MINIMAX_API_KEY`), and derives the Coding Plan
//! search endpoint (global vs CN) from explicit region config, the
//! `MINIMAX_API_HOST` override, or the configured MiniMax provider base URL.

use super::cache::{build_search_cache_key, read_cached_search_payload, write_cached_search_payload};
use super::common::{resolve_search_count, resolve_site_name, DEFAULT_SEARCH_COUNT};
use anyhow::Result;
use serde_json::json;

pub const MINIMAX_SEARCH_ENDPOINT_GLOBAL: &str = "https://api.minimax.io/v1/coding_plan/search";
pub const MINIMAX_SEARCH_ENDPOINT_CN: &str = "https://api.minimaxi.com/v1/coding_plan/search";

/// Env vars checked in order for a usable key: Coding Plan keys and OAuth
/// tokens first, generic API key last.
pub const MINIMAX_KEY_ENV_VARS: [&str; 4] = [
    "MINIMAX_CODE_PLAN_KEY",
    "MINIMAX_CODING_API_KEY",
    "MINIMAX_OAUTH_TOKEN",
    "MINIMAX_API_KEY",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiniMaxRegion {
    Global,
    Cn,
}

/// Resolve the API key: explicit config first, then env auto-detection.
pub fn resolve_minimax_api_key(
    configured: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    if let Some(key) = configured.map(str::trim).filter(|k| !k.is_empty()) {
        return Some(key.to_string());
    }
    for var in MINIMAX_KEY_ENV_VARS {
        if let Some(value) = env(var).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
            return Some(value);
        }
    }
    None
}

/// True when a URL/host string points at the CN (`minimaxi.com`) platform.
pub fn is_minimax_cn_host(value: Option<&str>) -> bool {
    let Some(trimmed) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return false;
    };
    match url::Url::parse(trimmed) {
        Ok(url) => url
            .host_str()
            .map(|h| h.ends_with("minimaxi.com"))
            .unwrap_or(false),
        Err(_) => trimmed.contains("minimaxi.com"),
    }
}

/// Resolve the region. Priority: explicit `region` config → the shared
/// `MINIMAX_API_HOST` env override → the configured MiniMax /
/// MiniMax-portal model-provider base URLs (set by CN onboarding).
pub fn resolve_minimax_region(
    configured_region: Option<&str>,
    api_host_env: Option<&str>,
    provider_base_url: Option<&str>,
    portal_base_url: Option<&str>,
) -> MiniMaxRegion {
    if let Some(region) = configured_region.map(str::trim).filter(|r| !r.is_empty()) {
        return if region == "cn" { MiniMaxRegion::Cn } else { MiniMaxRegion::Global };
    }
    if is_minimax_cn_host(api_host_env) {
        return MiniMaxRegion::Cn;
    }
    if is_minimax_cn_host(provider_base_url) || is_minimax_cn_host(portal_base_url) {
        return MiniMaxRegion::Cn;
    }
    MiniMaxRegion::Global
}

/// The Coding Plan search endpoint for a region.
pub fn resolve_minimax_endpoint(region: MiniMaxRegion) -> &'static str {
    match region {
        MiniMaxRegion::Cn => MINIMAX_SEARCH_ENDPOINT_CN,
        MiniMaxRegion::Global => MINIMAX_SEARCH_ENDPOINT_GLOBAL,
    }
}

/// Parse a MiniMax search response body into `(results, related_searches)`.
/// `base_resp.status_code != 0` is an API-level error.
pub fn parse_minimax_search_response(
    data: &serde_json::Value,
    count: usize,
) -> Result<(Vec<serde_json::Value>, Vec<String>), String> {
    if let Some(status_code) = data.pointer("/base_resp/status_code").and_then(|v| v.as_i64()) {
        if status_code != 0 {
            let msg = data
                .pointer("/base_resp/status_msg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(format!("MiniMax Search API error ({status_code}): {msg}"));
        }
    }
    let organic = data["organic"].as_array().cloned().unwrap_or_default();
    let results = organic
        .iter()
        .take(count)
        .map(|entry| {
            let url = entry["link"].as_str().unwrap_or("");
            json!({
                "title": entry["title"].as_str().unwrap_or(""),
                "url": url,
                "description": entry["snippet"].as_str().unwrap_or(""),
                "published": entry.get("date").and_then(|v| v.as_str()).filter(|s| !s.is_empty()),
                "siteName": resolve_site_name(url),
            })
        })
        .collect();
    let related = data["related_searches"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|r| r["query"].as_str())
                .filter(|q| !q.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    Ok((results, related))
}

pub struct MiniMaxSearchRequest<'a> {
    pub query: &'a str,
    pub count: Option<u64>,
    pub api_key: &'a str,
    /// Fully resolved endpoint (region already applied).
    pub endpoint: &'a str,
    pub timeout_seconds: u64,
    pub cache_ttl_ms: u64,
}

/// Execute a MiniMax Coding Plan search.
pub async fn execute_minimax_search(req: MiniMaxSearchRequest<'_>) -> Result<serde_json::Value> {
    if req.api_key.is_empty() {
        return Ok(super::common::search_error_payload(
            "missing_minimax_api_key",
            "web_search (minimax) needs a MiniMax Token Plan key or OAuth token. Set MINIMAX_CODE_PLAN_KEY, MINIMAX_CODING_API_KEY, MINIMAX_OAUTH_TOKEN, or MINIMAX_API_KEY in the Gateway environment.",
        ));
    }
    let count = resolve_search_count(req.count, DEFAULT_SEARCH_COUNT) as usize;
    let count_str = count.to_string();
    let cache_key = build_search_cache_key(&[
        Some("minimax"),
        Some(req.endpoint),
        Some(req.query),
        Some(&count_str),
    ]);
    if let Some(cached) = read_cached_search_payload(&cache_key) {
        return Ok(cached);
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(req.timeout_seconds))
        .build()?;
    let started = std::time::Instant::now();
    let response = client
        .post(req.endpoint)
        .header("Authorization", format!("Bearer {}", req.api_key))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&json!({ "q": req.query }))
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(1_000).collect();
        return Ok(super::common::search_error_payload(
            "minimax_api_error",
            &format!("MiniMax Search API error ({status}): {detail}"),
        ));
    }
    let data: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(_) => {
            return Ok(super::common::search_error_payload(
                "minimax_api_error",
                "MiniMax Search API error: malformed JSON response",
            ))
        }
    };
    let (results, related) = match parse_minimax_search_response(&data, count) {
        Ok(parsed) => parsed,
        Err(message) => {
            return Ok(super::common::search_error_payload("minimax_api_error", &message))
        }
    };

    let mut payload = json!({
        "query": req.query,
        "provider": "minimax",
        "count": results.len(),
        "tookMs": started.elapsed().as_millis() as u64,
        "results": results,
    });
    if !related.is_empty() {
        payload["relatedSearches"] = json!(related);
    }
    write_cached_search_payload(&cache_key, &payload, req.cache_ttl_ms);
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_prefers_config_then_env_order() {
        let env = |var: &str| match var {
            "MINIMAX_OAUTH_TOKEN" => Some("oauth-tok".to_string()),
            "MINIMAX_API_KEY" => Some("api-key".to_string()),
            _ => None,
        };
        assert_eq!(
            resolve_minimax_api_key(Some("cfg-key"), env).as_deref(),
            Some("cfg-key")
        );
        // Coding Plan / OAuth env vars outrank the generic API key.
        assert_eq!(resolve_minimax_api_key(None, env).as_deref(), Some("oauth-tok"));

        let env_api_only = |var: &str| {
            (var == "MINIMAX_API_KEY").then(|| "api-key".to_string())
        };
        assert_eq!(
            resolve_minimax_api_key(None, env_api_only).as_deref(),
            Some("api-key")
        );
        assert_eq!(resolve_minimax_api_key(Some("  "), |_| None), None);
    }

    #[test]
    fn code_plan_key_outranks_oauth_token() {
        let env = |var: &str| match var {
            "MINIMAX_CODE_PLAN_KEY" => Some("plan-key".to_string()),
            "MINIMAX_OAUTH_TOKEN" => Some("oauth-tok".to_string()),
            _ => None,
        };
        assert_eq!(resolve_minimax_api_key(None, env).as_deref(), Some("plan-key"));
    }

    #[test]
    fn cn_host_detection() {
        assert!(is_minimax_cn_host(Some("https://api.minimaxi.com/v1")));
        assert!(is_minimax_cn_host(Some("api.minimaxi.com")));
        assert!(!is_minimax_cn_host(Some("https://api.minimax.io/v1")));
        assert!(!is_minimax_cn_host(Some("https://minimaxi.com.evil.example")));
        assert!(!is_minimax_cn_host(None));
        assert!(!is_minimax_cn_host(Some("  ")));
    }

    #[test]
    fn region_resolution_priority() {
        // Explicit region wins.
        assert_eq!(
            resolve_minimax_region(Some("cn"), None, None, None),
            MiniMaxRegion::Cn
        );
        assert_eq!(
            resolve_minimax_region(Some("global"), Some("https://api.minimaxi.com"), None, None),
            MiniMaxRegion::Global
        );
        // MINIMAX_API_HOST inference.
        assert_eq!(
            resolve_minimax_region(None, Some("https://api.minimaxi.com"), None, None),
            MiniMaxRegion::Cn
        );
        // Provider base URL inference (CN onboarding).
        assert_eq!(
            resolve_minimax_region(None, None, Some("https://api.minimaxi.com/v1"), None),
            MiniMaxRegion::Cn
        );
        assert_eq!(
            resolve_minimax_region(None, None, None, Some("https://api.minimaxi.com/portal")),
            MiniMaxRegion::Cn
        );
        // Default: global.
        assert_eq!(resolve_minimax_region(None, None, None, None), MiniMaxRegion::Global);
    }

    #[test]
    fn endpoint_derives_from_region() {
        assert_eq!(
            resolve_minimax_endpoint(MiniMaxRegion::Global),
            MINIMAX_SEARCH_ENDPOINT_GLOBAL
        );
        assert_eq!(resolve_minimax_endpoint(MiniMaxRegion::Cn), MINIMAX_SEARCH_ENDPOINT_CN);
    }

    #[test]
    fn response_parsing_maps_organic_results() {
        let data = serde_json::json!({
            "organic": [
                {"title": "T", "link": "https://www.example.com/a", "snippet": "S", "date": "2026-05-01"},
                {"title": "U", "link": "https://example.org/b", "snippet": ""}
            ],
            "related_searches": [{"query": "next"}, {"query": ""}]
        });
        let (results, related) = parse_minimax_search_response(&data, 10).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["siteName"], "example.com");
        assert_eq!(results[0]["published"], "2026-05-01");
        assert_eq!(related, vec!["next".to_string()]);
    }

    #[test]
    fn response_parsing_surfaces_base_resp_errors() {
        let data = serde_json::json!({
            "base_resp": {"status_code": 1004, "status_msg": "invalid token"}
        });
        let err = parse_minimax_search_response(&data, 5).unwrap_err();
        assert!(err.contains("1004"));
        assert!(err.contains("invalid token"));
    }

    #[test]
    fn response_parsing_tolerates_missing_sections() {
        let (results, related) = parse_minimax_search_response(&serde_json::json!({}), 5).unwrap();
        assert!(results.is_empty());
        assert!(related.is_empty());
    }

    #[tokio::test]
    async fn search_posts_query_with_bearer_token() {
        use wiremock::matchers::{body_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/coding_plan/search"))
            .and(header("Authorization", "Bearer plan-key"))
            .and(body_json(serde_json::json!({"q": "minimax test"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "organic": [{"title": "R", "link": "https://r.example.com", "snippet": "s"}]
            })))
            .mount(&server)
            .await;

        let endpoint = format!("{}/v1/coding_plan/search", server.uri());
        let payload = execute_minimax_search(MiniMaxSearchRequest {
            query: "minimax test",
            count: Some(5),
            api_key: "plan-key",
            endpoint: &endpoint,
            timeout_seconds: 5,
            cache_ttl_ms: 0,
        })
        .await
        .unwrap();
        assert_eq!(payload["provider"], "minimax");
        assert_eq!(payload["results"][0]["title"], "R");
    }

    #[tokio::test]
    async fn missing_key_returns_structured_payload() {
        let payload = execute_minimax_search(MiniMaxSearchRequest {
            query: "q",
            count: None,
            api_key: "",
            endpoint: MINIMAX_SEARCH_ENDPOINT_GLOBAL,
            timeout_seconds: 5,
            cache_ttl_ms: 0,
        })
        .await
        .unwrap();
        assert_eq!(payload["error"], "missing_minimax_api_key");
    }
}
