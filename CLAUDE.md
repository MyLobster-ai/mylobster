# CLAUDE.md — MyLobster Agent (Rust)

Rust port of the OpenClaw AI gateway. Multi-channel AI agent with WebSocket gateway, 16+ tools, 8 channel integrations, memory/RAG, multi-provider AI, plugin system, and browser automation.

## Build & Run

```bash
cargo build              # dev build
cargo build --release    # release build → target/release/mylobster
cargo test               # no tests yet
```

Rust 1.75+, edition 2021. Uses axum (not actix-web like the backend/provisioner).

There are many compiler warnings (unused imports, dead code) — this is expected as the port is new and not all code paths are wired up yet.

## Binary & CLI

Single binary: `mylobster`. Entry point: `src/main.rs`.

```
mylobster gateway [--config path] [--port N] [--bind addr]   # start gateway server
mylobster agent --message "..." [--session-key key]          # single message
mylobster send <channel> <to> <message>                      # send via channel
mylobster config show|validate|init                          # config management
mylobster doctor                                             # diagnostics
mylobster version                                            # print version
```

## Source Layout

```
src/
├── main.rs              # CLI dispatch (clap)
├── lib.rs               # pub mod declarations for all modules
├── cli/mod.rs           # CLI arg structs (Commands, GatewayOpts, AgentOpts, etc.)
├── config/
│   ├── types.rs         # ~2500 lines — all configuration types (serde, camelCase JSON)
│   ├── defaults.rs      # default config constants
│   ├── io.rs            # config file loading/writing (JSON, YAML, TOML, JSON5)
│   ├── validation.rs    # config validation logic
│   └── mod.rs           # Config struct, load(), write_default()
├── gateway/
│   ├── server.rs        # GatewayServer: start(), run_until_shutdown()
│   ├── routes.rs        # HTTP routes: health, chat completions, responses API
│   ├── websocket.rs     # WebSocket handler + JSON-RPC methods
│   ├── chat.rs          # Chat processing → AI provider integration
│   ├── auth.rs          # Token/password auth, Tailscale auth
│   ├── protocol.rs      # WebSocket protocol types, RPC messages
│   ├── client.rs        # WebSocket client for connecting to remote gateways
│   └── mod.rs           # re-exports, GatewayState
├── agents/
│   ├── mod.rs           # Agent runtime, run_single_message(), API compat handlers
│   └── tools/
│       ├── mod.rs       # Tool trait, ToolContext, ToolResult, tool registry
│       ├── common.rs    # Parameter parsing utilities
│       ├── bash.rs      # Shell command execution with security controls
│       ├── web_fetch.rs # HTTP fetch with SSRF protection
│       ├── web_search.rs# Brave Search integration
│       ├── browser_tool.rs, canvas.rs, cron_tool.rs, image_tool.rs
│       ├── memory_tool.rs, message_tool.rs, sessions_tool.rs, tts_tool.rs
│       └── discord_actions.rs, slack_actions.rs, telegram_actions.rs, whatsapp_actions.rs
├── channels/
│   ├── mod.rs           # ChannelManager, send_message()
│   ├── normalize.rs     # Message normalization across platforms
│   ├── telegram.rs, discord.rs, slack.rs, whatsapp.rs
│   ├── signal.rs, imessage.rs, plugin.rs
├── providers/
│   ├── mod.rs           # ModelProvider trait, ProviderRequest/Response, StreamEvent
│   ├── anthropic.rs     # Anthropic Messages API (SSE streaming)
│   ├── openai.rs        # OpenAI Chat Completions API (SSE streaming)
│   ├── gemini.rs        # Google Generative AI
│   └── bedrock.rs       # AWS Bedrock (stub)
├── memory/
│   ├── mod.rs           # MemoryStore trait
│   ├── manager.rs       # MemoryIndexManager (SQLite + FTS5 + vector)
│   ├── schema.rs        # SQLite schema for memory tables
│   ├── search.rs        # FTS and vector search queries
│   ├── embeddings.rs    # OpenAI, Gemini, Voyage, Local embedding providers
│   ├── chunking.rs      # Text chunking for embedding
│   └── hybrid.rs        # Hybrid FTS+vector scoring (RRF)
├── sessions/mod.rs      # SessionStore (DashMap-based in-memory)
├── plugins/mod.rs       # Plugin loading and lifecycle
├── hooks/mod.rs         # Webhook/hook system
├── browser/mod.rs       # Browser automation (chromiumoxide)
├── media/mod.rs         # Media processing
├── cron/mod.rs          # Cron job scheduling
├── routing/mod.rs       # Agent routing / binding resolution
├── logging/mod.rs       # Tracing subscriber init
└── infra/
    ├── mod.rs
    └── doctor.rs        # Diagnostics runner
```

## Key Dependencies

- **axum 0.8** — HTTP/WebSocket server (with tower middleware)
- **tokio** — async runtime
- **serde / serde_json** — serialization (config uses camelCase JSON)
- **reqwest 0.12** — HTTP client (rustls-tls, no OpenSSL)
- **tokio-tungstenite** — WebSocket client
- **rusqlite** (bundled) + r2d2 — SQLite for memory/sessions
- **teloxide 0.13** — Telegram bot
- **serenity 0.12** — Discord bot
- **slack-morphism 2** — Slack bot
- **chromiumoxide 0.7** — browser automation via CDP
- **ndarray** — vector math for embeddings
- **clap 4** — CLI argument parsing
- **jsonwebtoken / bcrypt** — auth

## Configuration

JSON config file loaded via `Config::load()`. All config types use `#[serde(rename_all = "camelCase")]`. The type system is in `src/config/types.rs` (~2500 lines). Supports JSON, YAML, TOML, and JSON5 formats.

Default gateway port: **18789** (loopback bind).

Key env vars: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `BRAVE_SEARCH_API_KEY`, `TELEGRAM_BOT_TOKEN`, `DISCORD_BOT_TOKEN`, `SLACK_BOT_TOKEN`, `SLACK_APP_TOKEN`.

## Architecture Notes

- **Gateway** is the main server component — handles WebSocket connections (JSON-RPC protocol), HTTP endpoints (OpenAI-compatible chat completions + responses API), and auth.
- **Agents** are the AI processing layer — receive messages, select tools, call providers, return responses.
- **Tools** implement the `Tool` trait (`name()`, `description()`, `parameters()`, `execute()`). Registered in `tools/mod.rs`.
- **Providers** implement `ModelProvider` trait with streaming SSE support. Anthropic and OpenAI are fully implemented.
- **Channels** integrate with messaging platforms. Each channel normalizes inbound messages and formats outbound responses.
- **Memory** uses SQLite with FTS5 for full-text search and a vector table for semantic search. Hybrid scoring via Reciprocal Rank Fusion.
- **Sessions** are in-memory (DashMap). No persistence yet.

## Conventions

- Modules use `mod.rs` pattern (not filename-based modules)
- Error handling: `anyhow::Result` for application errors, `thiserror` for library errors
- Async everywhere — all I/O is async via tokio
- Config types are deeply nested but always `Option<T>` with `Default` impls for optional sections
- No tests exist yet — this is a fresh port
