//! Memory store and search tools.

use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use async_trait::async_trait;
use tokio::io::AsyncWriteExt;

/// Memory store tool — persist content into long-term memory.
pub struct MemoryStoreTool;

#[async_trait]
impl AgentTool for MemoryStoreTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "memory_store".to_string(),
            description: "Store information in long-term memory for later retrieval".to_string(),
            category: "memory".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "Content to store in memory"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional tags for categorisation"
                    }
                },
                "required": ["content"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult> {
        let content = params
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing content parameter"))?;

        let tags: Vec<String> = params
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        // Write to the memory file (daily log pattern matching OpenClaw)
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let memory_dir = context.config.state_dir.join("memory");
        let _ = tokio::fs::create_dir_all(&memory_dir).await;

        let memory_file = memory_dir.join(format!("{}.md", date));
        let timestamp = chrono::Local::now().format("%H:%M:%S").to_string();

        let tag_str = if tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", tags.join(", "))
        };

        let entry = format!("\n## {}{}\n\n{}\n", timestamp, tag_str, content);

        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&memory_file)
            .await?
            .write_all(entry.as_bytes())
            .await?;

        tracing::info!(
            tags = ?tags,
            file = %memory_file.display(),
            "stored memory entry"
        );

        Ok(ToolResult::json(serde_json::json!({
            "stored": true,
            "file": memory_file.display().to_string(),
            "timestamp": timestamp,
            "tags": tags,
            "chars": content.len()
        })))
    }
}

/// Memory search tool — hybrid BM25 + vector retrieval.
pub struct MemorySearchTool;

#[async_trait]
impl AgentTool for MemorySearchTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "memory_search".to_string(),
            description: "Search long-term memory using hybrid BM25 + semantic search".to_string(),
            category: "memory".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Natural language search query"
                    },
                    "maxResults": {
                        "type": "integer",
                        "description": "Maximum results to return",
                        "default": 10
                    },
                    "minScore": {
                        "type": "number",
                        "description": "Minimum relevance score (0.0-1.0)",
                        "default": 0.0
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
            .unwrap_or(10) as u32;

        let min_score = params
            .get("minScore")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);

        let results = crate::memory::search(
            &context.config,
            query,
            max_results,
            min_score,
            Some(&context.session_key),
        )
        .await?;

        let result_json: Vec<serde_json::Value> = results
            .iter()
            .map(|r| {
                serde_json::json!({
                    "text": r.text,
                    "path": r.path,
                    "score": r.score,
                    "startLine": r.start_line,
                    "endLine": r.end_line
                })
            })
            .collect();

        Ok(ToolResult::json(serde_json::json!({
            "results": result_json,
            "count": results.len(),
            "query": query
        })))
    }
}
