# Changelog

All notable changes to the MyLobster Rust agent are documented in this file.

## [2026.2.14] - 2026-02-14

### Synced with OpenClaw v2026.2.13

This release brings the Rust port up to date with key features from the OpenClaw v2026.2.13 TypeScript release.

### Documentation

- **docs/providers.md** — Provider architecture: three tiers (custom protocol, OpenAI-compatible, stub), provider resolution logic, configuration reference
- **docs/web-tools.md** — Web tools architecture: multi-provider search (Brave, Perplexity, Grok), freshness parameter, web.fetch content processing modes
- **docs/ssrf-protection.md** — SSRF protection: blocked categories, IPv4/IPv6 private ranges, carrier-grade NAT, IPv4-mapped IPv6, design decisions
- **docs/memory.md** — Memory/RAG system: MemoryIndexManager, SQLite schema (FTS5 + vector), hybrid RRF scoring, embedding providers, text chunking
- **docs/channels.md** — Channel integrations: ChannelPlugin trait, 14 capabilities, message normalization, 8 platform implementations
- **docs/gateway-protocol.md** — Gateway protocol: WebSocket frame types, JSON-RPC methods, chat streaming protocol, HTTP endpoints, authentication modes
- **docs/tools.md** — Agent tools reference: 26 tools across 8 categories, AgentTool trait, tool policy, parameter parsing, how to add new tools
- **docs/configuration.md** — Configuration reference: 18 top-level sections, all config formats, environment variable overrides, CLI config commands
- **docs/sessions.md** — Sessions: in-memory DashMap store, SessionHandle, WebSocket/HTTP access, limitations
- Updated **README.md** with documentation index linking to all docs

### Added

- **Ollama provider** (`src/providers/ollama.rs`) — Native Ollama API support via `/api/chat` with NDJSON streaming. Automatically detects models with tag separators (e.g. `llama3.3:latest`). Sets `num_ctx=65536` to override Ollama's small default context window. Strips `/v1` suffix from configured base URL. API key is optional (Ollama typically runs unauthenticated locally).

- **Groq provider** (`src/providers/groq.rs`) — Groq fast inference support via their OpenAI-compatible API. Delegates entirely to the shared OpenAI-compatible base. Detected when Groq is explicitly configured and model names match (`llama-*`, `mixtral-*`).

- **OpenAI-compatible base** (`src/providers/openai_compat.rs`) — Extracted shared types and HTTP logic from `openai.rs` into a reusable module. `OpenAiProvider` and `GroqProvider` both delegate to `openai_compat_chat()` and `openai_compat_stream_chat()`. This eliminates duplication and simplifies adding future OpenAI-compatible providers (HuggingFace, Together, etc.).

- **`ModelApi::Ollama` config variant** — New enum variant in `ModelApi` for Ollama-specific API routing.

- **`ModelsConfig::apply_groq_key()` and `apply_ollama_key()`** — Convenience methods for programmatic provider configuration, following the existing `apply_anthropic_key()` / `apply_openai_key()` pattern.

- **Perplexity web search provider** — Search via Perplexity's Sonar API. Resolves API key from config, `PERPLEXITY_API_KEY`, or `OPENROUTER_API_KEY` env vars. Infers base URL from key prefix (`pplx-*` vs `sk-or-*`). Maps freshness shortcuts to `search_recency_filter`. Returns content with citations.

- **Grok / xAI web search provider** — Search via xAI's Responses API with built-in `web_search` tool. Resolves API key from config or `XAI_API_KEY` env var. Default model: `grok-4-1-fast`. Extracts output text and URL annotations.

- **Web search `freshness` parameter** — All search providers now accept a `freshness` parameter with shortcuts: `pd` (past day), `pw` (past week), `pm` (past month), `py` (past year), and date ranges (`YYYY-MM-DDtoYYYY-MM-DD`). Brave passes it as a query parameter; Perplexity maps to `search_recency_filter`.

- **`PerplexitySearchConfig` and `GrokSearchConfig`** — New config structs under `WebSearchConfig` for provider-specific settings (API key, base URL, model, inline citations).

- **Cloudflare Markdown for Agents support** — `web.fetch` tool now detects `text/markdown` content-type and passes through pre-rendered markdown without processing. Logs `x-markdown-tokens` header at debug level.

- **JSON pretty-printing in web.fetch** — `application/json` responses are automatically pretty-printed for readability.

- **`extractMode` field in web.fetch response** — New field indicating how content was processed: `"markdown"`, `"json"`, or `"raw"`.

- **Enhanced SSRF protection** — New checks in `web.fetch`:
  - `.localhost` suffix blocking (e.g. `foo.localhost`)
  - Carrier-grade NAT range (`100.64.0.0/10`)
  - IPv4-mapped IPv6 addresses (`::ffff:x.x.x.x`) — applies IPv4 rules to the mapped address
  - IPv6 private ranges: ULA (`fc00::/7`), link-local (`fe80::/10`), deprecated site-local (`fec0::/10`), unspecified (`::`)
  - AWS IMDSv2 IPv6 endpoint (`fd00:ec2::254`)

### Changed

- **`openai.rs` refactored** — Reduced from ~325 lines to ~40 lines by delegating to the new `openai_compat` module.

- **`detect_provider()` now accepts `&Config`** — The provider detection function now takes the full config to check for explicitly configured providers (needed for Groq disambiguation). Internal only; no public API change.

- **`resolve_provider()` expanded** — Now handles 5 providers (anthropic, openai, google, groq, ollama) instead of 3. Ollama API key is optional.

- **`is_ssrf_target()` refactored** — Split into `is_ssrf_target()` (URL-level checks) and `is_private_ip()` (IP-level checks with full IPv4/IPv6 coverage).

### Architecture

- **Provider abstraction** — Three tiers of provider implementations:
  1. **Custom protocol** — Anthropic (Messages API) and Ollama (NDJSON `/api/chat`) have provider-specific request/response types
  2. **OpenAI-compatible** — OpenAI and Groq delegate to `openai_compat` shared functions
  3. **Stub** — Bedrock (not yet implemented)

- **Web search abstraction** — Three search backends behind a unified tool interface, selected by `tools.web.search.provider` config field. Each backend handles its own API key resolution, request format, and response parsing.

## [2026.2.10] - 2026-02-12

### Added

- Initial Rust port of OpenClaw AI gateway
- Anthropic, OpenAI, Gemini providers with SSE streaming
- WebSocket gateway with JSON-RPC protocol
- OpenAI-compatible HTTP endpoints (chat completions + responses API)
- 16+ agent tools (bash, web_fetch, web_search, browser, memory, etc.)
- 8 channel integrations (Telegram, Discord, Slack, WhatsApp, Signal, iMessage, plugin)
- Memory/RAG with SQLite FTS5 + vector search + hybrid RRF scoring
- Browser automation via CDP (chromiumoxide)
- Library mode with C FFI (rlib + cdylib + staticlib)
- CLI with 6 subcommands (gateway, agent, send, config, doctor, version)
- Configuration system supporting JSON, YAML, TOML, JSON5
- GitHub Actions CI workflow
