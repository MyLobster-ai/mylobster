# Agent Tools Reference

MyLobster includes 26 agent tools organized into 8 categories. All tools implement the `AgentTool` trait and are registered in `src/agents/tools/mod.rs`.

## Tool Trait

```rust
#[async_trait]
pub trait AgentTool: Send + Sync {
    fn info(&self) -> ToolInfo;
    async fn execute(&self, params: Value, context: &ToolContext) -> Result<ToolResult>;
}
```

**ToolInfo** fields: `name`, `description`, `category`, `hidden`, `input_schema` (JSON Schema).

**ToolResult** variants: text, JSON, image, or error.

## Tool Categories

### Web Tools

| Tool | Description | Source |
|------|-------------|--------|
| `web.fetch` | Fetch a URL with SSRF protection, content-type detection, and configurable limits | `web_fetch.rs` |
| `web.search` | Multi-provider web search (Brave, Perplexity, Grok) with freshness filtering | `web_search.rs` |

See [Web Tools Architecture](web-tools.md) and [SSRF Protection](ssrf-protection.md) for details.

### Browser Tools

| Tool | Description | Source |
|------|-------------|--------|
| `browser.navigate` | Navigate the browser to a URL | `browser_tool.rs` |
| `browser.click` | Click an element by CSS selector | `browser_tool.rs` |
| `browser.type` | Type text into an input field | `browser_tool.rs` |
| `browser.screenshot` | Capture a screenshot of the page | `browser_tool.rs` |
| `browser.evaluate` | Execute JavaScript in the page context | `browser_tool.rs` |
| `browser.wait` | Wait for a selector or condition | `browser_tool.rs` |

Browser tools use headless Chrome via CDP (chromiumoxide). Requires Chrome or Chromium installed.

### System Tools

| Tool | Description | Source |
|------|-------------|--------|
| `system.run` | Execute a shell command with security policy, timeout, and working directory support | `bash.rs` |

Security levels:
- `"deny"` — All command execution blocked
- `"full"` — Unrestricted execution

Default timeout: 120 seconds. Shell selection is platform-aware (`bash` on Unix, `cmd` on Windows).

### Memory Tools

| Tool | Description | Source |
|------|-------------|--------|
| `memory.store` | Store content in long-term memory | `memory_tool.rs` |
| `memory.search` | Search the memory store using hybrid FTS + vector retrieval | `memory_tool.rs` |

See [Memory / RAG System](memory.md) for details.

### Session Tools

| Tool | Description | Source |
|------|-------------|--------|
| `sessions.list` | List all active sessions | `sessions_tool.rs` |
| `sessions.history` | Retrieve the conversation transcript for a session | `sessions_tool.rs` |
| `sessions.send` | Send a message to another session | `sessions_tool.rs` |
| `sessions.spawn` | Create a new session with an initial message | `sessions_tool.rs` |

### Messaging Tools

| Tool | Description | Source |
|------|-------------|--------|
| `message.send` | Send a formatted message to any configured channel | `message_tool.rs` |
| `discord.send` | Send a message via Discord | `discord_actions.rs` |
| `telegram.send` | Send a message via Telegram | `telegram_actions.rs` |
| `slack.send` | Send a message via Slack | `slack_actions.rs` |
| `whatsapp.send` | Send a message via WhatsApp | `whatsapp_actions.rs` |

Platform-specific tools provide access to platform features (reactions, threads, embeds) not available through the generic `message.send`.

### Media Tools

| Tool | Description | Source |
|------|-------------|--------|
| `image.generate` | Generate images from text descriptions | `image_tool.rs` |
| `tts.speak` | Convert text to speech audio | `tts_tool.rs` |

### Scheduling Tools

| Tool | Description | Source |
|------|-------------|--------|
| `cron.schedule` | Schedule a recurring job with cron syntax | `cron_tool.rs` |

### Other Tools

| Tool | Description | Source |
|------|-------------|--------|
| `canvas.render` | Render a visual canvas element | `canvas.rs` |
| `gateway.invoke` | Invoke an internal gateway method (hidden from agent) | `mod.rs` |
| `agents.list` | List available agents in a multi-agent setup | `mod.rs` |

## Tool Policy

Tools can be filtered via allow/deny lists in the configuration:

```json
{
  "tools": {
    "allow": ["web.fetch", "web.search", "system.run"],
    "deny": ["browser.*"],
    "exec": {
      "security": "full"
    }
  }
}
```

- `allow` — Only these tools are available (whitelist)
- `deny` — These tools are blocked (blacklist)
- When both are set, `allow` takes precedence

## Parameter Parsing (`src/agents/tools/common.rs`)

Shared utilities for extracting typed parameters from JSON input:

```rust
let url = get_string_param(&params, "url")?;
let count = get_optional_int_param(&params, "count").unwrap_or(10);
```

## Adding a New Tool

1. Create `src/agents/tools/my_tool.rs` implementing `AgentTool`
2. Register it in `src/agents/tools/mod.rs` in the `list_available_tools()` function
3. Define the JSON Schema for input parameters in `info().input_schema`

```rust
pub struct MyTool;

#[async_trait]
impl AgentTool for MyTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "my.tool".to_string(),
            description: "Does something useful".to_string(),
            category: "custom".to_string(),
            hidden: false,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "input": { "type": "string", "description": "The input" }
                },
                "required": ["input"]
            }),
        }
    }

    async fn execute(&self, params: Value, context: &ToolContext) -> Result<ToolResult> {
        let input = get_string_param(&params, "input")?;
        Ok(ToolResult::text(format!("Processed: {input}")))
    }
}
```
