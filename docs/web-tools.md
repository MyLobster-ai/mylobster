# Web Tools Architecture

MyLobster provides two web-facing agent tools: `web.search` and `web.fetch`. Both are defined in `src/agents/tools/` and implement the `AgentTool` trait.

## Web Search (`src/agents/tools/web_search.rs`)

Multi-provider web search with three backends, selected by `tools.web.search.provider` in config.

### Providers

#### Brave Search (default)

Traditional web search returning structured results (title, URL, description).

- **API**: `GET https://api.search.brave.com/res/v1/web/search`
- **Auth**: `X-Subscription-Token` header
- **Key resolution**: config `tools.web.search.apiKey` -> `BRAVE_API_KEY` env var
- **Freshness**: Passed directly as `freshness` query parameter (Brave supports `pd`, `pw`, `pm`, `py`, and date ranges)
- **Output**: Array of `{ title, url, description }` results

#### Perplexity

AI-powered search using Perplexity's Sonar models. Returns synthesized content with citations.

- **API**: `POST {base_url}/chat/completions` (OpenAI-compatible)
- **Auth**: Bearer token
- **Key resolution**: config `tools.web.search.perplexity.apiKey` -> `PERPLEXITY_API_KEY` -> `OPENROUTER_API_KEY`
- **Base URL inference**: Keys starting with `pplx-` use `https://api.perplexity.ai`; keys starting with `sk-or-` use `https://openrouter.ai/v1`
- **Default model**: `sonar-pro` (strips `perplexity/` prefix for direct API calls)
- **Freshness mapping**: `pd` -> `day`, `pw` -> `week`, `pm` -> `month`, `py` -> `year` (set as `search_recency_filter`)
- **Output**: `{ content, citations: [{ url }], provider: "perplexity" }`

#### Grok / xAI

AI-powered search using xAI's Responses API with built-in web search tool.

- **API**: `POST https://api.x.ai/v1/responses`
- **Auth**: Bearer token
- **Key resolution**: config `tools.web.search.grok.apiKey` -> `XAI_API_KEY` env var
- **Default model**: `grok-4-1-fast`
- **Request format**: Responses API with `tools: [{ type: "web_search" }]`
- **Output**: `{ content, citations: [{ url, title }], provider: "grok" }`

### Freshness Parameter

All providers accept an optional `freshness` parameter in the tool input schema:

| Shortcut | Meaning |
|----------|---------|
| `pd` | Past day |
| `pw` | Past week |
| `pm` | Past month |
| `py` | Past year |
| `YYYY-MM-DDtoYYYY-MM-DD` | Custom date range |

How freshness is applied per provider:
- **Brave**: Passed as `freshness` query parameter (native support)
- **Perplexity**: Mapped to `search_recency_filter` field (`pd` -> `"day"`, etc.)
- **Grok**: Not currently mapped (xAI Responses API doesn't expose a freshness filter)

### Configuration

```json
{
  "tools": {
    "web": {
      "search": {
        "provider": "brave",
        "apiKey": "BSA...",
        "maxResults": 10,
        "perplexity": {
          "apiKey": "pplx-...",
          "model": "sonar-pro"
        },
        "grok": {
          "apiKey": "xai-...",
          "model": "grok-4-1-fast"
        }
      }
    }
  }
}
```

## Web Fetch (`src/agents/tools/web_fetch.rs`)

HTTP fetch tool with content-type aware processing, SSRF protection, and configurable limits.

### Content Processing

The tool examines the `Content-Type` response header and processes content accordingly:

| Content-Type | Mode | Behavior |
|-------------|------|----------|
| `text/markdown` | `"markdown"` | Cloudflare Markdown for Agents pre-rendered content; used as-is |
| `application/json` | `"json"` | Parsed and pretty-printed via `serde_json::to_string_pretty()` |
| Everything else | `"raw"` | Raw response text |

The `extractMode` field in the response indicates which processing was applied.

When the `x-markdown-tokens` header is present (set by Cloudflare's Markdown for Agents), its value is logged at debug level.

### Response Format

```json
{
  "status": 200,
  "contentType": "text/markdown",
  "extractMode": "markdown",
  "text": "# Page Title\n\nContent..."
}
```

### SSRF Protection

The `is_ssrf_target()` function blocks requests to private/internal addresses. See [SSRF Protection](ssrf-protection.md) for details.

### Configuration

```json
{
  "tools": {
    "web": {
      "fetch": {
        "enabled": true,
        "maxChars": 200000,
        "timeoutSeconds": 10,
        "maxRedirects": 3
      }
    }
  }
}
```
