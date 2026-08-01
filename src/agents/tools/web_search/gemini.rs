//! Gemini grounding web-search provider (v2026.7.1 parity).
//!
//! Ports `extensions/google/src/gemini-web-search-provider.runtime.ts`:
//! reuses the Google model-provider API key / base URL as fallback,
//! translates freshness/date filters into `google_search.timeRangeFilter`
//! grounding filters (with the day-freshness soft-hint exception), sends the
//! key via `x-goog-api-key`, and hardens parsing of grounding responses.

use super::cache::{build_search_cache_key, read_cached_search_payload, write_cached_search_payload};
use super::common::{
    parse_web_search_time_filters, search_error_payload, FreshnessProvider,
};
use crate::providers::gemini::normalize_google_api_base_url;
use anyhow::Result;
use serde_json::json;

pub const DEFAULT_GEMINI_WEB_SEARCH_MODEL: &str = "gemini-2.5-flash";

const GEMINI_DAY_FRESHNESS_HINT: &str =
    "Prioritize web sources published in the last 24 hours.";

/// Resolve the API key with the v2026.7.1 fallback chain:
/// `tools.web.search.gemini.apiKey` → `GEMINI_API_KEY` env →
/// `models.providers.google.apiKey`.
pub fn resolve_gemini_search_api_key(
    configured: Option<&str>,
    env_key: Option<&str>,
    google_provider_key: Option<&str>,
) -> Option<String> {
    for candidate in [configured, env_key, google_provider_key] {
        if let Some(key) = candidate.map(str::trim).filter(|k| !k.is_empty()) {
            return Some(key.to_string());
        }
    }
    None
}

/// Resolve the base URL: search config → Google provider base URL → default,
/// normalized so a bare `generativelanguage.googleapis.com` origin gets its
/// `/v1beta` path.
pub fn resolve_gemini_search_base_url(
    configured: Option<&str>,
    google_provider_base_url: Option<&str>,
) -> String {
    let raw = configured
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| google_provider_base_url.map(str::trim).filter(|s| !s.is_empty()));
    normalize_google_api_base_url(raw)
}

/// Gemini's `google_search.time_range_filter` accepts second-precision RFC
/// 3339 only; any fractional component is rejected with
/// "Granularity of nano is not supported".
pub fn to_gemini_time_range_timestamp(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn iso_date_start(value: &str) -> String {
    format!("{value}T00:00:00Z")
}

fn iso_date_exclusive_end(value: &str) -> Option<String> {
    let date = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
    let next = date.succ_opt()?;
    Some(format!("{}T00:00:00Z", next.format("%Y-%m-%d")))
}

fn freshness_days(freshness: &str) -> i64 {
    match freshness {
        "day" => 1,
        "week" => 7,
        "month" => 30,
        _ => 365,
    }
}

/// Resolved grounding time filter.
#[derive(Debug, Default, PartialEq)]
pub struct GeminiTimeRange {
    /// `google_search.timeRangeFilter` start/end (RFC 3339, second precision).
    pub time_range_filter: Option<(String, String)>,
    /// Day-freshness falls back to a soft prompt hint: Gemini rejects
    /// 24-hour timeRangeFilter windows.
    pub soft_day_freshness: bool,
}

/// Translate freshness/date args into the grounding time filter.
pub fn resolve_gemini_time_range(
    raw_freshness: Option<&str>,
    raw_date_after: Option<&str>,
    raw_date_before: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<GeminiTimeRange, serde_json::Value> {
    let filters = parse_web_search_time_filters(
        raw_freshness,
        raw_date_after,
        raw_date_before,
        FreshnessProvider::Perplexity,
        "freshness must be day, week, month, year, or the shortcuts pd, pw, pm, py.",
    )?;

    if let Some(freshness) = &filters.freshness {
        if freshness == "day" {
            return Ok(GeminiTimeRange { time_range_filter: None, soft_day_freshness: true });
        }
        let start = now - chrono::Duration::days(freshness_days(freshness));
        return Ok(GeminiTimeRange {
            time_range_filter: Some((
                to_gemini_time_range_timestamp(start),
                to_gemini_time_range_timestamp(now),
            )),
            soft_day_freshness: false,
        });
    }

    if filters.date_after.is_none() && filters.date_before.is_none() {
        return Ok(GeminiTimeRange::default());
    }

    let start = filters
        .date_after
        .as_deref()
        .map(iso_date_start)
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());
    let end = match filters.date_before.as_deref() {
        Some(before) => iso_date_exclusive_end(before).ok_or_else(|| {
            search_error_payload("invalid_date", "date_before must be YYYY-MM-DD format.")
        })?,
        None => to_gemini_time_range_timestamp(now),
    };
    Ok(GeminiTimeRange {
        time_range_filter: Some((start, end)),
        soft_day_freshness: false,
    })
}

/// Append the day-freshness soft hint to a query when applicable.
pub fn query_with_soft_freshness(query: &str, soft_day_freshness: bool) -> String {
    if !soft_day_freshness {
        return query.to_string();
    }
    format!(
        "{query}\n\nSearch recency instruction: {GEMINI_DAY_FRESHNESS_HINT} If no matching recent sources are available, state that limitation and use the most relevant available sources."
    )
}

/// Redact `key=...` URL fragments from error text so provider errors never
/// leak API keys.
pub fn redact_gemini_key(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    loop {
        // Match case-insensitive "key=" boundaries.
        let lower = rest.to_ascii_lowercase();
        match lower.find("key=") {
            Some(idx) => {
                out.push_str(&rest[..idx + 4]);
                let after = &rest[idx + 4..];
                let end = after
                    .find(|c: char| c == '&' || c.is_whitespace())
                    .unwrap_or(after.len());
                out.push_str("***");
                rest = &after[end..];
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    out
}

/// Parse a grounding response into `(content, citations)`.
///
/// Malformed shapes (no candidates array, missing content/parts, empty text,
/// non-array groundingChunks) produce a uniform "malformed JSON response"
/// error, mirroring upstream `throwMalformedGeminiResponse`.
pub fn parse_gemini_grounding_response(
    data: &serde_json::Value,
) -> Result<(String, Vec<serde_json::Value>), String> {
    if let Some(error) = data.get("error").filter(|e| !e.is_null()) {
        let message = error["message"]
            .as_str()
            .or_else(|| error["status"].as_str())
            .unwrap_or("unknown");
        let code = error["code"].as_i64().unwrap_or(0);
        return Err(redact_gemini_key(&format!("Gemini API error ({code}): {message}")));
    }
    let candidates = data["candidates"]
        .as_array()
        .ok_or("Gemini API error: malformed JSON response")?;
    let candidate = candidates
        .first()
        .and_then(|c| c.as_object())
        .ok_or("Gemini API error: malformed JSON response")?;
    let parts = candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
        .ok_or("Gemini API error: malformed JSON response")?;
    let content = parts
        .iter()
        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if content.is_empty() {
        return Err("Gemini API error: malformed JSON response".to_string());
    }
    let grounding_metadata = candidate.get("groundingMetadata");
    let grounding_chunks = match grounding_metadata {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(metadata) => {
            if !metadata.is_object() {
                return Err("Gemini API error: malformed JSON response".to_string());
            }
            match metadata.get("groundingChunks") {
                None | Some(serde_json::Value::Null) => Vec::new(),
                Some(chunks) => chunks
                    .as_array()
                    .ok_or("Gemini API error: malformed JSON response")?
                    .clone(),
            }
        }
    };
    let citations = grounding_chunks
        .iter()
        .filter_map(|chunk| {
            let web = chunk.get("web")?.as_object()?;
            let uri = web.get("uri")?.as_str()?;
            Some(json!({
                "url": uri,
                "title": web.get("title").and_then(|t| t.as_str()),
            }))
        })
        .collect();
    Ok((content, citations))
}

pub struct GeminiSearchRequest<'a> {
    pub query: &'a str,
    pub count: Option<u64>,
    pub freshness: Option<&'a str>,
    pub date_after: Option<&'a str>,
    pub date_before: Option<&'a str>,
    pub api_key: &'a str,
    pub base_url: &'a str,
    pub model: &'a str,
    pub timeout_seconds: u64,
    pub cache_ttl_ms: u64,
}

/// Execute a Gemini grounding web search.
pub async fn execute_gemini_search(req: GeminiSearchRequest<'_>) -> Result<serde_json::Value> {
    if req.api_key.is_empty() {
        return Ok(search_error_payload(
            "missing_gemini_api_key",
            "web_search (gemini) needs an API key. Set GEMINI_API_KEY in the Gateway environment, configure tools.web.search.gemini.apiKey, or reuse models.providers.google.apiKey.",
        ));
    }
    let time_range =
        match resolve_gemini_time_range(req.freshness, req.date_after, req.date_before, chrono::Utc::now()) {
            Ok(range) => range,
            Err(payload) => return Ok(payload),
        };

    let count = super::common::resolve_search_count(req.count, super::common::DEFAULT_SEARCH_COUNT);
    let count_str = count.to_string();
    let cache_key = build_search_cache_key(&[
        Some("gemini"),
        Some(req.query),
        Some(&count_str),
        Some(req.base_url),
        Some(req.model),
        time_range.soft_day_freshness.then_some("day"),
        time_range.time_range_filter.as_ref().map(|(s, _)| s.as_str()),
        time_range.time_range_filter.as_ref().map(|(_, e)| e.as_str()),
    ]);
    if let Some(cached) = read_cached_search_payload(&cache_key) {
        return Ok(cached);
    }

    let google_search = match &time_range.time_range_filter {
        Some((start, end)) => json!({
            "timeRangeFilter": {"startTime": start, "endTime": end}
        }),
        None => json!({}),
    };
    let body = json!({
        "contents": [{"parts": [{"text": query_with_soft_freshness(req.query, time_range.soft_day_freshness)}]}],
        "tools": [{"google_search": google_search}],
    });

    let endpoint = format!("{}/models/{}:generateContent", req.base_url, req.model);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(req.timeout_seconds))
        .build()?;
    let started = std::time::Instant::now();
    let response = client
        .post(&endpoint)
        .header("Content-Type", "application/json")
        .header("x-goog-api-key", req.api_key)
        .json(&body)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(1_000).collect();
        return Ok(search_error_payload(
            "gemini_api_error",
            &redact_gemini_key(&format!("Gemini API error ({status}): {detail}")),
        ));
    }
    let data: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(_) => {
            return Ok(search_error_payload(
                "gemini_api_error",
                "Gemini API error: malformed JSON response",
            ))
        }
    };
    let (content, citations) = match parse_gemini_grounding_response(&data) {
        Ok(parsed) => parsed,
        Err(message) => return Ok(search_error_payload("gemini_api_error", &message)),
    };

    let payload = json!({
        "query": req.query,
        "provider": "gemini",
        "model": req.model,
        "tookMs": started.elapsed().as_millis() as u64,
        "content": content,
        "citations": citations,
    });
    write_cached_search_payload(&cache_key, &payload, req.cache_ttl_ms);
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-06-15T10:30:45.123Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn api_key_fallback_chain() {
        assert_eq!(
            resolve_gemini_search_api_key(Some("cfg"), Some("env"), Some("prov")).as_deref(),
            Some("cfg")
        );
        assert_eq!(
            resolve_gemini_search_api_key(None, Some("env"), Some("prov")).as_deref(),
            Some("env")
        );
        assert_eq!(
            resolve_gemini_search_api_key(None, None, Some("prov")).as_deref(),
            Some("prov")
        );
        assert_eq!(resolve_gemini_search_api_key(Some(" "), None, None), None);
    }

    #[test]
    fn base_url_falls_back_to_google_provider() {
        assert_eq!(
            resolve_gemini_search_base_url(None, Some("https://generativelanguage.googleapis.com")),
            "https://generativelanguage.googleapis.com/v1beta"
        );
        assert_eq!(
            resolve_gemini_search_base_url(Some("https://proxy.corp/v1beta/"), None),
            "https://proxy.corp/v1beta"
        );
        assert_eq!(
            resolve_gemini_search_base_url(None, None),
            "https://generativelanguage.googleapis.com/v1beta"
        );
    }

    #[test]
    fn timestamps_strip_fractional_seconds() {
        assert_eq!(to_gemini_time_range_timestamp(fixed_now()), "2026-06-15T10:30:45Z");
    }

    #[test]
    fn day_freshness_uses_soft_hint_not_filter() {
        let range = resolve_gemini_time_range(Some("pd"), None, None, fixed_now()).unwrap();
        assert!(range.soft_day_freshness);
        assert!(range.time_range_filter.is_none());
        let hinted = query_with_soft_freshness("q", true);
        assert!(hinted.contains("last 24 hours"));
        assert_eq!(query_with_soft_freshness("q", false), "q");
    }

    #[test]
    fn wider_freshness_maps_to_time_range_filter() {
        let range = resolve_gemini_time_range(Some("week"), None, None, fixed_now()).unwrap();
        let (start, end) = range.time_range_filter.unwrap();
        assert_eq!(start, "2026-06-08T10:30:45Z");
        assert_eq!(end, "2026-06-15T10:30:45Z");
        assert!(!range.soft_day_freshness);
    }

    #[test]
    fn date_range_maps_to_inclusive_start_exclusive_end() {
        let range = resolve_gemini_time_range(None, Some("2026-01-01"), Some("2026-01-31"), fixed_now())
            .unwrap();
        let (start, end) = range.time_range_filter.unwrap();
        assert_eq!(start, "2026-01-01T00:00:00Z");
        assert_eq!(end, "2026-02-01T00:00:00Z", "date_before end is exclusive (+1 day)");
    }

    #[test]
    fn date_after_only_extends_to_now() {
        let range =
            resolve_gemini_time_range(None, Some("2026-01-01"), None, fixed_now()).unwrap();
        let (start, end) = range.time_range_filter.unwrap();
        assert_eq!(start, "2026-01-01T00:00:00Z");
        assert_eq!(end, "2026-06-15T10:30:45Z");
    }

    #[test]
    fn date_before_only_starts_at_epoch() {
        let range =
            resolve_gemini_time_range(None, None, Some("2026-01-31"), fixed_now()).unwrap();
        let (start, end) = range.time_range_filter.unwrap();
        assert_eq!(start, "1970-01-01T00:00:00Z");
        assert_eq!(end, "2026-02-01T00:00:00Z");
    }

    #[test]
    fn no_filters_produce_empty_range() {
        let range = resolve_gemini_time_range(None, None, None, fixed_now()).unwrap();
        assert_eq!(range, GeminiTimeRange::default());
    }

    #[test]
    fn invalid_freshness_is_rejected() {
        let err = resolve_gemini_time_range(Some("fortnight"), None, None, fixed_now()).unwrap_err();
        assert_eq!(err["error"], "invalid_freshness");
    }

    #[test]
    fn grounding_response_parses_content_and_citations() {
        let data = serde_json::json!({
            "candidates": [{
                "content": {"parts": [{"text": "part one"}, {"text": "part two"}]},
                "groundingMetadata": {
                    "groundingChunks": [
                        {"web": {"uri": "https://source.example.com", "title": "Source"}},
                        {"notWeb": true},
                        {"web": {"noUri": true}}
                    ]
                }
            }]
        });
        let (content, citations) = parse_gemini_grounding_response(&data).unwrap();
        assert_eq!(content, "part one\npart two");
        assert_eq!(citations.len(), 1);
        assert_eq!(citations[0]["url"], "https://source.example.com");
        assert_eq!(citations[0]["title"], "Source");
    }

    #[test]
    fn grounding_response_without_metadata_is_ok() {
        let data = serde_json::json!({
            "candidates": [{"content": {"parts": [{"text": "answer"}]}}]
        });
        let (content, citations) = parse_gemini_grounding_response(&data).unwrap();
        assert_eq!(content, "answer");
        assert!(citations.is_empty());
    }

    #[test]
    fn malformed_grounding_shapes_are_rejected() {
        for data in [
            serde_json::json!({}),
            serde_json::json!({"candidates": "nope"}),
            serde_json::json!({"candidates": []}),
            serde_json::json!({"candidates": [{"content": {}}]}),
            serde_json::json!({"candidates": [{"content": {"parts": []}}]}),
            serde_json::json!({"candidates": [{
                "content": {"parts": [{"text": "x"}]},
                "groundingMetadata": {"groundingChunks": "bad"}
            }]}),
        ] {
            let err = parse_gemini_grounding_response(&data).unwrap_err();
            assert!(err.contains("malformed JSON response"), "{data}: {err}");
        }
    }

    #[test]
    fn api_error_payloads_redact_keys() {
        let data = serde_json::json!({
            "error": {"code": 400, "message": "bad request for key=AIzaSyExample&x=1"}
        });
        let err = parse_gemini_grounding_response(&data).unwrap_err();
        assert!(err.contains("key=***"), "{err}");
        assert!(!err.contains("AIzaSy"), "{err}");
    }

    #[test]
    fn redaction_handles_multiple_keys_and_case() {
        let redacted = redact_gemini_key("url?KEY=abc def key=xyz");
        assert!(!redacted.contains("abc"));
        assert!(!redacted.contains("xyz"));
    }

    #[tokio::test]
    async fn search_sends_key_header_and_grounding_tool() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/gemini-2.5-flash:generateContent"))
            .and(header("x-goog-api-key", "g-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [{
                    "content": {"parts": [{"text": "grounded answer"}]},
                    "groundingMetadata": {"groundingChunks": [
                        {"web": {"uri": "https://cite.example.com"}}
                    ]}
                }]
            })))
            .mount(&server)
            .await;

        let payload = execute_gemini_search(GeminiSearchRequest {
            query: "gemini grounding test",
            count: None,
            freshness: None,
            date_after: None,
            date_before: None,
            api_key: "g-key",
            base_url: &server.uri(),
            model: "gemini-2.5-flash",
            timeout_seconds: 5,
            cache_ttl_ms: 0,
        })
        .await
        .unwrap();
        assert_eq!(payload["provider"], "gemini");
        assert_eq!(payload["content"], "grounded answer");
        assert_eq!(payload["citations"][0]["url"], "https://cite.example.com");

        // Verify the grounding tool was in the request body and the key was
        // never in the URL.
        let requests = server.received_requests().await.unwrap();
        let req = requests.first().unwrap();
        assert!(!req.url.as_str().contains("key="), "API key must not be in URL");
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert!(body["tools"][0].get("google_search").is_some());
    }

    #[tokio::test]
    async fn missing_key_returns_structured_payload() {
        let payload = execute_gemini_search(GeminiSearchRequest {
            query: "q",
            count: None,
            freshness: None,
            date_after: None,
            date_before: None,
            api_key: "",
            base_url: "https://generativelanguage.googleapis.com/v1beta",
            model: DEFAULT_GEMINI_WEB_SEARCH_MODEL,
            timeout_seconds: 5,
            cache_ttl_ms: 0,
        })
        .await
        .unwrap();
        assert_eq!(payload["error"], "missing_gemini_api_key");
    }
}
