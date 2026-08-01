//! Exa web-search provider (v2026.7.1 parity).
//!
//! Ports `extensions/exa/src/exa-web-search-provider.runtime.ts`:
//! `webSearch.baseUrl` override with `/search` endpoint normalization,
//! endpoint-partitioned cache keys, freshness → `startPublishedDate`
//! mapping, and optional `contents` request shaping with strict validation.

use super::cache::{build_search_cache_key, read_cached_search_payload, write_cached_search_payload};
use super::common::{parse_iso_date_range, resolve_site_name, search_error_payload};
use anyhow::Result;
use serde_json::json;

pub const EXA_SEARCH_ENDPOINT: &str = "https://api.exa.ai/search";
pub const EXA_MAX_SEARCH_COUNT: u64 = 100;
const EXA_SEARCH_TYPES: [&str; 6] = ["auto", "neural", "fast", "deep", "deep-reasoning", "instant"];
const EXA_FRESHNESS_VALUES: [&str; 4] = ["day", "week", "month", "year"];

fn invalid_base_url_payload(value: &str) -> serde_json::Value {
    json!({
        "error": "invalid_base_url",
        "message": format!("webSearch.baseUrl must be a valid http(s) URL. Got: {value}"),
        "docs": "https://docs.openclaw.ai/tools/exa-search",
    })
}

/// Resolve the Exa endpoint from an optional base-URL override.
///
/// Scheme-less values get `https://` prefixed; a trailing `/search` segment
/// is appended when missing; non-http(s) schemes are rejected.
pub fn resolve_exa_search_endpoint(base_url: Option<&str>) -> Result<String, serde_json::Value> {
    let configured = match base_url.map(str::trim).filter(|s| !s.is_empty()) {
        Some(v) => v,
        None => return Ok(EXA_SEARCH_ENDPOINT.to_string()),
    };

    let has_scheme = configured
        .split_once("://")
        .map(|(scheme, _)| {
            !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '.' || c == '-')
                && scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        })
        .unwrap_or(false);
    let is_http = configured.to_ascii_lowercase().starts_with("http://")
        || configured.to_ascii_lowercase().starts_with("https://");
    if has_scheme && !is_http {
        return Err(invalid_base_url_payload(configured));
    }

    let candidate = if is_http {
        configured.to_string()
    } else {
        format!("https://{configured}")
    };
    let mut parsed = url::Url::parse(&candidate).map_err(|_| invalid_base_url_payload(configured))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(invalid_base_url_payload(configured));
    }
    let pathname = parsed.path().trim_end_matches('/').to_string();
    let final_path = if pathname.ends_with("/search") {
        pathname
    } else {
        format!("{pathname}/search")
    };
    parsed.set_path(&final_path);
    parsed.set_fragment(None);
    Ok(parsed.to_string())
}

/// Normalize freshness to Exa's accepted recency buckets.
pub fn normalize_exa_freshness(value: Option<&str>) -> Option<&'static str> {
    let lower = value?.trim().to_ascii_lowercase();
    EXA_FRESHNESS_VALUES.iter().find(|v| **v == lower).copied()
}

/// Resolve the freshness window into a `startPublishedDate` timestamp.
pub fn resolve_freshness_start_date(freshness: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let start = match freshness {
        "day" => now - chrono::Duration::days(1),
        "week" => now - chrono::Duration::days(7),
        "month" => {
            // Same day previous month, clamped to that month's length.
            let day = chrono::Datelike::day(&now);
            let first = now.date_naive().with_day(1).unwrap();
            let prev_month_first = first.pred_opt().unwrap().with_day(1).unwrap();
            let last_day = days_in_month(
                chrono::Datelike::year(&prev_month_first),
                chrono::Datelike::month(&prev_month_first),
            );
            let target = prev_month_first
                .with_day(day.min(last_day))
                .unwrap_or(prev_month_first);
            return format!("{}T{}", target.format("%Y-%m-%d"), now.format("%H:%M:%S%.3fZ"));
        }
        _ => now - chrono::Duration::days(365),
    };
    start.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn days_in_month(year: i32, month: u32) -> u32 {
    use chrono::Datelike;
    let first = chrono::NaiveDate::from_ymd_opt(year, month, 1).unwrap();
    let next = if month == 12 {
        chrono::NaiveDate::from_ymd_opt(year + 1, 1, 1).unwrap()
    } else {
        chrono::NaiveDate::from_ymd_opt(year, month + 1, 1).unwrap()
    };
    (next - first).num_days() as u32
}

use chrono::Datelike;

fn invalid_contents_payload(message: &str) -> serde_json::Value {
    search_error_payload("invalid_contents", message)
}

/// Validate the optional `contents` argument (upstream `parseExaContents`).
///
/// Accepts `{text, highlights, summary}` where each entry is a boolean or an
/// object with a strict field whitelist. Returns the normalized contents or
/// an error payload.
pub fn parse_exa_contents(
    raw: Option<&serde_json::Value>,
) -> Result<Option<serde_json::Value>, serde_json::Value> {
    let raw = match raw {
        None | Some(serde_json::Value::Null) => return Ok(None),
        Some(v) => v,
    };
    let obj = raw.as_object().ok_or_else(|| {
        invalid_contents_payload(
            "contents must be an object with optional text, highlights, and summary fields.",
        )
    })?;
    for key in obj.keys() {
        if !["text", "highlights", "summary"].contains(&key.as_str()) {
            return Err(invalid_contents_payload(&format!(
                "contents has unknown field \"{key}\". Only \"text\", \"highlights\", and \"summary\" are allowed."
            )));
        }
    }

    let mut parsed = serde_json::Map::new();

    if let Some(text) = obj.get("text") {
        parsed.insert("text".into(), parse_bool_or_object(text, "contents.text", &["maxCharacters"])?);
    }
    if let Some(highlights) = obj.get("highlights") {
        parsed.insert(
            "highlights".into(),
            parse_bool_or_object(
                highlights,
                "contents.highlights",
                &["maxCharacters", "query", "numSentences", "highlightsPerUrl"],
            )?,
        );
    }
    if let Some(summary) = obj.get("summary") {
        parsed.insert(
            "summary".into(),
            parse_bool_or_object(summary, "contents.summary", &["query"])?,
        );
    }

    if parsed.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::Value::Object(parsed)))
}

fn parse_bool_or_object(
    value: &serde_json::Value,
    label: &str,
    allowed: &[&str],
) -> Result<serde_json::Value, serde_json::Value> {
    if value.is_boolean() {
        return Ok(value.clone());
    }
    let obj = value
        .as_object()
        .ok_or_else(|| invalid_contents_payload(&format!("{label} must be a boolean or an object.")))?;
    for (key, field) in obj {
        if !allowed.contains(&key.as_str()) {
            return Err(invalid_contents_payload(&format!(
                "{label} has unknown field \"{key}\"."
            )));
        }
        let numeric_field = ["maxCharacters", "numSentences", "highlightsPerUrl"];
        if numeric_field.contains(&key.as_str()) {
            let ok = field.as_u64().map(|v| v > 0).unwrap_or(false)
                && field.as_f64().map(|f| f.fract() == 0.0).unwrap_or(false);
            if !ok {
                return Err(invalid_contents_payload(&format!(
                    "{label}.{key} must be a positive integer."
                )));
            }
        }
        if key == "query" && !field.is_string() {
            return Err(invalid_contents_payload(&format!("{label}.query must be a string.")));
        }
    }
    Ok(value.clone())
}

/// Cache key partitioned by endpoint, search type, filters, and contents.
pub fn build_exa_cache_key(params: &ExaCacheKeyParams<'_>) -> String {
    let count_str = params.count.to_string();
    let highlights = params
        .contents
        .and_then(|c| c.get("highlights"))
        .map(|v| v.to_string());
    let text = params.contents.and_then(|c| c.get("text")).map(|v| v.to_string());
    let summary = params
        .contents
        .and_then(|c| c.get("summary"))
        .map(|v| v.to_string());
    build_search_cache_key(&[
        Some("exa"),
        Some(params.endpoint),
        Some(params.search_type),
        Some(params.query),
        Some(&count_str),
        params.freshness,
        params.date_after,
        params.date_before,
        highlights.as_deref(),
        text.as_deref(),
        summary.as_deref(),
    ])
}

pub struct ExaCacheKeyParams<'a> {
    pub endpoint: &'a str,
    pub search_type: &'a str,
    pub query: &'a str,
    pub count: u64,
    pub freshness: Option<&'a str>,
    pub date_after: Option<&'a str>,
    pub date_before: Option<&'a str>,
    pub contents: Option<&'a serde_json::Value>,
}

pub struct ExaSearchRequest<'a> {
    pub query: &'a str,
    pub count: Option<u64>,
    pub search_type: Option<&'a str>,
    pub freshness: Option<&'a str>,
    pub date_after: Option<&'a str>,
    pub date_before: Option<&'a str>,
    pub contents: Option<&'a serde_json::Value>,
    pub api_key: &'a str,
    pub base_url: Option<&'a str>,
    pub timeout_seconds: u64,
    pub cache_ttl_ms: u64,
}

fn resolve_exa_description(entry: &serde_json::Value) -> String {
    if let Some(highlights) = entry["highlights"].as_array() {
        let text: Vec<&str> = highlights
            .iter()
            .filter_map(|h| h.as_str())
            .filter(|h| !h.trim().is_empty())
            .collect();
        if !text.is_empty() {
            return text.join("\n");
        }
    }
    if let Some(summary) = entry["summary"].as_str() {
        if !summary.trim().is_empty() {
            return summary.to_string();
        }
    }
    entry["text"].as_str().unwrap_or("").to_string()
}

/// Execute an Exa search request.
pub async fn execute_exa_search(req: ExaSearchRequest<'_>) -> Result<serde_json::Value> {
    if req.api_key.is_empty() {
        return Ok(json!({
            "error": "missing_exa_api_key",
            "message": "web_search (exa) needs an Exa API key. Set EXA_API_KEY in the Gateway environment, or configure tools.web.search.exa.apiKey.",
            "docs": super::common::WEB_TOOLS_DOCS_URL,
        }));
    }
    let endpoint = match resolve_exa_search_endpoint(req.base_url) {
        Ok(e) => e,
        Err(payload) => return Ok(payload),
    };

    let search_type = req
        .search_type
        .filter(|t| EXA_SEARCH_TYPES.contains(t))
        .unwrap_or("auto");

    let freshness = normalize_exa_freshness(req.freshness);
    if req.freshness.is_some() && freshness.is_none() {
        return Ok(search_error_payload(
            "invalid_freshness",
            "freshness must be one of \"day\", \"week\", \"month\", or \"year\".",
        ));
    }
    if freshness.is_some() && (req.date_after.is_some() || req.date_before.is_some()) {
        return Ok(search_error_payload(
            "conflicting_time_filters",
            "freshness cannot be combined with date_after or date_before. Use one time-filter mode.",
        ));
    }
    let (date_after, date_before) = match parse_iso_date_range(req.date_after, req.date_before) {
        Ok(range) => range,
        Err(payload) => return Ok(payload),
    };

    let contents = match parse_exa_contents(req.contents) {
        Ok(c) => c,
        Err(payload) => return Ok(payload),
    };

    let count = req
        .count
        .filter(|c| *c > 0)
        .map(|c| c.min(EXA_MAX_SEARCH_COUNT))
        .unwrap_or(super::common::DEFAULT_SEARCH_COUNT as u64);

    let cache_key = build_exa_cache_key(&ExaCacheKeyParams {
        endpoint: &endpoint,
        search_type,
        query: req.query,
        count,
        freshness,
        date_after: date_after.as_deref(),
        date_before: date_before.as_deref(),
        contents: contents.as_ref(),
    });
    if let Some(cached) = read_cached_search_payload(&cache_key) {
        return Ok(cached);
    }

    let mut body = json!({
        "query": req.query,
        "numResults": count,
        "type": search_type,
        "contents": contents.clone().unwrap_or(json!({"highlights": true})),
    });
    if let Some(after) = &date_after {
        body["startPublishedDate"] = json!(after);
    } else if let Some(freshness) = freshness {
        body["startPublishedDate"] = json!(resolve_freshness_start_date(freshness, chrono::Utc::now()));
    }
    if let Some(before) = &date_before {
        body["endPublishedDate"] = json!(before);
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
        .header("x-exa-integration", "mylobster")
        .json(&body)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(8 * 1024).collect();
        return Ok(search_error_payload(
            "exa_api_error",
            &format!("Exa API error ({status}): {detail}"),
        ));
    }

    let data: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(_) => {
            return Ok(search_error_payload(
                "exa_api_error",
                "Exa API returned malformed JSON",
            ))
        }
    };
    let results = normalize_exa_results(&data);
    let took_ms = started.elapsed().as_millis() as u64;

    let mapped: Vec<serde_json::Value> = results
        .iter()
        .map(|entry| {
            let url = entry["url"].as_str().unwrap_or("");
            let mut row = json!({
                "title": entry["title"].as_str().unwrap_or(""),
                "url": url,
                "description": resolve_exa_description(entry),
                "published": entry.get("publishedDate").and_then(|v| v.as_str()).filter(|s| !s.is_empty()),
                "siteName": resolve_site_name(url),
            });
            if let Some(summary) = entry["summary"].as_str().filter(|s| !s.is_empty()) {
                row["summary"] = json!(summary);
            }
            if let Some(scores) = entry["highlightScores"].as_array() {
                let numeric: Vec<f64> = scores.iter().filter_map(|s| s.as_f64()).collect();
                if !numeric.is_empty() {
                    row["highlightScores"] = json!(numeric);
                }
            }
            row
        })
        .collect();

    let payload = json!({
        "query": req.query,
        "provider": "exa",
        "count": mapped.len(),
        "tookMs": took_ms,
        "results": mapped,
    });

    write_cached_search_payload(&cache_key, &payload, req.cache_ttl_ms);
    Ok(payload)
}

/// Extract the result-object array, tolerating malformed payload shapes.
pub fn normalize_exa_results(payload: &serde_json::Value) -> Vec<serde_json::Value> {
    payload["results"]
        .as_array()
        .map(|arr| arr.iter().filter(|e| e.is_object()).cloned().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_defaults_when_unset() {
        assert_eq!(resolve_exa_search_endpoint(None).unwrap(), EXA_SEARCH_ENDPOINT);
        assert_eq!(resolve_exa_search_endpoint(Some("  ")).unwrap(), EXA_SEARCH_ENDPOINT);
    }

    #[test]
    fn endpoint_appends_search_path() {
        assert_eq!(
            resolve_exa_search_endpoint(Some("https://exa.corp.internal")).unwrap(),
            "https://exa.corp.internal/search"
        );
        assert_eq!(
            resolve_exa_search_endpoint(Some("https://exa.corp.internal/v1/")).unwrap(),
            "https://exa.corp.internal/v1/search"
        );
        // Already ends with /search — do not double-append.
        assert_eq!(
            resolve_exa_search_endpoint(Some("https://exa.corp.internal/search")).unwrap(),
            "https://exa.corp.internal/search"
        );
    }

    #[test]
    fn endpoint_prefixes_https_for_schemeless_values() {
        assert_eq!(
            resolve_exa_search_endpoint(Some("exa.example.com")).unwrap(),
            "https://exa.example.com/search"
        );
    }

    #[test]
    fn endpoint_rejects_non_http_schemes() {
        let err = resolve_exa_search_endpoint(Some("ftp://exa.example.com")).unwrap_err();
        assert_eq!(err["error"], "invalid_base_url");
        let err = resolve_exa_search_endpoint(Some("file:///etc")).unwrap_err();
        assert_eq!(err["error"], "invalid_base_url");
    }

    #[test]
    fn cache_keys_partition_by_endpoint() {
        let base = ExaCacheKeyParams {
            endpoint: "https://api.exa.ai/search",
            search_type: "auto",
            query: "q",
            count: 5,
            freshness: None,
            date_after: None,
            date_before: None,
            contents: None,
        };
        let a = build_exa_cache_key(&base);
        let b = build_exa_cache_key(&ExaCacheKeyParams {
            endpoint: "https://exa.corp.internal/search",
            ..base
        });
        assert_ne!(a, b);
    }

    #[test]
    fn cache_keys_partition_by_contents() {
        let contents = serde_json::json!({"text": true});
        let base = ExaCacheKeyParams {
            endpoint: "https://api.exa.ai/search",
            search_type: "auto",
            query: "q",
            count: 5,
            freshness: None,
            date_after: None,
            date_before: None,
            contents: None,
        };
        let a = build_exa_cache_key(&base);
        let b = build_exa_cache_key(&ExaCacheKeyParams {
            contents: Some(&contents),
            ..base
        });
        assert_ne!(a, b);
    }

    #[test]
    fn contents_validation_rejects_unknown_fields() {
        let bad = serde_json::json!({"bogus": true});
        assert_eq!(parse_exa_contents(Some(&bad)).unwrap_err()["error"], "invalid_contents");
        let bad = serde_json::json!({"text": {"bogus": 1}});
        assert_eq!(parse_exa_contents(Some(&bad)).unwrap_err()["error"], "invalid_contents");
        let bad = serde_json::json!({"text": {"maxCharacters": -1}});
        assert_eq!(parse_exa_contents(Some(&bad)).unwrap_err()["error"], "invalid_contents");
        let bad = serde_json::json!({"highlights": {"query": 42}});
        assert_eq!(parse_exa_contents(Some(&bad)).unwrap_err()["error"], "invalid_contents");
    }

    #[test]
    fn contents_validation_accepts_valid_shapes() {
        let ok = serde_json::json!({
            "text": {"maxCharacters": 100},
            "highlights": true,
            "summary": {"query": "topic"}
        });
        let parsed = parse_exa_contents(Some(&ok)).unwrap().unwrap();
        assert_eq!(parsed["text"]["maxCharacters"], 100);
        assert_eq!(parsed["highlights"], true);
        assert_eq!(parsed["summary"]["query"], "topic");
        assert_eq!(parse_exa_contents(None).unwrap(), None);
        let empty = serde_json::json!({});
        assert_eq!(parse_exa_contents(Some(&empty)).unwrap(), None);
    }

    #[test]
    fn freshness_start_dates() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-03-31T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert!(resolve_freshness_start_date("day", now).starts_with("2026-03-30T"));
        assert!(resolve_freshness_start_date("week", now).starts_with("2026-03-24T"));
        // Month clamps 31 March back to 28 February (2026 is not a leap year).
        assert!(resolve_freshness_start_date("month", now).starts_with("2026-02-28T"));
        assert!(resolve_freshness_start_date("year", now).starts_with("2025-03-31T"));
    }

    #[test]
    fn normalize_results_tolerates_malformed_payloads() {
        assert!(normalize_exa_results(&serde_json::json!(null)).is_empty());
        assert!(normalize_exa_results(&serde_json::json!({"results": "nope"})).is_empty());
        let mixed = serde_json::json!({"results": [{"title": "a"}, 42, null]});
        assert_eq!(normalize_exa_results(&mixed).len(), 1);
    }

    #[tokio::test]
    async fn search_posts_to_configured_endpoint_with_key_header() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .and(header("x-api-key", "exa-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{
                    "title": "Result",
                    "url": "https://www.example.com/post",
                    "highlights": ["h1", "h2"],
                    "highlightScores": [0.9, 0.5],
                    "publishedDate": "2026-05-01"
                }]
            })))
            .mount(&server)
            .await;

        let payload = execute_exa_search(ExaSearchRequest {
            query: "exa endpoint test",
            count: Some(3),
            search_type: None,
            freshness: None,
            date_after: None,
            date_before: None,
            contents: None,
            api_key: "exa-key",
            base_url: Some(&server.uri()),
            timeout_seconds: 5,
            cache_ttl_ms: 0,
        })
        .await
        .unwrap();
        assert_eq!(payload["provider"], "exa");
        assert_eq!(payload["results"][0]["description"], "h1\nh2");
        assert_eq!(payload["results"][0]["siteName"], "example.com");
        assert_eq!(payload["results"][0]["highlightScores"][0], 0.9);
    }

    #[tokio::test]
    async fn missing_key_returns_structured_payload() {
        let payload = execute_exa_search(ExaSearchRequest {
            query: "q",
            count: None,
            search_type: None,
            freshness: None,
            date_after: None,
            date_before: None,
            contents: None,
            api_key: "",
            base_url: None,
            timeout_seconds: 5,
            cache_ttl_ms: 0,
        })
        .await
        .unwrap();
        assert_eq!(payload["error"], "missing_exa_api_key");
    }
}
