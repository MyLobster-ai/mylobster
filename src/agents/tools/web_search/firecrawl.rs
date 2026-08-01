//! Firecrawl search/scrape provider hardening (v2026.7.1 parity).
//!
//! Ports `extensions/firecrawl/src/firecrawl-client.ts`: scrape targets must
//! be public HTTP(S) URLs (private/loopback/metadata hosts rejected), and a
//! custom base URL must be either the official hosted endpoint or an
//! explicitly self-hosted private/internal endpoint.

use super::cache::{normalize_cache_key, read_cached_search_payload, write_cached_search_payload};
use super::common::resolve_site_name;
use crate::agents::tools::web_fetch::{
    hostname_resolves_only_to_private_ips, is_blocked_hostname, is_private_ip,
};
use anyhow::Result;
use serde_json::json;

pub const DEFAULT_FIRECRAWL_BASE_URL: &str = "https://api.firecrawl.dev";
const ALLOWED_FIRECRAWL_HOSTS: [&str; 1] = ["api.firecrawl.dev"];
pub const FIRECRAWL_SELF_HOSTED_PRIVATE_ERROR: &str =
    "Firecrawl custom baseUrl must target a private or internal self-hosted endpoint.";
pub const FIRECRAWL_HTTP_PRIVATE_ERROR: &str =
    "Firecrawl HTTP baseUrl must target a private or internal self-hosted endpoint. Use https:// for public hosts.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirecrawlEndpointMode {
    SelfHosted,
    Strict,
}

/// Reject scrape targets that could reach private/internal infrastructure.
///
/// Firecrawl fetches the URL server-side, but a self-hosted deployment sits
/// inside the operator's network — never forward private/loopback/metadata
/// or non-HTTP(S) targets.
pub fn assert_firecrawl_scrape_target_allowed(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url)
        .map_err(|_| "Invalid URL supplied to Firecrawl scrape".to_string())?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "Blocked non-HTTP(S) protocol in Firecrawl scrape URL: {other}:"
            ))
        }
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "Invalid URL supplied to Firecrawl scrape".to_string())?;
    let bare = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    let blocked = is_blocked_hostname(host)
        || bare
            .parse::<std::net::IpAddr>()
            .map(is_private_ip)
            .unwrap_or(false);
    if blocked {
        return Err(format!(
            "Blocked hostname or private/internal IP in Firecrawl scrape URL: {host}"
        ));
    }
    Ok(())
}

fn is_official_firecrawl_endpoint(url: &url::Url) -> bool {
    url.scheme() == "https"
        && url
            .host_str()
            .map(|h| ALLOWED_FIRECRAWL_HOSTS.contains(&h))
            .unwrap_or(false)
}

/// Validate the Firecrawl base URL.
///
/// The official hosted endpoint runs strict; a private/internal endpoint is
/// an explicitly-allowed self-hosted deployment; any other public host is
/// rejected (https) or told to use a private host (http).
pub async fn validate_firecrawl_base_url(base_url: &str) -> Result<FirecrawlEndpointMode, String> {
    let effective = {
        let trimmed = base_url.trim();
        if trimmed.is_empty() {
            DEFAULT_FIRECRAWL_BASE_URL
        } else {
            trimmed
        }
    };
    let parsed = url::Url::parse(effective)
        .map_err(|_| "Firecrawl baseUrl must be a valid http:// or https:// URL.".to_string())?;
    match parsed.scheme() {
        "http" | "https" => {}
        _ => return Err("Firecrawl baseUrl must use http:// or https://.".to_string()),
    }
    if is_official_firecrawl_endpoint(&parsed) {
        return Ok(FirecrawlEndpointMode::Strict);
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "Firecrawl baseUrl must be a valid http:// or https:// URL.".to_string())?;
    let is_private_target =
        is_blocked_hostname(host) || hostname_resolves_only_to_private_ips(host).await;
    if is_private_target {
        return Ok(FirecrawlEndpointMode::SelfHosted);
    }
    if parsed.scheme() == "http" {
        return Err(FIRECRAWL_HTTP_PRIVATE_ERROR.to_string());
    }
    Err(format!("{FIRECRAWL_SELF_HOSTED_PRIVATE_ERROR} Host: {host}"))
}

/// Resolve a validated endpoint URL for `/v2/search` or `/v2/scrape`.
pub async fn resolve_firecrawl_endpoint(
    base_url: &str,
    pathname: &str,
) -> Result<(String, FirecrawlEndpointMode), String> {
    let mode = validate_firecrawl_base_url(base_url).await?;
    let effective = {
        let trimmed = base_url.trim();
        if trimmed.is_empty() {
            DEFAULT_FIRECRAWL_BASE_URL
        } else {
            trimmed
        }
    };
    let mut url = url::Url::parse(effective)
        .map_err(|_| "Firecrawl baseUrl must be a valid http:// or https:// URL.".to_string())?;
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.set_path(pathname);
    Ok((url.to_string(), mode))
}

/// Extract search result rows from a Firecrawl search payload
/// (upstream `resolveSearchItems`: tolerates `data`, `results`,
/// `data.results`, `data.data`, `data.web`, `web.results` shapes).
pub fn resolve_firecrawl_search_items(payload: &serde_json::Value) -> Vec<serde_json::Value> {
    let candidates = [
        payload.get("data"),
        payload.get("results"),
        payload.pointer("/data/results"),
        payload.pointer("/data/data"),
        payload.pointer("/data/web"),
        payload.pointer("/web/results"),
    ];
    let raw_items = candidates
        .into_iter()
        .flatten()
        .find_map(|candidate| candidate.as_array());
    let Some(raw_items) = raw_items else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for entry in raw_items {
        let Some(record) = entry.as_object() else {
            continue;
        };
        let metadata = record.get("metadata").and_then(|m| m.as_object());
        let str_of = |v: Option<&serde_json::Value>| -> Option<String> {
            v.and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
        };
        let url = str_of(record.get("url"))
            .or_else(|| str_of(record.get("sourceURL")))
            .or_else(|| str_of(record.get("sourceUrl")))
            .or_else(|| str_of(metadata.and_then(|m| m.get("sourceURL"))));
        let Some(url) = url else {
            continue;
        };
        let title = str_of(record.get("title"))
            .or_else(|| str_of(metadata.and_then(|m| m.get("title"))))
            .unwrap_or_default();
        let description = str_of(record.get("description"))
            .or_else(|| str_of(record.get("snippet")))
            .or_else(|| str_of(record.get("summary")));
        let content = str_of(record.get("markdown"))
            .or_else(|| str_of(record.get("content")))
            .or_else(|| str_of(record.get("text")));
        let published = str_of(record.get("publishedDate"))
            .or_else(|| str_of(record.get("published")))
            .or_else(|| str_of(metadata.and_then(|m| m.get("publishedTime"))))
            .or_else(|| str_of(metadata.and_then(|m| m.get("publishedDate"))));
        items.push(json!({
            "title": title,
            "url": url,
            "description": description,
            "content": content,
            "published": published,
            "siteName": resolve_site_name(&url),
        }));
    }
    items
}

pub struct FirecrawlSearchRequest<'a> {
    pub query: &'a str,
    pub count: Option<u64>,
    pub api_key: &'a str,
    pub base_url: Option<&'a str>,
    pub timeout_seconds: u64,
    pub cache_ttl_ms: u64,
}

/// Run a Firecrawl `/v2/search` call.
pub async fn run_firecrawl_search(req: FirecrawlSearchRequest<'_>) -> Result<serde_json::Value> {
    if req.api_key.is_empty() {
        return Ok(super::common::search_error_payload(
            "missing_firecrawl_api_key",
            "web_search (firecrawl) needs a Firecrawl API key. Set FIRECRAWL_API_KEY in the Gateway environment, or configure tools.web.fetch.firecrawl.apiKey.",
        ));
    }
    let count = req
        .count
        .filter(|c| *c >= 1)
        .map(|c| c.min(10))
        .unwrap_or(5);
    let base_url = req.base_url.unwrap_or(DEFAULT_FIRECRAWL_BASE_URL);
    let (endpoint, _mode) = match resolve_firecrawl_endpoint(base_url, "/v2/search").await {
        Ok(v) => v,
        Err(message) => {
            return Ok(super::common::search_error_payload("invalid_base_url", &message))
        }
    };

    let cache_key = normalize_cache_key(
        &json!({
            "type": "firecrawl-search",
            "q": req.query,
            "count": count,
            "baseUrl": base_url,
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
    let response = client
        .post(&endpoint)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", req.api_key))
        .json(&json!({ "query": req.query, "limit": count }))
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(1_000).collect();
        return Ok(super::common::search_error_payload(
            "firecrawl_api_error",
            &format!("Firecrawl Search API error ({status}): {detail}"),
        ));
    }
    let data: serde_json::Value = response.json().await?;
    if data.get("success") == Some(&serde_json::Value::Bool(false)) {
        let error = data["error"]
            .as_str()
            .or_else(|| data["message"].as_str())
            .unwrap_or("unknown error");
        return Ok(super::common::search_error_payload(
            "firecrawl_api_error",
            &format!("Firecrawl Search API error: {error}"),
        ));
    }

    let items = resolve_firecrawl_search_items(&data);
    let payload = json!({
        "query": req.query,
        "provider": "firecrawl",
        "count": items.len(),
        "tookMs": started.elapsed().as_millis() as u64,
        "results": items,
    });
    write_cached_search_payload(&cache_key, &payload, req.cache_ttl_ms);
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- scrape target rejection -------------------------------------------

    #[test]
    fn scrape_target_rejects_non_http_schemes() {
        for target in ["ftp://example.com/file", "file:///etc/passwd", "gopher://x/"] {
            let err = assert_firecrawl_scrape_target_allowed(target).unwrap_err();
            assert!(err.contains("non-HTTP(S)"), "{target}: {err}");
        }
    }

    #[test]
    fn scrape_target_rejects_loopback_and_private() {
        for target in [
            "http://localhost/admin",
            "http://127.0.0.1:8080/",
            "https://10.0.0.8/internal",
            "http://192.168.1.1/",
            "http://[::1]/",
            "http://[fd00::1]/",
        ] {
            let err = assert_firecrawl_scrape_target_allowed(target).unwrap_err();
            assert!(err.contains("private/internal"), "{target}: {err}");
        }
    }

    #[test]
    fn scrape_target_rejects_metadata_endpoints() {
        assert!(assert_firecrawl_scrape_target_allowed("http://169.254.169.254/latest").is_err());
        assert!(
            assert_firecrawl_scrape_target_allowed("http://metadata.google.internal/computeMetadata")
                .is_err()
        );
    }

    #[test]
    fn scrape_target_rejects_invalid_urls() {
        assert!(assert_firecrawl_scrape_target_allowed("not a url").is_err());
    }

    #[test]
    fn scrape_target_allows_public_https() {
        assert!(assert_firecrawl_scrape_target_allowed("https://example.com/page").is_ok());
        assert!(assert_firecrawl_scrape_target_allowed("http://example.com/page").is_ok());
    }

    // ---- base URL validation ------------------------------------------------

    #[tokio::test]
    async fn official_endpoint_is_strict() {
        assert_eq!(
            validate_firecrawl_base_url(DEFAULT_FIRECRAWL_BASE_URL).await.unwrap(),
            FirecrawlEndpointMode::Strict
        );
        assert_eq!(
            validate_firecrawl_base_url("").await.unwrap(),
            FirecrawlEndpointMode::Strict
        );
    }

    #[tokio::test]
    async fn private_endpoints_are_self_hosted() {
        assert_eq!(
            validate_firecrawl_base_url("http://127.0.0.1:3002").await.unwrap(),
            FirecrawlEndpointMode::SelfHosted
        );
        assert_eq!(
            validate_firecrawl_base_url("http://localhost:3002").await.unwrap(),
            FirecrawlEndpointMode::SelfHosted
        );
        assert_eq!(
            validate_firecrawl_base_url("https://10.1.2.3").await.unwrap(),
            FirecrawlEndpointMode::SelfHosted
        );
    }

    #[tokio::test]
    async fn public_non_official_hosts_are_rejected() {
        let err = validate_firecrawl_base_url("https://evil.example.com").await.unwrap_err();
        assert!(err.contains("self-hosted"), "{err}");
        assert!(err.contains("evil.example.com"), "{err}");
        let err = validate_firecrawl_base_url("http://evil.example.com").await.unwrap_err();
        assert!(err.contains("Use https://"), "{err}");
    }

    #[tokio::test]
    async fn non_http_scheme_rejected() {
        assert!(validate_firecrawl_base_url("ftp://api.firecrawl.dev").await.is_err());
    }

    #[tokio::test]
    async fn endpoint_resolution_strips_credentials_and_query() {
        let (url, mode) = resolve_firecrawl_endpoint(
            "http://user:pass@127.0.0.1:3002/base?x=1#frag",
            "/v2/scrape",
        )
        .await
        .unwrap();
        assert_eq!(mode, FirecrawlEndpointMode::SelfHosted);
        assert_eq!(url, "http://127.0.0.1:3002/v2/scrape");
    }

    // ---- search payload parsing ---------------------------------------------

    #[test]
    fn search_items_extracted_from_alternative_shapes() {
        let payload = serde_json::json!({
            "data": {
                "web": [{
                    "url": "https://www.example.com/a",
                    "title": "A",
                    "snippet": "desc",
                }]
            }
        });
        let items = resolve_firecrawl_search_items(&payload);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["description"], "desc");
        assert_eq!(items[0]["siteName"], "example.com");

        let payload = serde_json::json!({
            "results": [{
                "metadata": {"sourceURL": "https://b.example.com", "title": "B"},
                "markdown": "content"
            }]
        });
        let items = resolve_firecrawl_search_items(&payload);
        assert_eq!(items[0]["url"], "https://b.example.com");
        assert_eq!(items[0]["title"], "B");
        assert_eq!(items[0]["content"], "content");
    }

    #[test]
    fn search_items_skip_rows_without_url() {
        let payload = serde_json::json!({"data": [{"title": "no url"}, 42]});
        assert!(resolve_firecrawl_search_items(&payload).is_empty());
    }

    #[tokio::test]
    async fn search_rejects_public_non_official_base_url() {
        let payload = run_firecrawl_search(FirecrawlSearchRequest {
            query: "q",
            count: None,
            api_key: "fc-key",
            base_url: Some("https://rogue.example.com"),
            timeout_seconds: 5,
            cache_ttl_ms: 0,
        })
        .await
        .unwrap();
        assert_eq!(payload["error"], "invalid_base_url");
    }

    #[tokio::test]
    async fn search_hits_self_hosted_endpoint() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/search"))
            .and(header("Authorization", "Bearer fc-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "data": [{
                    "url": "https://found.example.com",
                    "title": "Found",
                    "description": "hit"
                }]
            })))
            .mount(&server)
            .await;

        let payload = run_firecrawl_search(FirecrawlSearchRequest {
            query: "firecrawl search test",
            count: Some(3),
            api_key: "fc-key",
            base_url: Some(&server.uri()),
            timeout_seconds: 5,
            cache_ttl_ms: 0,
        })
        .await
        .unwrap();
        assert_eq!(payload["provider"], "firecrawl");
        assert_eq!(payload["results"][0]["title"], "Found");
    }
}
