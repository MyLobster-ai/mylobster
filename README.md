<p align="center">
  <img src="assets/logo.jpg" alt="MyLobster" width="200">
</p>

<h1 align="center">MyLobster</h1>

<p align="center">
  Multi-channel AI agent gateway written in Rust
</p>

<p align="center">
  <a href="#why-rust">Why Rust?</a> &middot;
  <a href="#features">Features</a> &middot;
  <a href="#build--run">Build & Run</a> &middot;
  <a href="#cli-reference">CLI Reference</a> &middot;
  <a href="#architecture">Architecture</a> &middot;
  <a href="#documentation">Documentation</a> &middot;
  <a href="#comparison-with-openclaw-typescript">OpenClaw Comparison</a> &middot;
  <a href="#configuration">Configuration</a>
</p>

---

MyLobster is a Rust port of the [OpenClaw](https://github.com/openclaw/openclaw) AI gateway. It provides a WebSocket gateway, 16+ agent tools, 8 channel integrations, memory/RAG with hybrid search, multi-provider AI support, a plugin system, and browser automation — compiled into a single static binary with no runtime dependencies.

## Why Rust?

OpenClaw is written in TypeScript and runs on Node.js. It's a mature, full-featured AI gateway — but embedding it inside another application means bundling a Node.js runtime, managing npm dependencies, and spawning a child process. This makes it impractical to integrate into desktop apps, mobile apps, embedded systems, or any Rust-based application without significant glue code.

MyLobster solves this by reimplementing OpenClaw's core in Rust:

- **Embed as a library** — Add `mylobster` as a Cargo dependency and call `GatewayServer::start()` directly from your Rust application. No child processes, no IPC, no runtime to bundle.
- **Single static binary** — `cargo build --release` produces one self-contained executable. No `node_modules/`, no `package.json`, no `pnpm install`. Copy it anywhere and run it.
- **Native integration** — Rust desktop apps (egui, Tauri, Dioxus) and server apps (axum, actix-web) can import MyLobster's modules directly: use its providers, tools, channels, or memory system as library components without running a separate gateway process.
- **Cross-compilation** — Build for Linux, macOS, Windows, ARM, and WASM from a single codebase. Deploy to resource-constrained environments where a Node.js runtime isn't viable.
- **No garbage collection pauses** — Predictable latency for real-time WebSocket streaming and tool execution.

The TypeScript OpenClaw remains the upstream reference implementation. MyLobster tracks its protocol and feature set so the two are interchangeable at the network level — any client that speaks the OpenClaw WebSocket protocol works with either.

## Features

- **Multi-provider AI** — Anthropic Claude, OpenAI GPT, Google Gemini, Groq, Ollama (local models), AWS Bedrock
- **WebSocket gateway** — JSON-RPC protocol with OpenAI-compatible HTTP endpoints (chat completions + responses API)
- **16+ agent tools** — shell execution, web fetch/search, browser automation, cron, TTS, image processing, memory, messaging actions, and more
- **8 channel integrations** — Telegram, Discord, Slack, WhatsApp, Signal, iMessage, plugin channels
- **Memory / RAG** — SQLite with FTS5 full-text search + vector semantic search, hybrid scoring via Reciprocal Rank Fusion
- **Browser automation** — headless Chrome via CDP (chromiumoxide)
- **Plugin system** — extensible tool and channel plugins
- **Configuration** — JSON, YAML, TOML, and JSON5 config formats
- **Embeddable** — use as a library crate in any Rust application

## Build & Run

Requires **Rust 1.75+**.

```bash
cargo build              # dev build
cargo build --release    # release build → target/release/mylobster
```

### As a library

Add to your `Cargo.toml`:

```toml
[dependencies]
mylobster = { path = "../agent/mylobster" }
```

Then start the gateway programmatically:

```rust
use mylobster::config::Config;
use mylobster::gateway::GatewayServer;
use mylobster::cli::GatewayOpts;

let config = Config::load(None)?;
let opts = GatewayOpts { config: None, port: Some(18789), bind: None };
let server = GatewayServer::start(config, opts).await?;
server.run_until_shutdown().await?;
```

Or use individual modules (providers, memory, channels) without starting the full gateway.

## CLI Reference

MyLobster compiles to a single binary with six subcommands. Each subcommand accepts an optional `--config <path>` flag to specify a configuration file (defaults to `mylobster.json` in the current directory).

### `mylobster gateway`

Start the WebSocket + HTTP gateway server. This is the primary runtime mode — it listens for client connections, manages sessions, dispatches messages to AI providers, executes tools, and bridges to messaging channels.

```
mylobster gateway [--config path] [--port N] [--bind addr]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--config` | `mylobster.json` | Path to configuration file |
| `--port` | `18789` | Port to listen on |
| `--bind` | `127.0.0.1` | Bind address (`0.0.0.0` for all interfaces) |

The gateway exposes:
- **WebSocket** at `/` — JSON-RPC protocol for real-time agent interaction
- **HTTP** at `/v1/chat/completions` — OpenAI-compatible chat completions API
- **HTTP** at `/v1/responses` — OpenAI-compatible responses API
- **HTTP** at `/health` — health check endpoint

The server runs until interrupted (Ctrl+C) with graceful shutdown.

### `mylobster agent`

Process a single message through the AI agent and print the response. Useful for scripting, automation, and testing without running a persistent gateway.

```
mylobster agent <message> [--session-key key] [--config path]
```

| Flag | Description |
|------|-------------|
| `<message>` | The message to process (positional argument) |
| `--session-key` | Optional session identifier for maintaining conversation context across invocations |
| `--config` | Path to configuration file |

Examples:

```bash
# One-shot question
mylobster agent "What's the weather in Tokyo?"

# Maintain conversation context across calls
mylobster agent "My name is Alice" --session-key alice-session
mylobster agent "What's my name?" --session-key alice-session
```

### `mylobster send`

Send a message directly through a configured channel without agent processing. Bypasses the AI entirely — just delivers the message to the specified recipient on the specified platform.

```
mylobster send <channel> <to> <message> [--config path]
```

| Argument | Description |
|----------|-------------|
| `<channel>` | Channel name: `telegram`, `discord`, `slack`, `whatsapp`, etc. |
| `<to>` | Recipient identifier (chat ID, channel ID, phone number, etc.) |
| `<message>` | Message text to send |

Examples:

```bash
mylobster send telegram 123456789 "Server deployment complete"
mylobster send discord 987654321 "Build passed"
mylobster send slack "#general" "Release v2.0 is live"
```

### `mylobster config`

Manage configuration files. Has three subcommands:

```
mylobster config show [--config path]       # Print current config as pretty JSON
mylobster config validate [--config path]   # Validate config and report errors
mylobster config init [--config path]       # Generate a default mylobster.json
```

| Subcommand | Description |
|------------|-------------|
| `show` | Loads the config file, resolves all defaults, and prints the full resolved configuration as formatted JSON. Useful for debugging which values are actually in effect. |
| `validate` | Loads and validates the config. Exits with success if valid, or prints validation errors and exits with a non-zero code. |
| `init` | Writes a default configuration file. Use this to bootstrap a new installation. |

### `mylobster doctor`

Run diagnostics to verify that the system is set up correctly. Checks for required dependencies, validates API key connectivity, tests database access, and reports the status of each subsystem.

```
mylobster doctor
```

### `mylobster version`

Print the version string and exit.

```
mylobster version
```

Output: `mylobster 2026.2.14`

## Architecture

```
                  ┌─────────────────────────────┐
                  │         Gateway              │
                  │  (axum HTTP + WebSocket)     │
                  └──────────┬──────────────────┘
                             │
              ┌──────────────┼──────────────────┐
              ▼              ▼                   ▼
        ┌──────────┐  ┌───────────┐     ┌────────────┐
        │  Agents  │  │ Channels  │     │  Providers │
        │ (tools)  │  │ (8 plat.) │     │  (6 LLMs)  │
        └────┬─────┘  └───────────┘     └────────────┘
             │
     ┌───────┼────────┐
     ▼       ▼        ▼
 ┌────────┐ ┌──────┐ ┌─────────┐
 │ Memory │ │ Cron │ │ Browser │
 │ (RAG)  │ │      │ │ (CDP)   │
 └────────┘ └──────┘ └─────────┘
```

- **Gateway** — WebSocket (JSON-RPC) + HTTP server with auth (JWT, password, Tailscale). Built on axum 0.8 with tower middleware.
- **Agents** — AI processing layer; receives messages, selects tools, calls providers, returns responses. Supports the OpenAI-compatible chat completions and responses APIs.
- **Tools** — implement the `Tool` trait (`name()`, `description()`, `parameters()`, `execute()`). 16+ tools registered in `src/agents/tools/mod.rs`: bash, web_fetch, web_search, browser, canvas, cron, image, memory, message, sessions, TTS, and platform-specific actions for Discord, Slack, Telegram, and WhatsApp.
- **Providers** — implement `ModelProvider` with SSE streaming. Anthropic, OpenAI, Gemini, Groq, and Ollama are fully implemented; Bedrock is a stub. OpenAI and Groq share a common `openai_compat` base module. Ollama uses native NDJSON streaming via `/api/chat`.
- **Channels** — messaging platform integrations with normalized message handling. Telegram (teloxide), Discord (serenity), Slack (slack-morphism), WhatsApp, Signal, iMessage, and plugin channels.
- **Memory** — SQLite with FTS5 for full-text search + vector table for semantic search. Hybrid scoring via Reciprocal Rank Fusion (RRF). Supports OpenAI, Gemini, Voyage, and local embedding providers.
- **Sessions** — in-memory via DashMap. Per-session conversation state.

## Documentation

Detailed documentation for each subsystem is in the [`docs/`](docs/) directory:

| Document | Description |
|----------|-------------|
| [Provider Architecture](docs/providers.md) | 6 AI providers: Anthropic, OpenAI, Gemini, Groq, Ollama, Bedrock. Provider trait, resolution logic, OpenAI-compatible base. |
| [Gateway Protocol](docs/gateway-protocol.md) | WebSocket JSON-RPC protocol, frame types, chat streaming, HTTP endpoints, OpenAI-compatible API, authentication. |
| [Agent Tools](docs/tools.md) | 26 tools across 8 categories. AgentTool trait, tool policy, parameter parsing, guide to adding new tools. |
| [Web Tools](docs/web-tools.md) | `web.search` (Brave, Perplexity, Grok) and `web.fetch` with content-type detection and freshness filtering. |
| [SSRF Protection](docs/ssrf-protection.md) | URL and IP-level blocking: IPv4/IPv6 private ranges, cloud metadata, carrier-grade NAT, IPv4-mapped IPv6. |
| [Memory / RAG](docs/memory.md) | SQLite with FTS5 + vector search, hybrid RRF scoring, 4 embedding providers, text chunking. |
| [Channel Integrations](docs/channels.md) | 8 platforms: Telegram, Discord, Slack, WhatsApp, Signal, iMessage, plugin. ChannelPlugin trait, capabilities, message normalization. |
| [Configuration](docs/configuration.md) | 18 config sections, JSON/YAML/TOML/JSON5 formats, environment variable overrides. |
| [Sessions](docs/sessions.md) | In-memory session store, SessionHandle, WebSocket/HTTP access. |
| [Changelog](CHANGELOG.md) | All notable changes by version. |

## Comparison with OpenClaw (TypeScript)

MyLobster is a Rust port of [OpenClaw](https://github.com/openclaw/openclaw). The table below compares the two implementations:

| Aspect | OpenClaw (TypeScript) | MyLobster (Rust) |
|--------|----------------------|------------------|
| **Runtime** | Node.js 22+ | Single static binary (no runtime) |
| **CLI framework** | Commander.js | clap 4 (derive macros) |
| **HTTP server** | Express 5 | axum 0.8 |
| **WebSocket** | ws (Node.js) | axum ws + tokio-tungstenite |
| **Async model** | Node.js event loop | tokio multi-threaded runtime |
| **Package manager** | pnpm + npm | Cargo |
| **Config formats** | JSON5, YAML, TOML | JSON, YAML, TOML, JSON5 |
| **Database** | SQLite (better-sqlite3) | SQLite (rusqlite, bundled) |
| **AI providers** | Anthropic, OpenAI, Gemini, Bedrock, GitHub Copilot, Qwen | Anthropic, OpenAI, Gemini, Groq, Ollama, Bedrock |
| **Channels** | 12+ (WhatsApp, Telegram, Slack, Discord, Signal, iMessage, Google Chat, Teams, Matrix, Zalo, WebChat) | 8 (Telegram, Discord, Slack, WhatsApp, Signal, iMessage, plugin) |
| **Tools** | 40+ | 16+ |
| **Plugin system** | JS plugin SDK with runtime loading | Rust trait-based plugins |
| **CLI commands** | 15+ (gateway, agent, agents, onboard, setup, configure, config, memory, channels, models, maintenance, health, status, sessions, browser, tui, doctor) | 6 (gateway, agent, send, config, doctor, version) |
| **Gateway daemon** | install/uninstall as launchd/systemd service | Manual (run in foreground or use systemd externally) |
| **TUI** | Built-in terminal UI (`openclaw tui`) | Not yet implemented |
| **Embeddable** | Requires bundling Node.js runtime + `node_modules` | `cargo add mylobster` — use as a library crate |
| **Cross-compilation** | Requires Node.js on target | `cargo build --target <triple>` |
| **Binary size** | ~200MB+ (Node.js + node_modules) | ~30MB (static binary) |
| **Memory usage** | Node.js baseline + V8 heap | Minimal (no GC, no VM) |

### What MyLobster covers

MyLobster implements the core of OpenClaw: the gateway protocol (WebSocket JSON-RPC + HTTP), agent runtime with tool dispatch, multi-provider AI with SSE streaming, channel integrations for the major platforms, memory/RAG with hybrid search, browser automation, cron scheduling, and the configuration system. Any client that speaks the OpenClaw WebSocket protocol works with either implementation.

### What's not yet ported

OpenClaw has accumulated features that MyLobster hasn't replicated yet: the interactive onboarding wizard, multi-agent routing/workspaces, the built-in TUI, daemon install/uninstall, some niche channels (Google Chat, Teams, Matrix, Zalo, WebChat), and advanced safety features like Docker sandboxing and DM pairing. These are tracked for future work.

## Configuration

Config file loaded via `Config::load()`. Supports JSON, YAML, TOML, and JSON5. All config types use camelCase serialization via serde.

Default gateway port: **18789** (loopback bind).

### Environment variables

| Variable | Description |
|----------|-------------|
| `ANTHROPIC_API_KEY` | Anthropic Claude API key |
| `OPENAI_API_KEY` | OpenAI API key |
| `GROQ_API_KEY` | Groq fast inference API key |
| `OLLAMA_API_KEY` | Ollama API key (optional, for authenticated instances) |
| `BRAVE_SEARCH_API_KEY` | Brave Search API key (for web_search tool) |
| `PERPLEXITY_API_KEY` | Perplexity Sonar API key (for web_search tool) |
| `XAI_API_KEY` | xAI / Grok API key (for web_search tool) |
| `OPENROUTER_API_KEY` | OpenRouter API key (fallback for Perplexity search) |
| `TELEGRAM_BOT_TOKEN` | Telegram bot token |
| `DISCORD_BOT_TOKEN` | Discord bot token |
| `SLACK_BOT_TOKEN` | Slack bot token |
| `SLACK_APP_TOKEN` | Slack app-level token |

### Example config

```json
{
  "agent": {
    "model": "anthropic/claude-sonnet-4-5-20250929"
  },
  "gateway": {
    "port": 18789,
    "bind": "loopback"
  },
  "providers": {
    "anthropic": {
      "apiKey": "sk-ant-..."
    },
    "openai": {
      "apiKey": "sk-..."
    },
    "groq": {
      "apiKey": "gsk_..."
    },
    "ollama": {
      "baseUrl": "http://127.0.0.1:11434"
    }
  },
  "tools": {
    "web": {
      "search": {
        "provider": "brave",
        "perplexity": {
          "apiKey": "pplx-..."
        },
        "grok": {
          "apiKey": "xai-..."
        }
      }
    }
  },
  "channels": {
    "telegram": {
      "botToken": "123456:ABC-DEF..."
    },
    "discord": {
      "token": "..."
    }
  }
}
```

## License

MIT
