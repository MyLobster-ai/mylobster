//! SearXNG web-search provider (v2026.7.1 parity).
//!
//! Ports `extensions/searxng/src/searxng-client.ts`: base-URL validation
//! (self-hosted vs strict), image-result `img_src` URL passthrough, and a
//! one-shot retry with the `general` category when a non-general category
//! search comes back empty.

use super::cache::{normalize_cache_key, read_cached_search_payload, write_cached_search_payload};
use super::common::{resolve_search_count, resolve_site_name, DEFAULT_SEARCH_COUNT};
use crate::agents::tools::web_fetch::{
    hostname_resolves_only_to_private_ips, is_blocked_hostname,
};
use anyhow::Result;
use serde_json::json;

pub const SEARXNG_DEFAULT_TIMEOUT_SECONDS: u64 = 20;
const MAX_RESPONSE_BYTES: usize = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearxngEndpointMode {
    SelfHosted,
    Strict,
}

/// Validate the SearXNG base URL and classify the endpoint trust mode.
pub async fn validate_searxng_base_url(base_url: &str) -> Result<SearxngEndpointMode, String> {
    let parsed = url::Url::parse(base_url)
        .map_err(|_| "SearXNG base URL must be a valid http:// or https:// URL.".to_string())?;
    match parsed.scheme() {
        "http" | "https" => {}
        _ => return Err("SearXNG base URL must use http:// or https://.".to_string()),
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "SearXNG base URL must be a valid http:// or https:// URL.".to_string())?;
    let targets_private =
        is_blocked_hostname(host) || hostname_resolves_only_to_private_ips(host).await;
    if parsed.scheme() == "http" {
        if targets_private {
            return Ok(SearxngEndpointMode::SelfHosted);
        }
        return Err(
            "SearXNG HTTP base URL must target a trusted private or loopback host. Use https:// for public hosts."
                .to_string(),
        );
    }
    Ok(if targets_private {
        SearxngEndpointMode::SelfHosted
    } else {
        SearxngEndpointMode::Strict
    })
}

/// Build the `/search` URL with query params.
pub fn build_searxng_search_url(
    base_url: &str,
    query: &str,
    categories: Option<&str>,
    language: Option<&str>,
    engines: Option<&str>,
) -> Result<String> {
    let mut url = url::Url::parse(base_url)?;
    let path = if url.path().ends_with('/') {
        format!("{}search", url.path())
    } else {
        format!("{}/search", url.path())
    };
    url.set_path(&path);
    url.set_query(None);
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("q", query);
        pairs.append_pair("format", "json");
        if let Some(categories) = categories {
            pairs.append_pair("categories", categories);
        }
        if let Some(language) = language {
            pairs.append_pair("language", language);
        }
        if let Some(engines) = engines {
            pairs.append_pair("engines", engines);
        }
    }
    Ok(url.to_string())
}

/// A normalized SearXNG result row. `img_src` passes through image-category
/// result URLs (v2026.7.1).
#[derive(Debug, Clone, PartialEq)]
pub struct SearxngResult {
    pub url: String,
    pub title: String,
    pub content: Option<String>,
    pub img_src: Option<String>,
}

/// Normalize one raw result entry; rows without string url+title are dropped.
pub fn normalize_searxng_result(value: &serde_json::Value) -> Option<SearxngResult> {
    let url = value.get("url")?.as_str()?;
    let title = value.get("title")?.as_str()?;
    Some(SearxngResult {
        url: url.to_string(),
        title: title.to_string(),
        content: value.get("content").and_then(|v| v.as_str()).map(String::from),
        img_src: value.get("img_src").and_then(|v| v.as_str()).map(String::from),
    })
}

/// Parse the JSON body into up to `count` normalized results.
pub fn parse_searxng_response_text(text: &str, count: usize) -> Result<Vec<SearxngResult>, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(text).map_err(|_| "SearXNG returned invalid JSON.".to_string())?;
    let raw_results = parsed
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut results = Vec::new();
    for raw in &raw_results {
        if let Some(result) = normalize_searxng_result(raw) {
            results.push(result);
        }
        if results.len() >= count {
            break;
        }
    }
    Ok(results)
}

/// True when an empty result set for a category search should retry once
/// with the `general` category (upstream
/// `shouldRetryEmptyCategorySearchWithGeneral`).
pub fn should_retry_empty_category_search_with_general(categories: Option<&str>) -> bool {
    let Some(categories) = categories else {
        return false;
    };
    let normalized: Vec<String> = categories
        .split(',')
        .map(|c| c.trim().to_ascii_lowercase())
        .filter(|c| !c.is_empty())
        .collect();
    !normalized.is_empty() && !normalized.iter().any(|c| c == "general")
}

pub struct SearxngSearchRequest<'a> {
    pub query: &'a str,
    pub count: Option<u64>,
    pub base_url: &'a str,
    pub categories: Option<&'a str>,
    pub language: Option<&'a str>,
    pub engines: Option<&'a str>,
    pub timeout_seconds: u64,
    pub cache_ttl_ms: u64,
}

async fn fetch_searxng_results(
    client: &reqwest::Client,
    base_url: &str,
    query: &str,
    categories: Option<&str>,
    language: Option<&str>,
    engines: Option<&str>,
    count: usize,
) -> Result<Vec<SearxngResult>> {
    let url = build_searxng_search_url(base_url, query, categories, language, engines)?;
    let response = client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await?;
    if !response.status().is_success() {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(64_000).collect();
        anyhow::bail!("SearXNG search error ({status}): {detail}");
    }
    let body = response.text().await?;
    if body.len() > MAX_RESPONSE_BYTES {
        anyhow::bail!("SearXNG response too large.");
    }
    parse_searxng_response_text(&body, count).map_err(|e| anyhow::anyhow!(e))
}

/// Run a SearXNG search with caching and the empty-category retry.
pub async fn run_searxng_search(req: SearxngSearchRequest<'_>) -> Result<serde_json::Value> {
    let count = resolve_search_count(req.count, DEFAULT_SEARCH_COUNT) as usize;
    if let Err(message) = validate_searxng_base_url(req.base_url).await {
        return Ok(super::common::search_error_payload("invalid_base_url", &message));
    }

    let cache_key = normalize_cache_key(
        &json!({
            "provider": "searxng",
            "query": req.query,
            "count": count,
            "categories": req.categories.unwrap_or(""),
            "language": req.language.unwrap_or(""),
            "baseUrl": req.base_url,
        })
        .to_string(),
    );
    if let Some(cached) = read_cached_search_payload(&cache_key) {
        return Ok(cached);
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(req.timeout_seconds))
        .build()?;
    let started = std::time::Instant::now();

    let mut results = fetch_searxng_results(
        &client,
        req.base_url,
        req.query,
        req.categories,
        req.language,
        req.engines,
        count,
    )
    .await?;

    // v2026.7.1: an empty non-general category search retries once with the
    // general category — engines for niche categories often return nothing.
    if results.is_empty() && should_retry_empty_category_search_with_general(req.categories) {
        results = fetch_searxng_results(
            &client,
            req.base_url,
            req.query,
            Some("general"),
            req.language,
            req.engines,
            count,
        )
        .await?;
    }

    let payload = json!({
        "query": req.query,
        "provider": "searxng",
        "count": results.len(),
        "tookMs": started.elapsed().as_millis() as u64,
        "results": results
            .iter()
            .map(|r| {
                json!({
                    "title": r.title,
                    "url": r.url,
                    "snippet": r.content.clone().unwrap_or_default(),
                    "siteName": resolve_site_name(&r.url),
                    "img_src": r.img_src,
                })
            })
            .collect::<Vec<_>>(),
    });
    write_cached_search_payload(&cache_key, &payload, req.cache_ttl_ms);
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_logic_targets_non_general_categories_only() {
        assert!(!should_retry_empty_category_search_with_general(None));
        assert!(!should_retry_empty_category_search_with_general(Some("")));
        assert!(!should_retry_empty_category_search_with_general(Some("general")));
        assert!(!should_retry_empty_category_search_with_general(Some("news, General")));
        assert!(should_retry_empty_category_search_with_general(Some("news")));
        assert!(should_retry_empty_category_search_with_general(Some("images, science")));
    }

    #[test]
    fn search_url_appends_categories_and_language() {
        let url = build_searxng_search_url(
            "http://localhost:8888",
            "rust",
            Some("images"),
            Some("en"),
            None,
        )
        .unwrap();
        assert!(url.starts_with("http://localhost:8888/search?"));
        assert!(url.contains("q=rust"));
        assert!(url.contains("categories=images"));
        assert!(url.contains("language=en"));
        assert!(url.contains("format=json"));
    }

    #[test]
    fn normalize_result_passes_img_src_through() {
        let raw = serde_json::json!({
            "url": "https://img.example.com/page",
            "title": "Image",
            "img_src": "https://img.example.com/full.jpg"
        });
        let result = normalize_searxng_result(&raw).unwrap();
        assert_eq!(result.img_src.as_deref(), Some("https://img.example.com/full.jpg"));
        assert_eq!(result.content, None);
    }

    #[test]
    fn normalize_result_drops_rows_without_url_or_title() {
        assert!(normalize_searxng_result(&serde_json::json!({"title": "x"})).is_none());
        assert!(normalize_searxng_result(&serde_json::json!({"url": "https://x"})).is_none());
        assert!(normalize_searxng_result(&serde_json::json!(42)).is_none());
    }

    #[test]
    fn parse_response_caps_result_count() {
        let body = serde_json::json!({
            "results": [
                {"url": "https://a", "title": "a"},
                {"url": "https://b", "title": "b"},
                {"url": "https://c", "title": "c"}
            ]
        })
        .to_string();
        let results = parse_searxng_response_text(&body, 2).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn parse_response_rejects_invalid_json() {
        assert!(parse_searxng_response_text("not-json", 5).is_err());
    }

    #[tokio::test]
    async fn base_url_validation() {
        assert_eq!(
            validate_searxng_base_url("http://localhost:8888").await.unwrap(),
            SearxngEndpointMode::SelfHosted
        );
        assert!(validate_searxng_base_url("http://searx.public.example.com").await.is_err());
        assert!(validate_searxng_base_url("gopher://x").await.is_err());
    }

    #[tokio::test]
    async fn empty_non_general_category_retries_once_with_general() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // First call: science category → empty results.
        Mock::given(method("GET"))
            .and(path("/search"))
            .and(query_param("categories", "science"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": []
            })))
            .expect(1)
            .mount(&server)
            .await;
        // Retry: general category → results.
        Mock::given(method("GET"))
            .and(path("/search"))
            .and(query_param("categories", "general"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{"url": "https://found.example.com", "title": "Found"}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let payload = run_searxng_search(SearxngSearchRequest {
            query: "searxng retry test",
            count: Some(5),
            base_url: &server.uri(),
            categories: Some("science"),
            language: None,
            engines: None,
            timeout_seconds: 5,
            cache_ttl_ms: 0,
        })
        .await
        .unwrap();
        assert_eq!(payload["count"], 1);
        assert_eq!(payload["results"][0]["title"], "Found");
    }

    #[tokio::test]
    async fn image_results_pass_img_src_to_payload() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{
                    "url": "https://img.example.com/page",
                    "title": "Image",
                    "img_src": "https://img.example.com/full.jpg"
                }]
            })))
            .mount(&server)
            .await;

        let payload = run_searxng_search(SearxngSearchRequest {
            query: "searxng img test",
            count: Some(5),
            base_url: &server.uri(),
            categories: None,
            language: None,
            engines: None,
            timeout_seconds: 5,
            cache_ttl_ms: 0,
        })
        .await
        .unwrap();
        assert_eq!(
            payload["results"][0]["img_src"],
            "https://img.example.com/full.jpg"
        );
    }
}
