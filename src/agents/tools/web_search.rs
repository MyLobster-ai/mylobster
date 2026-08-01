use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::debug;

// v2026.7.1 provider submodules (files live under `web_search/`).
pub mod brave;
pub mod cache;
pub mod common;
pub mod duckduckgo;
pub mod exa;
pub mod firecrawl;
pub mod gemini;
pub mod minimax_search;
pub mod parallel;
pub mod searxng;

/// Shared search result type used by the X search provider.
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

/// Legacy full Brave web-search endpoint URL (pre-v2026.7.1 config default).
/// The v2026.7.1 Brave provider works from the origin base URL and appends
/// endpoint paths — see [`brave::resolve_brave_base_url`], which reduces this
/// legacy form back to the origin.
pub const BRAVE_DEFAULT_BASE_URL: &str = "https://api.search.brave.com/res/v1/web/search";

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
                    },
                    "date_after": {
                        "type": "string",
                        "description": "Only include results published after this date (YYYY-MM-DD). Cannot be combined with freshness."
                    },
                    "date_before": {
                        "type": "string",
                        "description": "Only include results published before this date (YYYY-MM-DD). Cannot be combined with freshness."
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
        let date_after = params
            .get("date_after")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let date_before = params
            .get("date_before")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let search_cfg = context.config.tools.web.search.as_ref();
        let provider = search_cfg
            .and_then(|s| s.provider.as_deref())
            .unwrap_or("brave");

        let timeout_seconds =
            common::resolve_search_timeout_seconds(search_cfg.and_then(|s| s.timeout_seconds));
        let cache_ttl_ms =
            common::resolve_search_cache_ttl_ms(search_cfg.and_then(|s| s.cache_ttl_minutes));

        match provider {
            "brave" => {
                let env_api_key = std::env::var("BRAVE_API_KEY").ok();
                let brave_cfg = search_cfg.and_then(|s| s.brave.as_ref());
                // v2026.7.1: the Brave-scoped key wins over the legacy
                // top-level `tools.web.search.apiKey` (upstream migrated the
                // Brave key into the provider-scoped plugin entry).
                let api_key = brave_cfg
                    .and_then(|b| b.api_key.as_deref())
                    .or_else(|| search_cfg.and_then(|s| s.api_key.as_deref()))
                    .or_else(|| env_api_key.as_deref())
                    .unwrap_or("");
                let payload = brave::execute_brave_search(brave::BraveSearchRequest {
                    query,
                    count: Some(max_results as u64),
                    api_key,
                    base_url: brave_cfg.and_then(|b| b.base_url.as_deref()),
                    mode: brave_cfg.and_then(|b| b.mode.as_deref()),
                    freshness: freshness.as_deref(),
                    date_after: date_after.as_deref(),
                    date_before: date_before.as_deref(),
                    country: params.get("country").and_then(|v| v.as_str()),
                    search_lang: params.get("search_lang").and_then(|v| v.as_str()),
                    ui_lang: params.get("ui_lang").and_then(|v| v.as_str()),
                    timeout_seconds,
                    cache_ttl_ms,
                    http_diag: brave_cfg.and_then(|b| b.http).unwrap_or(false),
                })
                .await?;
                Ok(ToolResult::json(payload))
            }
            "perplexity" => {
                search_perplexity(query, context, freshness.as_deref()).await
            }
            "grok" => {
                search_grok(query, context).await
            }
            // v2026.4.1: SearXNG bundled web search provider
            "searxng" => {
                let searxng_cfg = search_cfg.and_then(|s| s.searxng.as_ref());
                let env_base = std::env::var("SEARXNG_BASE_URL").ok();
                let base_url = searxng_cfg
                    .and_then(|c| c.host.as_deref())
                    .or(env_base.as_deref())
                    .unwrap_or("http://localhost:8888");
                let categories = params
                    .get("categories")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| searxng_cfg.and_then(|c| c.categories.as_ref()).map(|c| c.join(",")));
                let engines = searxng_cfg
                    .and_then(|c| c.engines.as_ref())
                    .map(|e| e.join(","));
                let payload = searxng::run_searxng_search(searxng::SearxngSearchRequest {
                    query,
                    count: Some(
                        searxng_cfg
                            .and_then(|c| c.max_results)
                            .map(|m| m as u64)
                            .unwrap_or(max_results as u64),
                    ),
                    base_url,
                    categories: categories.as_deref(),
                    language: searxng_cfg.and_then(|c| c.language.as_deref()),
                    engines: engines.as_deref(),
                    timeout_seconds: searxng_cfg
                        .and_then(|c| c.timeout_seconds)
                        .unwrap_or(searxng::SEARXNG_DEFAULT_TIMEOUT_SECONDS),
                    cache_ttl_ms,
                })
                .await?;
                Ok(ToolResult::json(payload))
            }
            // v2026.7.1: Exa search with baseUrl override
            "exa" => {
                let exa_cfg = search_cfg.and_then(|s| s.exa.as_ref());
                let env_key = std::env::var("EXA_API_KEY").ok();
                let api_key = exa_cfg
                    .and_then(|c| c.api_key.as_deref())
                    .or(env_key.as_deref())
                    .unwrap_or("");
                let payload = exa::execute_exa_search(exa::ExaSearchRequest {
                    query,
                    count: Some(max_results as u64),
                    search_type: params.get("type").and_then(|v| v.as_str()),
                    freshness: freshness.as_deref(),
                    date_after: date_after.as_deref(),
                    date_before: date_before.as_deref(),
                    contents: params.get("contents"),
                    api_key,
                    base_url: exa_cfg.and_then(|c| c.base_url.as_deref()),
                    timeout_seconds,
                    cache_ttl_ms,
                })
                .await?;
                Ok(ToolResult::json(payload))
            }
            // v2026.7.1: MiniMax Coding Plan search
            "minimax" => {
                let minimax_cfg = search_cfg.and_then(|s| s.minimax.as_ref());
                let api_key = minimax_search::resolve_minimax_api_key(
                    minimax_cfg
                        .and_then(|c| c.api_key.as_deref())
                        .or(search_cfg.and_then(|s| s.api_key.as_deref())),
                    |var| std::env::var(var).ok(),
                )
                .unwrap_or_default();
                let providers = &context.config.models.providers;
                let region = minimax_search::resolve_minimax_region(
                    minimax_cfg.and_then(|c| c.region.as_deref()),
                    std::env::var("MINIMAX_API_HOST").ok().as_deref(),
                    providers.get("minimax").map(|p| p.base_url.as_str()),
                    providers.get("minimax-portal").map(|p| p.base_url.as_str()),
                );
                let payload = minimax_search::execute_minimax_search(
                    minimax_search::MiniMaxSearchRequest {
                        query,
                        count: Some(max_results as u64),
                        api_key: &api_key,
                        endpoint: minimax_search::resolve_minimax_endpoint(region),
                        timeout_seconds,
                        cache_ttl_ms,
                    },
                )
                .await?;
                Ok(ToolResult::json(payload))
            }
            // v2026.7.1: Firecrawl search (base URL restricted to hosted or
            // explicitly self-hosted private endpoints)
            "firecrawl" => {
                let firecrawl_cfg = context
                    .config
                    .tools
                    .web
                    .fetch
                    .as_ref()
                    .and_then(|f| f.firecrawl.as_ref());
                let env_key = std::env::var("FIRECRAWL_API_KEY").ok();
                let api_key = firecrawl_cfg
                    .and_then(|c| c.api_key.as_deref())
                    .or(env_key.as_deref())
                    .unwrap_or("");
                let payload = firecrawl::run_firecrawl_search(firecrawl::FirecrawlSearchRequest {
                    query,
                    count: Some(max_results as u64),
                    api_key,
                    base_url: firecrawl_cfg.and_then(|c| c.base_url.as_deref()),
                    timeout_seconds,
                    cache_ttl_ms,
                })
                .await?;
                Ok(ToolResult::json(payload))
            }
            // v2026.7.1: Gemini grounding search with Google-provider fallback
            "gemini" | "google" => {
                let gemini_cfg = search_cfg.and_then(|s| s.gemini.as_ref());
                let google_provider = context.config.models.providers.get("google");
                let env_key = std::env::var("GEMINI_API_KEY").ok();
                let api_key = gemini::resolve_gemini_search_api_key(
                    gemini_cfg.and_then(|c| c.api_key.as_deref()),
                    env_key.as_deref(),
                    google_provider.and_then(|p| p.api_key.as_deref()),
                )
                .unwrap_or_default();
                let base_url = gemini::resolve_gemini_search_base_url(
                    gemini_cfg.and_then(|c| c.base_url.as_deref()),
                    google_provider.map(|p| p.base_url.as_str()),
                );
                let payload = gemini::execute_gemini_search(gemini::GeminiSearchRequest {
                    query,
                    count: Some(max_results as u64),
                    freshness: freshness.as_deref(),
                    date_after: date_after.as_deref(),
                    date_before: date_before.as_deref(),
                    api_key: &api_key,
                    base_url: &base_url,
                    model: gemini_cfg
                        .and_then(|c| c.model.as_deref())
                        .unwrap_or(gemini::DEFAULT_GEMINI_WEB_SEARCH_MODEL),
                    timeout_seconds,
                    cache_ttl_ms,
                })
                .await?;
                Ok(ToolResult::json(payload))
            }
            // v2026.7.1: bundled Parallel provider (api.parallel.ai/v1/search)
            "parallel" => {
                let parallel_cfg = search_cfg.and_then(|s| s.parallel.as_ref());
                let env_key = std::env::var("PARALLEL_API_KEY").ok();
                let api_key = parallel_cfg
                    .and_then(|c| c.api_key.as_deref())
                    .or(env_key.as_deref())
                    .unwrap_or("");
                let payload = parallel::execute_parallel_search(parallel::ParallelSearchRequest {
                    query: Some(query),
                    objective: params.get("objective").and_then(|v| v.as_str()),
                    search_queries: params.get("search_queries"),
                    count: Some(max_results as u64),
                    session_id: params.get("session_id").and_then(|v| v.as_str()),
                    client_model: params.get("client_model").and_then(|v| v.as_str()),
                    api_key,
                    base_url: parallel_cfg.and_then(|c| c.base_url.as_deref()),
                    timeout_seconds,
                    cache_ttl_ms,
                })
                .await?;
                Ok(ToolResult::json(payload))
            }
            // v2026.7.1: DuckDuckGo key-free provider — explicit opt-in only
            // (never auto-selected; the default provider stays Brave).
            "duckduckgo" | "ddg" => {
                let ddg_cfg = search_cfg.and_then(|s| s.duckduckgo.as_ref());
                let payload = duckduckgo::run_duckduckgo_search(
                    duckduckgo::DuckDuckGoSearchRequest {
                        query,
                        count: Some(max_results as u64),
                        region: ddg_cfg.and_then(|c| c.region.as_deref()),
                        safe_search: ddg_cfg.and_then(|c| c.safe_search.as_deref()),
                        timeout_seconds: duckduckgo::DDG_DEFAULT_TIMEOUT_SECONDS
                            .min(timeout_seconds),
                        cache_ttl_ms,
                        endpoint: None,
                    },
                )
                .await?;
                Ok(ToolResult::json(payload))
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

    // v2026.7.1: malformed-Responses parse hardening. Parse the body as an
    // untyped JSON value and extract text/citations defensively — null
    // entries, missing fields, and wrong-typed arrays must degrade to the
    // structured "malformed JSON response" error instead of a serde failure.
    let resp: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(_) => {
            return Ok(ToolResult::error(
                "xAI Grok web_search: malformed JSON response",
            ))
        }
    };

    let (content, citations) = match extract_xai_web_search_content(&resp) {
        Some(extracted) => extracted,
        None => {
            return Ok(ToolResult::error(
                "xAI Grok web_search: malformed JSON response",
            ))
        }
    };

    Ok(ToolResult::json(serde_json::json!({
        "content": content,
        "citations": citations,
        "query": query,
        "provider": "grok"
    })))
}

/// Extract `(text, citations)` from an xAI Responses payload, tolerating
/// malformed shapes (v2026.7.1 parity with upstream
/// `extractXaiWebSearchContent` / `requireXaiResponseTextAndCitations`).
///
/// Walks `output[]` for a `message` entry with an `output_text` block (or an
/// `output_text` entry directly), collecting `url_citation` annotations.
/// Falls back to top-level `output_text`; the top-level `citations` string
/// array, when non-empty, wins over annotation-derived citations. Returns
/// `None` when no usable text exists.
fn extract_xai_web_search_content(
    data: &serde_json::Value,
) -> Option<(String, Vec<serde_json::Value>)> {
    fn url_citations(annotations: Option<&serde_json::Value>) -> Vec<String> {
        let Some(arr) = annotations.and_then(|a| a.as_array()) else {
            return Vec::new();
        };
        let mut seen = std::collections::HashSet::new();
        arr.iter()
            .filter_map(|ann| {
                let ann = ann.as_object()?;
                if ann.get("type")?.as_str()? != "url_citation" {
                    return None;
                }
                let url = ann.get("url")?.as_str()?.to_string();
                seen.insert(url.clone()).then_some(url)
            })
            .collect()
    }

    let mut found: Option<(String, Vec<String>)> = None;
    if let Some(outputs) = data.get("output").and_then(|o| o.as_array()) {
        'outer: for output in outputs {
            let Some(output) = output.as_object() else {
                continue;
            };
            let output_type = output.get("type").and_then(|t| t.as_str());
            if output_type == Some("message") {
                if let Some(blocks) = output.get("content").and_then(|c| c.as_array()) {
                    for block in blocks {
                        let Some(block) = block.as_object() else {
                            continue;
                        };
                        if block.get("type").and_then(|t| t.as_str()) == Some("output_text") {
                            if let Some(text) =
                                block.get("text").and_then(|t| t.as_str()).filter(|t| !t.is_empty())
                            {
                                found = Some((
                                    text.to_string(),
                                    url_citations(block.get("annotations")),
                                ));
                                break 'outer;
                            }
                        }
                    }
                }
            } else if output_type == Some("output_text") {
                if let Some(text) =
                    output.get("text").and_then(|t| t.as_str()).filter(|t| !t.is_empty())
                {
                    found = Some((text.to_string(), url_citations(output.get("annotations"))));
                    break;
                }
            }
        }
    }

    let (text, annotation_citations) = match found {
        Some(f) => f,
        None => {
            let text = data
                .get("output_text")
                .and_then(|t| t.as_str())
                .filter(|t| !t.is_empty())?
                .to_string();
            (text, Vec::new())
        }
    };

    // Top-level `citations` (string array), when non-empty, wins over
    // annotation-derived citations.
    let top_level: Vec<String> = data
        .get("citations")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| c.as_str())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let urls = if top_level.is_empty() { annotation_citations } else { top_level };
    let citations = urls
        .into_iter()
        .map(|url| serde_json::json!({ "url": url }))
        .collect();
    Some((text, citations))
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

    // ---- xAI Responses parse hardening (v2026.7.1) -------------------------

    #[test]
    fn xai_extracts_message_output_text_with_annotations() {
        let data = serde_json::json!({
            "output": [
                null,
                {"type": "reasoning"},
                {"type": "message", "content": [
                    null,
                    {"type": "output_text", "text": "answer", "annotations": [
                        null,
                        {"type": "url_citation", "url": "https://a.example.com"},
                        {"type": "other", "url": "https://ignored.example.com"},
                        {"type": "url_citation", "url": "https://a.example.com"}
                    ]}
                ]}
            ]
        });
        let (text, citations) = extract_xai_web_search_content(&data).unwrap();
        assert_eq!(text, "answer");
        assert_eq!(citations.len(), 1, "duplicate + non-url_citation dropped");
        assert_eq!(citations[0]["url"], "https://a.example.com");
    }

    #[test]
    fn xai_falls_back_to_top_level_output_text() {
        let data = serde_json::json!({
            "output": "not-an-array",
            "output_text": "fallback text"
        });
        let (text, citations) = extract_xai_web_search_content(&data).unwrap();
        assert_eq!(text, "fallback text");
        assert!(citations.is_empty());
    }

    #[test]
    fn xai_top_level_citations_win_over_annotations() {
        let data = serde_json::json!({
            "output": [{"type": "output_text", "text": "t", "annotations": [
                {"type": "url_citation", "url": "https://ann.example.com"}
            ]}],
            "citations": ["https://top.example.com"]
        });
        let (_, citations) = extract_xai_web_search_content(&data).unwrap();
        assert_eq!(citations.len(), 1);
        assert_eq!(citations[0]["url"], "https://top.example.com");
    }

    #[test]
    fn xai_malformed_payloads_yield_none() {
        for data in [
            serde_json::json!({}),
            serde_json::json!({"output": []}),
            serde_json::json!({"output": [{"type": "message", "content": null}]}),
            serde_json::json!({"output": [{"type": "message", "content": [{"type": "output_text", "text": ""}]}]}),
            serde_json::json!({"output_text": 42}),
            serde_json::json!(null),
        ] {
            assert!(
                extract_xai_web_search_content(&data).is_none(),
                "expected None for {data}"
            );
        }
    }

    #[test]
    fn xai_tolerates_null_content_entries() {
        let data = serde_json::json!({
            "output": [{"type": "message", "content": [null, 17, {"type": "output_text", "text": "ok"}]}]
        });
        let (text, _) = extract_xai_web_search_content(&data).unwrap();
        assert_eq!(text, "ok");
    }
}
