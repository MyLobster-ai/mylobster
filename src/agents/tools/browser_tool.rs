use super::ToolInfo;

/// Browser automation tool.
pub struct BrowserTool;

/// Return tool definitions for all browser automation tools.
pub fn browser_tools() -> Vec<ToolInfo> {
    vec![
        ToolInfo {
            name: "browser_navigate".to_string(),
            description: "Navigate the browser to a URL".to_string(),
            category: "browser".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "URL to navigate to" }
                },
                "required": ["url"]
            }),
        },
        ToolInfo {
            name: "browser_click".to_string(),
            description: "Click an element on the page".to_string(),
            category: "browser".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector of the element to click" }
                },
                "required": ["selector"]
            }),
        },
        ToolInfo {
            name: "browser_type".to_string(),
            description: "Type text into an input element".to_string(),
            category: "browser".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector of the input element" },
                    "text": { "type": "string", "description": "Text to type" }
                },
                "required": ["selector", "text"]
            }),
        },
        ToolInfo {
            name: "browser_screenshot".to_string(),
            description: "Take a screenshot of the current page".to_string(),
            category: "browser".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "fullPage": { "type": "boolean", "description": "Capture the full page", "default": false }
                }
            }),
        },
        ToolInfo {
            name: "browser_evaluate".to_string(),
            description: "Evaluate JavaScript in the browser context".to_string(),
            category: "browser".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "expression": { "type": "string", "description": "JavaScript expression to evaluate" }
                },
                "required": ["expression"]
            }),
        },
        ToolInfo {
            name: "browser_wait".to_string(),
            description: "Wait for a selector or condition".to_string(),
            category: "browser".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector to wait for" },
                    "timeout": { "type": "integer", "description": "Timeout in milliseconds", "default": 30000 }
                },
                "required": ["selector"]
            }),
        },
        ToolInfo {
            name: "browser_snapshot".to_string(),
            description: "Get an accessibility snapshot of the current page".to_string(),
            category: "browser".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
    ]
}
