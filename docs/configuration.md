# Configuration Reference

MyLobster loads configuration from a JSON, YAML, TOML, or JSON5 file. All config types use camelCase serialization.

## Loading Order

1. Search for config file in order: `mylobster.json`, `mylobster.yaml`, `mylobster.yml`, `mylobster.toml` in the current directory
2. Fall back to `~/.mylobster/config.json`
3. Override with `--config <path>` CLI flag
4. Apply environment variable overrides

## Top-Level Sections

The `Config` struct has 18 sections, all optional with sensible defaults:

| Section | Type | Description |
|---------|------|-------------|
| `agent` | `AgentDefaultsConfig` | Default agent behavior (model, system prompt, temperature) |
| `agents` | `AgentsConfig` | Multi-agent configuration and routing |
| `gateway` | `GatewayConfig` | Server bind, port, auth, TLS, reload mode |
| `channels` | `ChannelsConfig` | Messaging platform integrations |
| `tools` | `ToolsConfig` | Tool allow/deny lists, exec security, web tool settings |
| `memory` | `MemoryConfig` | Memory/RAG: embedding provider, search settings, sync |
| `models` | `ModelsConfig` | AI provider configs (API keys, base URLs, custom models) |
| `plugins` | `PluginsConfig` | Plugin registry |
| `hooks` | `HooksConfig` | Webhook system |
| `messages` | `MessagesConfig` | Message handling and formatting |
| `session` | `SessionConfig` | Session TTL, limits |
| `logging` | `LoggingConfig` | Log level, format, output |
| `diagnostics` | `DiagnosticsConfig` | Telemetry settings |
| `sandbox` | `SandboxConfig` | Tool sandboxing |
| `browser` | `BrowserConfig` | Chrome path, headless mode, viewport |
| `tts` | `TtsConfig` | Text-to-speech provider settings |
| `cron` | `CronConfig` | Cron scheduler settings |
| `web` | `WebConfig` | Web server settings |

## Gateway Configuration

```json
{
  "gateway": {
    "port": 18789,
    "bind": "loopback",
    "auth": {
      "mode": "token",
      "token": "my-secret-token",
      "allowLocalBypass": true
    },
    "tls": {
      "cert": "/path/to/cert.pem",
      "key": "/path/to/key.pem"
    },
    "reload": "hot"
  }
}
```

**Bind modes:** `loopback` (127.0.0.1 only), `lan` (0.0.0.0), `auto`, `custom`, `tailnet`

**Auth modes:** `token`, `password`

**Reload modes:** `off`, `restart`, `hot`, `hybrid`

## Provider Configuration

```json
{
  "models": {
    "providers": {
      "anthropic": {
        "baseUrl": "https://api.anthropic.com",
        "apiKey": "sk-ant-..."
      },
      "openai": {
        "baseUrl": "https://api.openai.com/v1",
        "apiKey": "sk-..."
      },
      "groq": {
        "baseUrl": "https://api.groq.com/openai/v1",
        "apiKey": "gsk_..."
      },
      "ollama": {
        "baseUrl": "http://127.0.0.1:11434"
      }
    }
  }
}
```

See [Provider Architecture](providers.md) for details on each provider.

## Agent Configuration

```json
{
  "agent": {
    "model": "anthropic/claude-sonnet-4-5-20250929",
    "systemPrompt": "You are a helpful assistant.",
    "temperature": 0.7,
    "maxTokens": 4096
  }
}
```

## Channel Configuration

```json
{
  "channels": {
    "telegram": {
      "enabled": true,
      "defaultAccount": {
        "botToken": "123456:ABC-DEF..."
      }
    },
    "discord": {
      "enabled": true,
      "defaultAccount": {
        "token": "..."
      }
    },
    "slack": {
      "enabled": true,
      "defaultAccount": {
        "botToken": "xoxb-...",
        "appToken": "xapp-..."
      }
    }
  }
}
```

See [Channel Integrations](channels.md) for details.

## Tool Configuration

```json
{
  "tools": {
    "allow": ["web.fetch", "web.search", "system.run", "memory.*"],
    "deny": [],
    "exec": {
      "security": "full"
    },
    "web": {
      "fetch": {
        "enabled": true,
        "maxChars": 200000,
        "timeoutSeconds": 10,
        "maxRedirects": 3
      },
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

See [Web Tools Architecture](web-tools.md) for details.

## Memory Configuration

```json
{
  "memory": {
    "embedding": {
      "provider": "openai",
      "model": "text-embedding-3-small"
    },
    "search": {
      "mode": "hybrid",
      "maxResults": 10,
      "minScore": 0.3
    },
    "sync": {
      "paths": ["./docs", "./src"],
      "include": ["*.md", "*.rs"],
      "exclude": ["target", "node_modules"]
    }
  }
}
```

See [Memory / RAG System](memory.md) for details.

## Browser Configuration

```json
{
  "browser": {
    "headless": true,
    "chromePath": "/usr/bin/chromium",
    "viewport": {
      "width": 1280,
      "height": 720
    }
  }
}
```

## Environment Variables

Environment variables override config file values:

| Variable | Config path | Description |
|----------|-------------|-------------|
| `ANTHROPIC_API_KEY` | `models.providers.anthropic.apiKey` | Anthropic API key |
| `OPENAI_API_KEY` | `models.providers.openai.apiKey` | OpenAI API key |
| `GROQ_API_KEY` | `models.providers.groq.apiKey` | Groq API key |
| `OLLAMA_API_KEY` | `models.providers.ollama.apiKey` | Ollama API key (optional) |
| `GOOGLE_API_KEY` | `models.providers.gemini.apiKey` | Google Gemini API key |
| `BRAVE_SEARCH_API_KEY` | `tools.web.search.apiKey` | Brave Search API key |
| `PERPLEXITY_API_KEY` | `tools.web.search.perplexity.apiKey` | Perplexity API key |
| `XAI_API_KEY` | `tools.web.search.grok.apiKey` | xAI / Grok API key |
| `OPENROUTER_API_KEY` | — | Fallback for Perplexity (via OpenRouter) |
| `TELEGRAM_BOT_TOKEN` | `channels.telegram.defaultAccount.botToken` | Telegram bot token |
| `DISCORD_BOT_TOKEN` | `channels.discord.defaultAccount.token` | Discord bot token |
| `SLACK_BOT_TOKEN` | `channels.slack.defaultAccount.botToken` | Slack bot token |
| `SLACK_APP_TOKEN` | `channels.slack.defaultAccount.appToken` | Slack app-level token |
| `MYLOBSTER_STATE_DIR` | — | State directory (default: `~/.mylobster`) |
| `MYLOBSTER_GATEWAY_PORT` | `gateway.port` | Gateway port override |

## Config File Formats

All formats are supported. Examples:

**JSON** (`mylobster.json`):
```json
{ "gateway": { "port": 18789 } }
```

**YAML** (`mylobster.yaml`):
```yaml
gateway:
  port: 18789
```

**TOML** (`mylobster.toml`):
```toml
[gateway]
port = 18789
```

**JSON5** (`mylobster.json5`):
```json5
{ gateway: { port: 18789 } }  // comments allowed
```

## CLI Config Commands

```bash
mylobster config init       # Generate default config file
mylobster config show       # Print resolved config as JSON
mylobster config validate   # Validate config and report errors
```
