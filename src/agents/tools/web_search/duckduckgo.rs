//! DuckDuckGo key-free web-search provider (v2026.7.1 parity).
//!
//! Ports `extensions/duckduckgo/src/ddg-client.ts`: HTML endpoint scraping
//! with entity decoding, `uddg` redirect-URL extraction, bot-challenge
//! detection, and safe-search/region params. Key-free providers are explicit
//! opt-ins — never auto-selected.

use super::cache::{normalize_cache_key, read_cached_search_payload, write_cached_search_payload};
use super::common::{resolve_search_count, resolve_site_name, DEFAULT_SEARCH_COUNT};
use anyhow::Result;
use serde_json::json;

pub const DDG_HTML_ENDPOINT: &str = "https://html.duckduckgo.com/html";
pub const DDG_DEFAULT_TIMEOUT_SECONDS: u64 = 20;

/// Safe-search levels and their `kp` parameter values.
pub fn ddg_safe_search_param(safe_search: &str) -> &'static str {
    match safe_search {
        "strict" => "1",
        "off" => "-2",
        _ => "-1", // moderate (default)
    }
}

fn is_decodable_code_point(cp: u32) -> bool {
    cp <= 0x10ffff && !(0xd800..=0xdfff).contains(&cp)
}

/// Decode the HTML entities DuckDuckGo's HTML endpoint emits.
pub fn decode_html_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let entity_rest = &rest[start..];
        let Some(end) = entity_rest.find(';') else {
            out.push('&');
            rest = &rest[start + 1..];
            continue;
        };
        let entity = &entity_rest[..=end];
        let lower = entity.to_ascii_lowercase();
        let decoded: Option<String> = match lower.as_str() {
            "&lt;" => Some("<".into()),
            "&gt;" => Some(">".into()),
            "&quot;" => Some("\"".into()),
            "&apos;" | "&#39;" | "&#x27;" => Some("'".into()),
            "&#x2f;" => Some("/".into()),
            "&nbsp;" => Some(" ".into()),
            "&ndash;" => Some("-".into()),
            "&mdash;" => Some("--".into()),
            "&hellip;" => Some("...".into()),
            "&amp;" => Some("&".into()),
            _ if lower.starts_with("&#x") => {
                u32::from_str_radix(&lower[3..lower.len() - 1], 16)
                    .ok()
                    .filter(|cp| is_decodable_code_point(*cp))
                    .and_then(char::from_u32)
                    .map(String::from)
            }
            _ if lower.starts_with("&#") => lower[2..lower.len() - 1]
                .parse::<u32>()
                .ok()
                .filter(|cp| is_decodable_code_point(*cp))
                .and_then(char::from_u32)
                .map(String::from),
            _ => None,
        };
        match decoded {
            Some(s) => out.push_str(&s),
            None => out.push_str(entity),
        }
        rest = &entity_rest[end + 1..];
    }
    out.push_str(rest);
    out
}

fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Extract the real target from a DDG redirect URL (`uddg` param); direct
/// links pass through unchanged.
pub fn decode_duckduckgo_url(raw_url: &str) -> String {
    let normalized = if raw_url.starts_with("//") {
        format!("https:{raw_url}")
    } else {
        raw_url.to_string()
    };
    if let Ok(parsed) = url::Url::parse(&normalized) {
        for (key, value) in parsed.query_pairs() {
            if key == "uddg" && !value.is_empty() {
                return value.into_owned();
            }
        }
    }
    raw_url.to_string()
}

/// True when the response is a bot-detection challenge, not a result page.
pub fn is_bot_challenge(html: &str) -> bool {
    if html.contains("result__a") {
        return false;
    }
    let lower = html.to_ascii_lowercase();
    lower.contains("g-recaptcha")
        || lower.contains("are you a human")
        || lower.contains("id=\"challenge-form\"")
        || lower.contains("name=\"challenge\"")
}

#[derive(Debug, Clone, PartialEq)]
pub struct DuckDuckGoResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

fn find_attr(tag: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_string())
}

/// Parse DDG's HTML result page into structured rows. Anchors with class
/// `result__a` carry title+href; the following `result__snippet` anchor
/// (before the next result) carries the snippet.
pub fn parse_duckduckgo_html(html: &str) -> Vec<DuckDuckGoResult> {
    let mut results = Vec::new();
    let mut search_from = 0usize;

    while let Some(rel_pos) = html[search_from..].find("result__a") {
        let class_pos = search_from + rel_pos;
        // Find the enclosing <a ...> open tag.
        let Some(tag_start) = html[..class_pos].rfind("<a") else {
            search_from = class_pos + 1;
            continue;
        };
        let Some(tag_end_rel) = html[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + tag_end_rel;
        let open_tag = &html[tag_start..=tag_end];
        // Guard: the class must belong to this anchor's class attribute.
        if !find_attr(open_tag, "class").map(|c| c.contains("result__a")).unwrap_or(false) {
            search_from = class_pos + "result__a".len();
            continue;
        }
        let raw_url = find_attr(open_tag, "href").unwrap_or_default();
        let Some(close_rel) = html[tag_end..].find("</a>") else {
            break;
        };
        let raw_title = &html[tag_end + 1..tag_end + close_rel];
        let after_anchor = tag_end + close_rel + 4;

        // Scope the snippet search to before the next result anchor.
        let trailing = &html[after_anchor..];
        let next_result = trailing.find("result__a").unwrap_or(trailing.len());
        let scoped = &trailing[..next_result];
        let snippet = scoped
            .find("result__snippet")
            .and_then(|pos| {
                let seg = &scoped[pos..];
                let start = seg.find('>')? + 1;
                let end = seg.find("</a>")?;
                if start <= end {
                    Some(seg[start..end].to_string())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        let title = decode_html_entities(&strip_html(raw_title));
        let url = decode_duckduckgo_url(&decode_html_entities(&raw_url));
        let snippet = decode_html_entities(&strip_html(&snippet));
        if !title.is_empty() && !url.is_empty() {
            results.push(DuckDuckGoResult { title, url, snippet });
        }
        search_from = after_anchor;
    }
    results
}

pub struct DuckDuckGoSearchRequest<'a> {
    pub query: &'a str,
    pub count: Option<u64>,
    pub region: Option<&'a str>,
    pub safe_search: Option<&'a str>,
    pub timeout_seconds: u64,
    pub cache_ttl_ms: u64,
    /// Endpoint override for tests; defaults to [`DDG_HTML_ENDPOINT`].
    pub endpoint: Option<&'a str>,
}

/// Run a DuckDuckGo HTML search.
pub async fn run_duckduckgo_search(req: DuckDuckGoSearchRequest<'_>) -> Result<serde_json::Value> {
    let count = resolve_search_count(req.count, DEFAULT_SEARCH_COUNT) as usize;
    let safe_search = req.safe_search.unwrap_or("moderate");
    let cache_key = normalize_cache_key(
        &json!({
            "provider": "duckduckgo",
            "query": req.query,
            "count": count,
            "region": req.region.unwrap_or(""),
            "safeSearch": safe_search,
        })
        .to_string(),
    );
    if let Some(cached) = read_cached_search_payload(&cache_key) {
        return Ok(cached);
    }

    let mut url = url::Url::parse(req.endpoint.unwrap_or(DDG_HTML_ENDPOINT))?;
    url.query_pairs_mut().append_pair("q", req.query);
    if let Some(region) = req.region {
        url.query_pairs_mut().append_pair("kl", region);
    }
    url.query_pairs_mut()
        .append_pair("kp", ddg_safe_search_param(safe_search));

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(req.timeout_seconds))
        .build()?;
    let started = std::time::Instant::now();
    let response = client
        .get(url)
        .header(
            "User-Agent",
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36",
        )
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        anyhow::bail!("DuckDuckGo search error ({status})");
    }
    let html = response.text().await?;
    if is_bot_challenge(&html) {
        anyhow::bail!("DuckDuckGo returned a bot-detection challenge.");
    }
    let results: Vec<DuckDuckGoResult> =
        parse_duckduckgo_html(&html).into_iter().take(count).collect();

    let payload = json!({
        "query": req.query,
        "provider": "duckduckgo",
        "count": results.len(),
        "tookMs": started.elapsed().as_millis() as u64,
        "results": results
            .iter()
            .map(|r| json!({
                "title": r.title,
                "url": r.url,
                "snippet": r.snippet,
                "siteName": resolve_site_name(&r.url),
            }))
            .collect::<Vec<_>>(),
    });
    write_cached_search_payload(&cache_key, &payload, req.cache_ttl_ms);
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_decoding() {
        assert_eq!(decode_html_entities("a &amp; b &lt;c&gt;"), "a & b <c>");
        assert_eq!(decode_html_entities("&#39;x&#x27;"), "'x'");
        assert_eq!(decode_html_entities("&#65;&#x42;"), "AB");
        // Surrogate code points stay as-is.
        assert_eq!(decode_html_entities("&#xd800;"), "&#xd800;");
        assert_eq!(decode_html_entities("&bogus;"), "&bogus;");
    }

    #[test]
    fn redirect_url_decoding() {
        assert_eq!(
            decode_duckduckgo_url(
                "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&rut=abc"
            ),
            "https://example.com/page"
        );
        assert_eq!(
            decode_duckduckgo_url("https://direct.example.com/x"),
            "https://direct.example.com/x"
        );
    }

    #[test]
    fn bot_challenge_detection() {
        assert!(is_bot_challenge("<div class=\"g-recaptcha\"></div>"));
        assert!(is_bot_challenge("<form id=\"challenge-form\"></form>"));
        // A page with results is never a challenge even if words match.
        assert!(!is_bot_challenge(
            "<a class=\"result__a\" href=\"x\">are you a human</a>"
        ));
        assert!(!is_bot_challenge("<p>normal page</p>"));
    }

    #[test]
    fn html_parsing_extracts_results_with_snippets() {
        let html = r##"
            <a rel="noopener" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fa.example.com">Title &amp; One</a>
            <a class="result__snippet" href="#">Snippet <b>one</b></a>
            <a class="result__a" href="https://b.example.com">Title Two</a>
        "##;
        let results = parse_duckduckgo_html(html);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Title & One");
        assert_eq!(results[0].url, "https://a.example.com");
        assert_eq!(results[0].snippet, "Snippet one");
        assert_eq!(results[1].title, "Title Two");
        assert_eq!(results[1].snippet, "");
    }

    #[test]
    fn snippet_scoped_to_current_result() {
        // The snippet after the SECOND result must not leak into the first.
        let html = r##"
            <a class="result__a" href="https://a.example.com">A</a>
            <a class="result__a" href="https://b.example.com">B</a>
            <a class="result__snippet" href="#">only for b</a>
        "##;
        let results = parse_duckduckgo_html(html);
        assert_eq!(results[0].snippet, "");
        assert_eq!(results[1].snippet, "only for b");
    }

    #[test]
    fn safe_search_params() {
        assert_eq!(ddg_safe_search_param("strict"), "1");
        assert_eq!(ddg_safe_search_param("moderate"), "-1");
        assert_eq!(ddg_safe_search_param("off"), "-2");
        assert_eq!(ddg_safe_search_param("bogus"), "-1");
    }

    #[tokio::test]
    async fn search_hits_html_endpoint_and_parses() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/html"))
            .and(query_param("q", "ddg test"))
            .and(query_param("kp", "-1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<a class="result__a" href="https://found.example.com">Found</a>"#,
            ))
            .mount(&server)
            .await;

        let endpoint = format!("{}/html", server.uri());
        let payload = run_duckduckgo_search(DuckDuckGoSearchRequest {
            query: "ddg test",
            count: Some(5),
            region: None,
            safe_search: None,
            timeout_seconds: 5,
            cache_ttl_ms: 0,
            endpoint: Some(&endpoint),
        })
        .await
        .unwrap();
        assert_eq!(payload["provider"], "duckduckgo");
        assert_eq!(payload["results"][0]["title"], "Found");
        assert_eq!(payload["results"][0]["siteName"], "found.example.com");
    }

    #[tokio::test]
    async fn bot_challenge_fails_the_search() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/html"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"<div class="g-recaptcha">are you a human</div>"#),
            )
            .mount(&server)
            .await;

        let endpoint = format!("{}/html", server.uri());
        let err = run_duckduckgo_search(DuckDuckGoSearchRequest {
            query: "ddg challenge test",
            count: Some(5),
            region: None,
            safe_search: None,
            timeout_seconds: 5,
            cache_ttl_ms: 0,
            endpoint: Some(&endpoint),
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("bot-detection"), "{err}");
    }
}
