mod bash;
mod browser_tool;
mod canvas;
mod common;
mod cron_tool;
mod discord_actions;
mod image_tool;
mod memory_tool;
mod message_tool;
mod sessions_tool;
mod slack_actions;
mod telegram_actions;
mod tts_tool;
mod web_fetch;
mod web_search;
mod whatsapp_actions;

pub use common::*;

use crate::config::Config;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ============================================================================
// Tool System
// ============================================================================

/// Information about an available tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub category: String,
    pub hidden: bool,
    pub input_schema: serde_json::Value,
}

/// Result from executing a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ToolImageResult>,
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolImageResult {
    pub label: String,
    pub path: String,
    pub base64: String,
    pub mime_type: String,
}

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            json: None,
            image: None,
            is_error: false,
        }
    }

    pub fn json(value: serde_json::Value) -> Self {
        Self {
            text: None,
            json: Some(value),
            image: None,
            is_error: false,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            text: Some(message.into()),
            json: None,
            image: None,
            is_error: true,
        }
    }
}

/// Trait for tool execution.
#[async_trait::async_trait]
pub trait AgentTool: Send + Sync {
    fn info(&self) -> ToolInfo;
    async fn execute(
        &self,
        params: serde_json::Value,
        context: &ToolContext,
    ) -> anyhow::Result<ToolResult>;
}

/// Context provided to tools during execution.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub session_key: String,
    pub agent_id: String,
    pub config: Config,
}

/// List all available tools based on configuration.
pub fn list_available_tools(config: &Config) -> Vec<ToolInfo> {
    let mut tools = Vec::new();

    // Web tools
    tools.push(ToolInfo {
        name: "web.fetch".to_string(),
        description: "Fetch content from a URL with SSRF protection".to_string(),
        category: "web".to_string(),
        hidden: false,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "URL to fetch" },
                "method": { "type": "string", "enum": ["GET", "POST"], "default": "GET" },
                "headers": { "type": "object", "description": "Request headers" },
                "body": { "type": "string", "description": "Request body" }
            },
            "required": ["url"]
        }),
    });

    tools.push(ToolInfo {
        name: "web.search".to_string(),
        description: "Search the web using a search engine".to_string(),
        category: "web".to_string(),
        hidden: false,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "maxResults": { "type": "integer", "description": "Maximum results" }
            },
            "required": ["query"]
        }),
    });

    // Browser tools
    if config.browser.enabled {
        for tool in browser_tool::browser_tools() {
            tools.push(tool);
        }
    }

    // Memory tools
    tools.push(ToolInfo {
        name: "memory.store".to_string(),
        description: "Store information in long-term memory".to_string(),
        category: "memory".to_string(),
        hidden: false,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "Content to remember" },
                "tags": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["content"]
        }),
    });

    tools.push(ToolInfo {
        name: "memory.search".to_string(),
        description: "Search long-term memory using RAG".to_string(),
        category: "memory".to_string(),
        hidden: false,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "maxResults": { "type": "integer", "default": 10 }
            },
            "required": ["query"]
        }),
    });

    // Bash tool
    tools.push(ToolInfo {
        name: "system.run".to_string(),
        description: "Execute a shell command".to_string(),
        category: "system".to_string(),
        hidden: false,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to execute" },
                "cwd": { "type": "string", "description": "Working directory" },
                "timeout": { "type": "integer", "description": "Timeout in seconds" }
            },
            "required": ["command"]
        }),
    });

    // Session tools
    tools.push(ToolInfo {
        name: "sessions.list".to_string(),
        description: "List active sessions".to_string(),
        category: "sessions".to_string(),
        hidden: false,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {}
        }),
    });

    tools.push(ToolInfo {
        name: "sessions.history".to_string(),
        description: "Get session transcript history".to_string(),
        category: "sessions".to_string(),
        hidden: false,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "sessionKey": { "type": "string" },
                "limit": { "type": "integer" }
            },
            "required": ["sessionKey"]
        }),
    });

    tools.push(ToolInfo {
        name: "sessions.send".to_string(),
        description: "Send a message to another session".to_string(),
        category: "sessions".to_string(),
        hidden: false,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "sessionKey": { "type": "string" },
                "message": { "type": "string" }
            },
            "required": ["sessionKey", "message"]
        }),
    });

    tools.push(ToolInfo {
        name: "sessions.spawn".to_string(),
        description: "Create a new session".to_string(),
        category: "sessions".to_string(),
        hidden: false,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "message": { "type": "string" },
                "model": { "type": "string" }
            },
            "required": ["message"]
        }),
    });

    // Message tool
    tools.push(ToolInfo {
        name: "message.send".to_string(),
        description: "Send a formatted message to a channel".to_string(),
        category: "messaging".to_string(),
        hidden: false,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "channel": { "type": "string" },
                "to": { "type": "string" },
                "text": { "type": "string" },
                "format": { "type": "string", "enum": ["text", "markdown", "html"] }
            },
            "required": ["channel", "to", "text"]
        }),
    });

    // Channel action tools
    tools.push(ToolInfo {
        name: "discord.send".to_string(),
        description: "Send a message via Discord".to_string(),
        category: "discord".to_string(),
        hidden: false,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "channelId": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["channelId", "content"]
        }),
    });

    tools.push(ToolInfo {
        name: "telegram.send".to_string(),
        description: "Send a message via Telegram".to_string(),
        category: "telegram".to_string(),
        hidden: false,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "chatId": { "type": "string" },
                "text": { "type": "string" }
            },
            "required": ["chatId", "text"]
        }),
    });

    tools.push(ToolInfo {
        name: "slack.send".to_string(),
        description: "Send a message via Slack".to_string(),
        category: "slack".to_string(),
        hidden: false,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "channel": { "type": "string" },
                "text": { "type": "string" }
            },
            "required": ["channel", "text"]
        }),
    });

    tools.push(ToolInfo {
        name: "whatsapp.send".to_string(),
        description: "Send a message via WhatsApp".to_string(),
        category: "whatsapp".to_string(),
        hidden: false,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "to": { "type": "string" },
                "text": { "type": "string" }
            },
            "required": ["to", "text"]
        }),
    });

    // Image tool
    tools.push(ToolInfo {
        name: "image.generate".to_string(),
        description: "Generate an image from text".to_string(),
        category: "media".to_string(),
        hidden: false,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string" },
                "model": { "type": "string" }
            },
            "required": ["prompt"]
        }),
    });

    // TTS tool
    tools.push(ToolInfo {
        name: "tts.speak".to_string(),
        description: "Convert text to speech".to_string(),
        category: "media".to_string(),
        hidden: false,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string" },
                "voice": { "type": "string" }
            },
            "required": ["text"]
        }),
    });

    // Cron tool
    tools.push(ToolInfo {
        name: "cron.schedule".to_string(),
        description: "Schedule a recurring job".to_string(),
        category: "system".to_string(),
        hidden: false,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "expression": { "type": "string", "description": "Cron expression" },
                "message": { "type": "string" },
                "name": { "type": "string" }
            },
            "required": ["expression", "message"]
        }),
    });

    // Canvas tool
    tools.push(ToolInfo {
        name: "canvas.render".to_string(),
        description: "Render a visual canvas".to_string(),
        category: "media".to_string(),
        hidden: false,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "content": { "type": "string" },
                "format": { "type": "string" }
            },
            "required": ["content"]
        }),
    });

    // Gateway tool
    tools.push(ToolInfo {
        name: "gateway.invoke".to_string(),
        description: "Invoke a gateway method".to_string(),
        category: "system".to_string(),
        hidden: true,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "method": { "type": "string" },
                "params": { "type": "object" }
            },
            "required": ["method"]
        }),
    });

    // Agents tool
    tools.push(ToolInfo {
        name: "agents.list".to_string(),
        description: "List available agents".to_string(),
        category: "system".to_string(),
        hidden: false,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {}
        }),
    });

    // Filter by tool policy
    filter_tools_by_policy(tools, config)
}

/// Filter tools based on configuration policy.
fn filter_tools_by_policy(tools: Vec<ToolInfo>, config: &Config) -> Vec<ToolInfo> {
    let allow = &config.tools.allow;
    let deny = &config.tools.deny;

    tools
        .into_iter()
        .filter(|t| {
            if !deny.is_empty() && deny.iter().any(|d| t.name.starts_with(d) || d == &t.name) {
                return false;
            }
            if !allow.is_empty() {
                return allow.iter().any(|a| t.name.starts_with(a) || a == &t.name);
            }
            true
        })
        .collect()
}
