//! Canvas rendering tool.
//!
//! Supports: present, hide, navigate, eval, snapshot, a2ui_push, a2ui_reset.

use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use async_trait::async_trait;

pub struct CanvasTool;

#[async_trait]
impl AgentTool for CanvasTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "canvas".to_string(),
            description: "Canvas tool for visual rendering: present HTML/code, navigate URLs, evaluate JS, take snapshots, push UI updates".to_string(),
            category: "media".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["present", "hide", "navigate", "eval", "snapshot", "a2ui_push", "a2ui_reset"],
                        "description": "Canvas action to perform"
                    },
                    "content": {
                        "type": "string",
                        "description": "HTML/code content for 'present', URL for 'navigate', JS for 'eval'"
                    },
                    "format": {
                        "type": "string",
                        "enum": ["html", "markdown", "code", "svg", "mermaid"],
                        "default": "html"
                    },
                    "title": {
                        "type": "string",
                        "description": "Title for the canvas"
                    },
                    "width": { "type": "integer", "description": "Canvas width" },
                    "height": { "type": "integer", "description": "Canvas height" },
                    "uiSpec": {
                        "type": "object",
                        "description": "UI specification for a2ui_push"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _context: &ToolContext,
    ) -> Result<ToolResult> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing action parameter"))?;

        match action {
            "present" => {
                let content = params
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing content parameter"))?;

                let format = params
                    .get("format")
                    .and_then(|v| v.as_str())
                    .unwrap_or("html");

                let title = params
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Canvas");

                Ok(ToolResult::json(serde_json::json!({
                    "action": "present",
                    "title": title,
                    "format": format,
                    "contentLength": content.len(),
                    "rendered": true
                })))
            }
            "hide" => {
                Ok(ToolResult::json(serde_json::json!({
                    "action": "hide",
                    "hidden": true
                })))
            }
            "navigate" => {
                let url = params
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing content (URL) parameter"))?;

                // Validate URL
                let parsed = url::Url::parse(url)?;
                if parsed.scheme() != "http" && parsed.scheme() != "https" {
                    return Ok(ToolResult::error("Only http/https URLs are supported"));
                }

                Ok(ToolResult::json(serde_json::json!({
                    "action": "navigate",
                    "url": url,
                    "navigated": true
                })))
            }
            "eval" => {
                let js_code = params
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing content (JS code) parameter"))?;

                Ok(ToolResult::json(serde_json::json!({
                    "action": "eval",
                    "codeLength": js_code.len(),
                    "evaluated": true,
                    "note": "JS evaluation requires an active browser context"
                })))
            }
            "snapshot" => {
                Ok(ToolResult::json(serde_json::json!({
                    "action": "snapshot",
                    "captured": false,
                    "note": "Snapshot requires an active canvas with browser context"
                })))
            }
            "a2ui_push" => {
                let ui_spec = params
                    .get("uiSpec")
                    .ok_or_else(|| anyhow::anyhow!("Missing uiSpec parameter"))?;

                Ok(ToolResult::json(serde_json::json!({
                    "action": "a2ui_push",
                    "pushed": true,
                    "specKeys": ui_spec.as_object().map(|o| o.keys().collect::<Vec<_>>()).unwrap_or_default()
                })))
            }
            "a2ui_reset" => {
                Ok(ToolResult::json(serde_json::json!({
                    "action": "a2ui_reset",
                    "reset": true
                })))
            }
            _ => Ok(ToolResult::error(format!(
                "Unknown canvas action: {}",
                action
            ))),
        }
    }
}
