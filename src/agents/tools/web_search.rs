use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::debug;

/// Web search tool supporting Brave, Perplexity, and Grok (xAI) providers.
pub struct WebSearchTool;

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

                search_brave(query, max_results, api_key, freshness.as_deref()).await
            }
            "perplexity" => {
                search_perplexity(query, context, freshness.as_deref()).await
            }
            "grok" => {
                search_grok(query, context).await
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
) -> Result<ToolResult> {
    let client = reqwest::Client::new();

    let mut query_params = vec![
        ("q".to_string(), query.to_string()),
        ("count".to_string(), max_results.to_string()),
    ];

    if let Some(f) = freshness {
        query_params.push(("freshness".to_string(), f.to_string()));
    }

    let response = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("Accept", "application/json")
        .header("Accept-Encoding", "gzip")
        .header("X-Subscription-Token", api_key)
        .query(&query_params)
        .send()
        .await?;

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

    let client = reqwest::Client::new();
    let response = client
        .post("https://api.x.ai/v1/responses")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

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
