use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::debug;

/// Web search tool using Brave Search or other providers.
pub struct WebSearchTool;

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

#[async_trait::async_trait]
impl AgentTool for WebSearchTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "web.search".to_string(),
            description: "Search the web using a search engine".to_string(),
            category: "web".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "maxResults": { "type": "integer", "default": 10 }
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

        let provider = context
            .config
            .tools
            .web
            .search
            .as_ref()
            .and_then(|s| s.provider.as_deref())
            .unwrap_or("brave");

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
            return Ok(ToolResult::error("No search API key configured"));
        }

        match provider {
            "brave" => search_brave(query, max_results, api_key).await,
            _ => Ok(ToolResult::error(format!(
                "Unknown search provider: {}",
                provider
            ))),
        }
    }
}

async fn search_brave(query: &str, max_results: usize, api_key: &str) -> Result<ToolResult> {
    let client = reqwest::Client::new();

    let response = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("Accept", "application/json")
        .header("Accept-Encoding", "gzip")
        .header("X-Subscription-Token", api_key)
        .query(&[("q", query), ("count", &max_results.to_string())])
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
