//! Parallel bundled web-search provider (v2026.7.1 parity).
//!
//! Ports `extensions/parallel/src/parallel-web-search-provider.runtime.ts` +
//! `parallel-search-normalize.ts`: the `api.parallel.ai/v1/search` REST
//! transport, query/objective normalization with API caps, endpoint-
//! partitioned cache keys (NUL-delimited query arrays), and the
//! generated-session-id cache strip.

use super::cache::{build_search_cache_key, read_cached_search_payload, write_cached_search_payload};
use super::common::{resolve_site_name, DEFAULT_SEARCH_COUNT};
use anyhow::Result;
use serde_json::json;

pub const PARALLEL_BASE_URL: &str = "https://api.parallel.ai";
pub const PARALLEL_SEARCH_PATHNAME: &str = "/v1/search";
/// Parallel accepts up to 40 results per request (internal bound; the
/// model-facing schema declares its own copy).
pub const PARALLEL_MAX_SEARCH_COUNT: u64 = 40;
/// Parallel v1 Search caps each search_queries entry at 200 chars, the
/// objective at 5000, and accepts up to 5 queries.
pub const PARALLEL_MAX_SEARCH_QUERY_CHARS: usize = 200;
pub const PARALLEL_MAX_OBJECTIVE_CHARS: usize = 5000;
pub const PARALLEL_MAX_SEARCH_QUERIES: usize = 5;

fn truncate_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

/// Clamp the requested count into Parallel's accepted range.
pub fn resolve_parallel_search_count(value: u64) -> u64 {
    value.clamp(1, PARALLEL_MAX_SEARCH_COUNT)
}

/// Trim + cap the objective to the API limit.
pub fn normalize_parallel_objective(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_chars(trimmed, PARALLEL_MAX_OBJECTIVE_CHARS))
}

/// Trim, drop empties/duplicates, cap entry length to 200 chars and the list
/// to 5 entries so malformed model calls do not 422 the request.
pub fn normalize_parallel_search_queries(value: Option<&serde_json::Value>) -> Vec<String> {
    let Some(arr) = value.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for entry in arr {
        let Some(raw) = entry.as_str() else { continue };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let capped = truncate_chars(trimmed, PARALLEL_MAX_SEARCH_QUERY_CHARS);
        if !seen.insert(capped.clone()) {
            continue;
        }
        out.push(capped);
        if out.len() == PARALLEL_MAX_SEARCH_QUERIES {
            break;
        }
    }
    out
}

fn invalid_base_url_payload(value: &str) -> serde_json::Value {
    json!({
        "error": "invalid_base_url",
        "message": format!("webSearch.baseUrl must be a valid http(s) URL. Got: {value}"),
        "docs": "https://docs.openclaw.ai/tools/parallel-search",
    })
}

/// Resolve the search endpoint from an optional base-URL override; the
/// `/v1/search` pathname is appended when missing.
pub fn resolve_parallel_search_endpoint(
    base_url: Option<&str>,
) -> Result<String, serde_json::Value> {
    let configured = match base_url.map(str::trim).filter(|s| !s.is_empty()) {
        Some(v) => v,
        None => return Ok(format!("{PARALLEL_BASE_URL}{PARALLEL_SEARCH_PATHNAME}")),
    };
    let lower = configured.to_ascii_lowercase();
    let has_scheme = configured.contains("://");
    let is_http = lower.starts_with("http://") || lower.starts_with("https://");
    if has_scheme && !is_http {
        return Err(invalid_base_url_payload(configured));
    }
    let candidate = if is_http {
        configured.to_string()
    } else {
        format!("https://{configured}")
    };
    let mut parsed =
        url::Url::parse(&candidate).map_err(|_| invalid_base_url_payload(configured))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(invalid_base_url_payload(configured));
    }
    let pathname = parsed.path().trim_end_matches('/').to_string();
    let final_path = if pathname.ends_with(PARALLEL_SEARCH_PATHNAME) {
        pathname
    } else {
        format!("{pathname}{PARALLEL_SEARCH_PATHNAME}")
    };
    parsed.set_path(&final_path);
    parsed.set_fragment(None);
    Ok(parsed.to_string())
}

/// Cache key partitioned by transport endpoint, filters, session, and client
/// model. Query arrays join with a NUL delimiter so `["ab","c"]` and
/// `["a","bc"]` never collide.
pub fn build_parallel_cache_key(params: &ParallelCacheKeyParams<'_>) -> String {
    let queries = params.search_queries.join("\u{0}");
    let count_str = params.count.to_string();
    build_search_cache_key(&[
        Some("parallel"),
        Some(params.endpoint),
        params.objective,
        Some(&queries),
        Some(&count_str),
        params.session_id,
        params.client_model,
    ])
}

pub struct ParallelCacheKeyParams<'a> {
    pub endpoint: &'a str,
    pub objective: Option<&'a str>,
    pub search_queries: &'a [String],
    pub count: u64,
    pub session_id: Option<&'a str>,
    pub client_model: Option<&'a str>,
}

/// Map a Parallel v1 response into result rows (excerpts → description).
pub fn map_parallel_results(response: &serde_json::Value) -> Vec<serde_json::Value> {
    response["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter(|entry| entry.is_object())
                .map(|entry| {
                    let url = entry["url"].as_str().unwrap_or("");
                    let excerpts: Vec<&str> = entry["excerpts"]
                        .as_array()
                        .map(|e| e.iter().filter_map(|x| x.as_str()).collect())
                        .unwrap_or_default();
                    let mut row = json!({
                        "title": entry["title"].as_str().unwrap_or(""),
                        "url": url,
                        "description": excerpts.join("\n\n"),
                        "siteName": resolve_site_name(url),
                    });
                    if let Some(published) =
                        entry["publish_date"].as_str().filter(|p| !p.is_empty())
                    {
                        row["published"] = json!(published);
                    }
                    if !excerpts.is_empty() {
                        row["excerpts"] = json!(excerpts);
                    }
                    row
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Drop a Parallel-generated `sessionId` before caching: identical queries
/// from unrelated tasks would otherwise share that id. Caller-supplied ids
/// are already part of the cache key.
pub fn strip_parallel_generated_session_id(payload: &serde_json::Value) -> serde_json::Value {
    let mut copy = payload.clone();
    if let Some(obj) = copy.as_object_mut() {
        obj.remove("sessionId");
    }
    copy
}

pub struct ParallelSearchRequest<'a> {
    /// CLI-shaped fallback query, promoted to the lone search query when
    /// `search_queries` is absent.
    pub query: Option<&'a str>,
    pub objective: Option<&'a str>,
    pub search_queries: Option<&'a serde_json::Value>,
    pub count: Option<u64>,
    pub session_id: Option<&'a str>,
    pub client_model: Option<&'a str>,
    pub api_key: &'a str,
    pub base_url: Option<&'a str>,
    pub timeout_seconds: u64,
    pub cache_ttl_ms: u64,
}

/// Execute a Parallel v1 search.
pub async fn execute_parallel_search(req: ParallelSearchRequest<'_>) -> Result<serde_json::Value> {
    if req.api_key.is_empty() {
        return Ok(json!({
            "error": "missing_parallel_api_key",
            "message": "web_search (parallel) needs a Parallel API key. Set PARALLEL_API_KEY in the Gateway environment, or configure tools.web.search.parallel.apiKey.",
            "docs": "https://docs.openclaw.ai/tools/parallel-search",
        }));
    }
    let endpoint = match resolve_parallel_search_endpoint(req.base_url) {
        Ok(e) => e,
        Err(payload) => return Ok(payload),
    };

    // Generic `query` fallback: shared tool callers pass `{query, count}`
    // without knowing Parallel's richer `{objective, search_queries}` schema.
    let objective = normalize_parallel_objective(req.objective);
    let mut search_queries = normalize_parallel_search_queries(req.search_queries);
    if search_queries.is_empty() {
        if let Some(query) = normalize_parallel_objective(req.query) {
            search_queries =
                normalize_parallel_search_queries(Some(&json!([query])));
        }
    }
    if search_queries.is_empty() {
        return Ok(json!({
            "error": "invalid_search_queries",
            "message": "search_queries must be a non-empty array of keyword strings (max 5, max 200 chars each). See https://docs.parallel.ai/search/best-practices.",
            "docs": "https://docs.openclaw.ai/tools/parallel-search",
        }));
    }
    // Always pass max_results so Parallel matches the shared web_search
    // default of 5 instead of Parallel's own default of 10.
    let count = resolve_parallel_search_count(req.count.unwrap_or(DEFAULT_SEARCH_COUNT as u64));
    let session_id = req
        .session_id
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 1000);
    let client_model = req
        .client_model
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| truncate_chars(s, 100));

    let cache_key = build_parallel_cache_key(&ParallelCacheKeyParams {
        endpoint: &endpoint,
        objective: objective.as_deref(),
        search_queries: &search_queries,
        count,
        session_id,
        client_model: client_model.as_deref(),
    });
    if let Some(cached) = read_cached_search_payload(&cache_key) {
        return Ok(cached);
    }

    let mut body = json!({
        "search_queries": search_queries,
        "advanced_settings": {"max_results": count},
    });
    if let Some(objective) = &objective {
        body["objective"] = json!(objective);
    }
    if let Some(session_id) = session_id {
        body["session_id"] = json!(session_id);
    }
    if let Some(client_model) = &client_model {
        body["client_model"] = json!(client_model);
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(req.timeout_seconds))
        .build()?;
    let started = std::time::Instant::now();
    let response = client
        .post(&endpoint)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("x-api-key", req.api_key)
        .header("User-Agent", "mylobster-parallel (rust)")
        .json(&body)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(8 * 1024).collect();
        return Ok(super::common::search_error_payload(
            "parallel_api_error",
            &format!("Parallel API error ({status}): {detail}"),
        ));
    }
    let data: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(_) => {
            return Ok(super::common::search_error_payload(
                "parallel_api_error",
                "Parallel API returned malformed JSON",
            ))
        }
    };

    let results = map_parallel_results(&data);
    let mut payload = json!({
        "searchQueries": search_queries,
        "provider": "parallel",
        "count": results.len(),
        "tookMs": started.elapsed().as_millis() as u64,
        "results": results,
    });
    if let Some(objective) = &objective {
        payload["objective"] = json!(objective);
    }
    if let Some(search_id) = data["search_id"].as_str() {
        payload["searchId"] = json!(search_id);
    }
    if let Some(response_session) = data["session_id"].as_str() {
        payload["sessionId"] = json!(response_session);
    }
    for key in ["warnings", "usage"] {
        if let Some(arr) = data[key].as_array().filter(|a| !a.is_empty()) {
            payload[key] = json!(arr);
        }
    }

    // Generated session ids never enter the shared cache.
    let cache_payload = if session_id.is_some() {
        payload.clone()
    } else {
        strip_parallel_generated_session_id(&payload)
    };
    write_cached_search_payload(&cache_key, &cache_payload, req.cache_ttl_ms);
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_resolution() {
        assert_eq!(
            resolve_parallel_search_endpoint(None).unwrap(),
            "https://api.parallel.ai/v1/search"
        );
        assert_eq!(
            resolve_parallel_search_endpoint(Some("https://proxy.corp")).unwrap(),
            "https://proxy.corp/v1/search"
        );
        assert_eq!(
            resolve_parallel_search_endpoint(Some("proxy.corp/v1/search")).unwrap(),
            "https://proxy.corp/v1/search"
        );
        assert_eq!(
            resolve_parallel_search_endpoint(Some("ftp://proxy.corp")).unwrap_err()["error"],
            "invalid_base_url"
        );
    }

    #[test]
    fn query_normalization_caps_and_dedupes() {
        let long = "x".repeat(300);
        let value = serde_json::json!(["a", " a ", "", "b", long, "c", "d", "e", "f"]);
        let queries = normalize_parallel_search_queries(Some(&value));
        assert_eq!(queries.len(), PARALLEL_MAX_SEARCH_QUERIES);
        assert_eq!(queries[0], "a");
        assert_eq!(queries[1], "b");
        assert_eq!(queries[2].len(), PARALLEL_MAX_SEARCH_QUERY_CHARS);
        assert!(normalize_parallel_search_queries(None).is_empty());
        assert!(normalize_parallel_search_queries(Some(&serde_json::json!("nope"))).is_empty());
    }

    #[test]
    fn objective_normalization_truncates() {
        assert_eq!(normalize_parallel_objective(Some("  obj  ")).as_deref(), Some("obj"));
        assert_eq!(normalize_parallel_objective(Some("  ")), None);
        let long = "y".repeat(6000);
        assert_eq!(
            normalize_parallel_objective(Some(&long)).unwrap().len(),
            PARALLEL_MAX_OBJECTIVE_CHARS
        );
    }

    #[test]
    fn cache_keys_distinguish_query_arrays() {
        let base = |queries: &[String]| {
            build_parallel_cache_key(&ParallelCacheKeyParams {
                endpoint: "https://api.parallel.ai/v1/search",
                objective: None,
                search_queries: queries,
                count: 5,
                session_id: None,
                client_model: None,
            })
        };
        let a = base(&["ab".to_string(), "c".to_string()]);
        let b = base(&["a".to_string(), "bc".to_string()]);
        assert_ne!(a, b, "NUL delimiter must keep distinct arrays distinct");
    }

    #[test]
    fn cache_keys_partition_by_endpoint_and_session() {
        let make = |endpoint: &str, session: Option<&str>| {
            build_parallel_cache_key(&ParallelCacheKeyParams {
                endpoint,
                objective: None,
                search_queries: &["q".to_string()],
                count: 5,
                session_id: session,
                client_model: None,
            })
        };
        assert_ne!(
            make("https://api.parallel.ai/v1/search", None),
            make("https://proxy.corp/v1/search", None)
        );
        assert_ne!(
            make("https://api.parallel.ai/v1/search", None),
            make("https://api.parallel.ai/v1/search", Some("s1"))
        );
    }

    #[test]
    fn generated_session_id_stripped_from_cache_payload() {
        let payload = serde_json::json!({"provider": "parallel", "sessionId": "gen-1"});
        let stripped = strip_parallel_generated_session_id(&payload);
        assert!(stripped.get("sessionId").is_none());
        assert_eq!(stripped["provider"], "parallel");
    }

    #[test]
    fn results_map_excerpts_into_description() {
        let response = serde_json::json!({
            "results": [
                {
                    "title": "T",
                    "url": "https://www.example.com/a",
                    "publish_date": "2026-06-01",
                    "excerpts": ["e1", "e2"]
                },
                "malformed",
                {"title": "U", "url": "https://example.org"}
            ]
        });
        let mapped = map_parallel_results(&response);
        assert_eq!(mapped.len(), 2);
        assert_eq!(mapped[0]["description"], "e1\n\ne2");
        assert_eq!(mapped[0]["published"], "2026-06-01");
        assert_eq!(mapped[0]["siteName"], "example.com");
        assert_eq!(mapped[1]["description"], "");
    }

    #[tokio::test]
    async fn missing_key_and_missing_queries_return_structured_errors() {
        let payload = execute_parallel_search(ParallelSearchRequest {
            query: None,
            objective: None,
            search_queries: None,
            count: None,
            session_id: None,
            client_model: None,
            api_key: "",
            base_url: None,
            timeout_seconds: 5,
            cache_ttl_ms: 0,
        })
        .await
        .unwrap();
        assert_eq!(payload["error"], "missing_parallel_api_key");

        let payload = execute_parallel_search(ParallelSearchRequest {
            query: None,
            objective: Some("find things"),
            search_queries: None,
            count: None,
            session_id: None,
            client_model: None,
            api_key: "pk",
            base_url: None,
            timeout_seconds: 5,
            cache_ttl_ms: 0,
        })
        .await
        .unwrap();
        assert_eq!(payload["error"], "invalid_search_queries");
    }

    #[tokio::test]
    async fn search_promotes_cli_query_and_posts_expected_body() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .and(header("x-api-key", "pk-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "search_id": "srch_1",
                "session_id": "sess_gen",
                "results": [{
                    "title": "R",
                    "url": "https://r.example.com",
                    "excerpts": ["hit"]
                }]
            })))
            .mount(&server)
            .await;

        let payload = execute_parallel_search(ParallelSearchRequest {
            query: Some("parallel endpoint test"),
            objective: None,
            search_queries: None,
            count: Some(3),
            session_id: None,
            client_model: None,
            api_key: "pk-1",
            base_url: Some(&server.uri()),
            timeout_seconds: 5,
            cache_ttl_ms: 0,
        })
        .await
        .unwrap();
        assert_eq!(payload["provider"], "parallel");
        assert_eq!(payload["searchQueries"][0], "parallel endpoint test");
        assert_eq!(payload["searchId"], "srch_1");
        assert_eq!(payload["sessionId"], "sess_gen");

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&requests.first().unwrap().body).unwrap();
        assert_eq!(body["search_queries"][0], "parallel endpoint test");
        assert_eq!(body["advanced_settings"]["max_results"], 3);
        assert!(body.get("objective").is_none(), "objective never faked from query");
    }
}
