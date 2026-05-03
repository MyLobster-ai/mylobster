use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::debug;

/// Shared search result type used by SearXNG and X search providers.
#[derive(Debug)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// Web search tool supporting Brave, Perplexity, and Grok (xAI) providers.
pub struct WebSearchTool;

/// Default HTTP timeout for the xAI Grok `web_search` Responses-API call,
/// in seconds. Mirrors the v2026.5.2 default (#58063, #58733) — historical
/// no-timeout behavior could hang the tool loop when xAI was slow.
pub const GROK_WEB_SEARCH_DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Default Brave web-search endpoint. v2026.5.2 made this overridable per
/// deployment (compatible proxies / corporate gateways) via
/// `tools.web.search.brave.baseUrl` — see [`crate::config::types::BraveSearchConfig`].
pub const BRAVE_DEFAULT_BASE_URL: &str = "https://api.search.brave.com/res/v1/web/search";

// ============================================================================
// Brave Search Types
// ============================================================================

#[derive(Debug, Serialize, Deserialize)]
struct BraveSearchResponse {
    web: Option<BraveWebResults>,
}

#[derive(Debug, Serialize, Deserialize)]
struct BraveWebResults {
    results: Vec<BraveWebResult>,
}

#[derive(Debug, Serialize, Deserialize)]
struct BraveWebResult {
    title: String,
    url: String,
    description: String,
}

// ============================================================================
// Perplexity Types
// ============================================================================

#[derive(Debug, Serialize)]
struct PerplexityRequest {
    model: String,
    messages: Vec<PerplexityMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    search_recency_filter: Option<String>,
}

#[derive(Debug, Serialize)]
struct PerplexityMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct PerplexityResponse {
    choices: Vec<PerplexityChoice>,
    #[serde(default)]
    citations: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PerplexityChoice {
    message: PerplexityChoiceMessage,
}

#[derive(Debug, Deserialize)]
struct PerplexityChoiceMessage {
    content: String,
}

// ============================================================================
// Grok / xAI Types
// ============================================================================

#[derive(Debug, Serialize)]
struct GrokRequest {
    model: String,
    input: Vec<GrokInput>,
    tools: Vec<GrokTool>,
}

#[derive(Debug, Serialize)]
struct GrokInput {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct GrokTool {
    #[serde(rename = "type")]
    tool_type: String,
}

#[derive(Debug, Deserialize)]
struct GrokResponse {
    #[serde(default)]
    output: Vec<GrokOutput>,
}

#[derive(Debug, Deserialize)]
struct GrokOutput {
    #[serde(rename = "type")]
    output_type: String,
    #[serde(default)]
    content: Vec<GrokContent>,
}

#[derive(Debug, Deserialize)]
struct GrokContent {
    #[serde(rename = "type")]
    content_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    annotations: Option<Vec<GrokAnnotation>>,
}

#[derive(Debug, Deserialize)]
struct GrokAnnotation {
    url: Option<String>,
    title: Option<String>,
}

// ============================================================================
// Tool Implementation
// ============================================================================

#[async_trait::async_trait]
impl AgentTool for WebSearchTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "web_search".to_string(),
            description: "Search the web using a search engine".to_string(),
            category: "web".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "maxResults": { "type": "integer", "default": 10 },
                    "freshness": {
                        "type": "string",
                        "description": "Filter results by recency. Shortcuts: pd (past day), pw (past week), pm (past month), py (past year). Also accepts date ranges: YYYY-MM-DDtoYYYY-MM-DD"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult> {
        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing query parameter"))?;

        let max_results = params
            .get("maxResults")
            .and_then(|v| v.as_u64())
            .unwrap_or(10) as usize;

        let freshness = params
            .get("freshness")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let provider = context
            .config
            .tools
            .web
            .search
            .as_ref()
            .and_then(|s| s.provider.as_deref())
            .unwrap_or("brave");

        match provider {
            "brave" => {
                let env_api_key = std::env::var("BRAVE_API_KEY").ok();
                let api_key = context
                    .config
                    .tools
                    .web
                    .search
                    .as_ref()
                    .and_then(|s| s.api_key.as_deref())
                    .or_else(|| env_api_key.as_deref())
                    .unwrap_or("");

                if api_key.is_empty() {
                    return Ok(ToolResult::error("No Brave search API key configured"));
                }

                let brave_cfg = context
                    .config
                    .tools
                    .web
                    .search
                    .as_ref()
                    .and_then(|s| s.brave.as_ref());
                let base_url = brave_cfg
                    .and_then(|b| b.base_url.as_deref())
                    .unwrap_or(BRAVE_DEFAULT_BASE_URL);
                let http_diag = brave_cfg.and_then(|b| b.http).unwrap_or(false);

                search_brave(
                    query,
                    max_results,
                    api_key,
                    freshness.as_deref(),
                    base_url,
                    http_diag,
                )
                .await
            }
            "perplexity" => {
                search_perplexity(query, context, freshness.as_deref()).await
            }
            "grok" => {
                search_grok(query, context).await
            }
            // v2026.4.1: SearXNG bundled web search provider
            "searxng" => {
                searxng_search(query, &context.config).await
            }
            // v2026.4.1: X (Twitter) search via xAI Grok
            "x" | "x_search" | "twitter" => {
                x_search(query, &context.config).await
            }
            _ => Ok(ToolResult::error(format!(
                "Unknown search provider: {}",
                provider
            ))),
        }
    }
}

// ============================================================================
// Brave Search
// ============================================================================

async fn search_brave(
    query: &str,
    max_results: usize,
    api_key: &str,
    freshness: Option<&str>,
    base_url: &str,
    http_diag: bool,
) -> Result<ToolResult> {
    let client = reqwest::Client::new();

    let mut query_params = vec![
        ("q".to_string(), query.to_string()),
        ("count".to_string(), max_results.to_string()),
    ];

    if let Some(f) = freshness {
        query_params.push(("freshness".to_string(), f.to_string()));
    }

    if http_diag {
        // v2026.5.2 brave.http diagnostics: log endpoint + non-secret query
        // params, never the API key or response body.
        debug!(
            target: "brave.http",
            base_url = base_url,
            query = query,
            count = max_results,
            freshness = ?freshness,
            "brave.http: outbound request"
        );
    }

    let send_started = std::time::Instant::now();
    let response = client
        .get(base_url)
        .header("Accept", "application/json")
        .header("Accept-Encoding", "gzip")
        .header("X-Subscription-Token", api_key)
        .query(&query_params)
        .send()
        .await?;

    if http_diag {
        debug!(
            target: "brave.http",
            base_url = base_url,
            status = %response.status(),
            duration_ms = send_started.elapsed().as_millis() as u64,
            "brave.http: response"
        );
    }

    if !response.status().is_success() {
        return Ok(ToolResult::error(format!(
            "Search API returned status {}",
            response.status()
        )));
    }

    let body: BraveSearchResponse = response.json().await?;

    let results: Vec<serde_json::Value> = body
        .web
        .map(|w| {
            w.results
                .into_iter()
                .take(max_results)
                .map(|r| {
                    serde_json::json!({
                        "title": r.title,
                        "url": r.url,
                        "description": r.description
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(ToolResult::json(serde_json::json!({
        "results": results,
        "query": query
    })))
}

// ============================================================================
// Perplexity Search
// ============================================================================

async fn search_perplexity(
    query: &str,
    context: &ToolContext,
    freshness: Option<&str>,
) -> Result<ToolResult> {
    let search_config = context.config.tools.web.search.as_ref();
    let pplx_config = search_config.and_then(|s| s.perplexity.as_ref());

    // Resolve API key: config → PERPLEXITY_API_KEY → OPENROUTER_API_KEY
    let env_pplx_key = std::env::var("PERPLEXITY_API_KEY").ok();
    let env_openrouter_key = std::env::var("OPENROUTER_API_KEY").ok();
    let api_key = pplx_config
        .and_then(|c| c.api_key.as_deref())
        .or_else(|| env_pplx_key.as_deref())
        .or_else(|| env_openrouter_key.as_deref());

    let api_key = match api_key {
        Some(k) if !k.is_empty() => k,
        _ => return Ok(ToolResult::error("No Perplexity API key configured")),
    };

    // Infer base URL from key prefix
    let base_url = pplx_config
        .and_then(|c| c.base_url.as_deref())
        .unwrap_or_else(|| {
            if api_key.starts_with("sk-or-") {
                "https://openrouter.ai/v1"
            } else {
                "https://api.perplexity.ai"
            }
        });

    // Default model
    let mut model = pplx_config
        .and_then(|c| c.model.as_deref())
        .unwrap_or("sonar-pro")
        .to_string();

    // Strip perplexity/ prefix for direct API
    if !api_key.starts_with("sk-or-") {
        if let Some(stripped) = model.strip_prefix("perplexity/") {
            model = stripped.to_string();
        }
    }

    // Map freshness to Perplexity's search_recency_filter
    let recency_filter = freshness.map(|f| match f {
        "pd" => "day".to_string(),
        "pw" => "week".to_string(),
        "pm" => "month".to_string(),
        "py" => "year".to_string(),
        other => other.to_string(),
    });

    let body = PerplexityRequest {
        model,
        messages: vec![PerplexityMessage {
            role: "user".to_string(),
            content: query.to_string(),
        }],
        search_recency_filter: recency_filter,
    };

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/chat/completions", base_url))
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Ok(ToolResult::error(format!(
            "Perplexity API error ({}): {}",
            status, text
        )));
    }

    let resp: PerplexityResponse = response.json().await?;

    let content = resp
        .choices
        .first()
        .map(|c| c.message.content.clone())
        .unwrap_or_default();

    let citations: Vec<serde_json::Value> = resp
        .citations
        .into_iter()
        .map(|url| serde_json::json!({ "url": url }))
        .collect();

    Ok(ToolResult::json(serde_json::json!({
        "content": content,
        "citations": citations,
        "query": query,
        "provider": "perplexity"
    })))
}

// ============================================================================
// Grok / xAI Search
// ============================================================================

async fn search_grok(query: &str, context: &ToolContext) -> Result<ToolResult> {
    let search_config = context.config.tools.web.search.as_ref();
    let grok_config = search_config.and_then(|s| s.grok.as_ref());

    // Resolve API key: config → XAI_API_KEY
    let env_key = std::env::var("XAI_API_KEY").ok();
    let api_key = grok_config
        .and_then(|c| c.api_key.as_deref())
        .or_else(|| env_key.as_deref());

    let api_key = match api_key {
        Some(k) if !k.is_empty() => k,
        _ => return Ok(ToolResult::error("No xAI API key configured")),
    };

    let model = grok_config
        .and_then(|c| c.model.as_deref())
        .unwrap_or("grok-4-1-fast");

    let body = GrokRequest {
        model: model.to_string(),
        input: vec![GrokInput {
            role: "user".to_string(),
            content: query.to_string(),
        }],
        tools: vec![GrokTool {
            tool_type: "web_search".to_string(),
        }],
    };

    // v2026.5.2: 60s default timeout (configurable). Historical no-timeout
    // builds let slow xAI Responses API calls hang the tool loop.
    let timeout_secs = grok_config
        .and_then(|c| c.timeout_seconds)
        .unwrap_or(GROK_WEB_SEARCH_DEFAULT_TIMEOUT_SECS);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()?;
    let send_result = client
        .post("https://api.x.ai/v1/responses")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await;

    let response = match send_result {
        Ok(r) => r,
        Err(e) if e.is_timeout() => {
            // v2026.5.2: structured timeout error instead of aborting tool call.
            return Ok(ToolResult::json(serde_json::json!({
                "error": "timeout",
                "provider": "grok",
                "query": query,
                "timeout_seconds": timeout_secs,
                "message": format!("xAI Grok web_search timed out after {timeout_secs}s")
            })));
        }
        Err(e) => return Err(e.into()),
    };

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Ok(ToolResult::error(format!(
            "xAI API error ({}): {}",
            status, text
        )));
    }

    let resp: GrokResponse = response.json().await?;

    let mut text_parts = Vec::new();
    let mut citations = Vec::new();

    for output in &resp.output {
        if output.output_type == "message" {
            for content in &output.content {
                if content.content_type == "output_text" {
                    if let Some(ref text) = content.text {
                        text_parts.push(text.clone());
                    }
                }
                if let Some(ref annotations) = content.annotations {
                    for ann in annotations {
                        if let Some(ref url) = ann.url {
                            citations.push(serde_json::json!({
                                "url": url,
                                "title": ann.title
                            }));
                        }
                    }
                }
            }
        }
    }

    Ok(ToolResult::json(serde_json::json!({
        "content": text_parts.join("\n"),
        "citations": citations,
        "query": query,
        "provider": "grok"
    })))
}

// ============================================================================
// SearXNG Search (v2026.4.1)
// ============================================================================

/// SearXNG web search provider (v2026.4.1).
async fn searxng_search(query: &str, config: &crate::config::Config) -> Result<ToolResult> {
    let searxng_config = config.tools.web.search.as_ref()
        .and_then(|s| s.searxng.as_ref());

    let host = searxng_config
        .and_then(|c| c.host.as_deref())
        .unwrap_or("http://localhost:8888");

    let max_results = searxng_config
        .and_then(|c| c.max_results)
        .unwrap_or(10);

    let timeout_secs = searxng_config
        .and_then(|c| c.timeout_seconds)
        .unwrap_or(10);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()?;

    let mut params: Vec<(&str, String)> = vec![
        ("q", query.to_string()),
        ("format", "json".to_string()),
    ];

    if let Some(engines) = searxng_config.and_then(|c| c.engines.as_ref()) {
        params.push(("engines", engines.join(",")));
    }
    if let Some(lang) = searxng_config.and_then(|c| c.language.as_deref()) {
        params.push(("language", lang.to_string()));
    }

    let response = client
        .get(&format!("{}/search", host))
        .query(&params)
        .send()
        .await?;

    if !response.status().is_success() {
        return Ok(ToolResult::error(format!(
            "SearXNG returned status {}",
            response.status()
        )));
    }

    let resp: serde_json::Value = response.json().await?;

    let results: Vec<SearchResult> = resp["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .take(max_results as usize)
                .filter_map(|r| {
                    Some(SearchResult {
                        title: r["title"].as_str()?.to_string(),
                        url: r["url"].as_str()?.to_string(),
                        snippet: r["content"].as_str().unwrap_or("").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let output: Vec<serde_json::Value> = results
        .into_iter()
        .map(|r| serde_json::json!({ "title": r.title, "url": r.url, "snippet": r.snippet }))
        .collect();

    Ok(ToolResult::json(serde_json::json!({
        "results": output,
        "query": query,
        "provider": "searxng"
    })))
}

// ============================================================================
// X (Twitter) Search via xAI Grok (v2026.4.1)
// ============================================================================

/// X (Twitter) search via xAI Grok (v2026.4.1).
async fn x_search(query: &str, config: &crate::config::Config) -> Result<ToolResult> {
    let x_config = config.tools.web.search.as_ref()
        .and_then(|s| s.x_search.as_ref());

    let api_key = x_config
        .and_then(|c| c.api_key.clone())
        .or_else(|| std::env::var("XAI_API_KEY").ok())
        .ok_or_else(|| anyhow::anyhow!("No xAI API key for X search"))?;

    let model = x_config
        .and_then(|c| c.model.as_deref())
        .unwrap_or("grok-3");

    let max_results = x_config
        .and_then(|c| c.max_results)
        .unwrap_or(10);

    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.x.ai/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": model,
            "messages": [{
                "role": "user",
                "content": format!("Search X/Twitter for: {}", query)
            }],
            "search": true,
            "max_tokens": 1024
        }))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Ok(ToolResult::error(format!(
            "xAI X search API error ({}): {}",
            status, text
        )));
    }

    let body: serde_json::Value = resp.json().await?;

    let content = body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("");

    // Encode query for URL — use percent-encoding via the url crate
    let encoded_query: String = query
        .chars()
        .flat_map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '~' {
                vec![c]
            } else {
                format!("%{:02X}", c as u32).chars().collect()
            }
        })
        .collect();

    debug!("X search max_results config: {}", max_results);

    let result = SearchResult {
        title: format!("X search: {}", query),
        url: format!("https://x.com/search?q={}", encoded_query),
        snippet: content.chars().take(500).collect(),
    };

    Ok(ToolResult::json(serde_json::json!({
        "results": [{
            "title": result.title,
            "url": result.url,
            "snippet": result.snippet
        }],
        "query": query,
        "provider": "x_search"
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grok_web_search_default_timeout_is_60s() {
        // v2026.5.2 default. The structured timeout error path keys off this
        // value, so any future change must update both the helper text and
        // the consumer expectation.
        assert_eq!(GROK_WEB_SEARCH_DEFAULT_TIMEOUT_SECS, 60);
    }

    #[test]
    fn brave_default_base_url_points_to_brave_api() {
        assert_eq!(
            BRAVE_DEFAULT_BASE_URL,
            "https://api.search.brave.com/res/v1/web/search"
        );
    }

    #[tokio::test]
    async fn search_brave_uses_provided_base_url() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/res/v1/web/search"))
            .and(query_param("q", "rustlang"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "web": {
                    "results": [{
                        "title": "Rust Programming Language",
                        "url": "https://www.rust-lang.org",
                        "description": "Empowering everyone to build reliable and efficient software."
                    }]
                }
            })))
            .mount(&server)
            .await;

        let base = format!("{}/res/v1/web/search", server.uri());
        let result = search_brave("rustlang", 5, "fake-key", None, &base, false)
            .await
            .expect("brave search ok");

        let payload = result.json.expect("brave search returns json");
        let body = payload.to_string();
        assert!(body.contains("Rust Programming Language"), "body: {body}");
        assert!(body.contains("rust-lang.org"));
    }

    #[tokio::test]
    async fn search_brave_diagnostics_flag_does_not_alter_response() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/res/v1/web/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "web": {"results": []}
            })))
            .mount(&server)
            .await;

        let base = format!("{}/res/v1/web/search", server.uri());
        let off = search_brave("q", 5, "k", None, &base, false).await.unwrap();
        let on = search_brave("q", 5, "k", None, &base, true).await.unwrap();

        // The diagnostic flag is a logging-only side effect; the JSON
        // payload returned must be identical so plugins/agents observing
        // the result aren't sensitive to operator log preferences.
        assert_eq!(off.json, on.json);
    }
}
