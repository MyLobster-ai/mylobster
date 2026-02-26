# Changelog

All notable changes to the MyLobster Rust agent are documented in this file.

## [2026.2.25] - 2026-02-26

### Synced with OpenClaw v2026.2.25

This release ports security hardening, channel lifecycle, heartbeat direct policy, tool filtering, and model fallback types from OpenClaw v2026.2.25 (159 commits).

### Added

- **`DirectPolicy` enum** (`src/config/types.rs`) — `Last` (default) / `None` policy for heartbeat DM delivery. Added `direct_policy: Option<DirectPolicy>` field to `HeartbeatConfig`. Includes serde roundtrip tests.

- **Hardlink security guards** (`src/infra/hardlink_guards.rs`) — `PathAliasPolicy` enum with `AllowFinalSymlink`, `RejectAliases`, `UnlinkTarget` variants. `assert_no_hardlinked_final_path()` checks `nlink > 1` via `fs::metadata()` (Unix) to prevent workspace boundary escapes. `assert_no_path_alias_escape()` validates that canonicalized paths remain within workspace root. `same_file_identity()` helper compares inode+device via `MetadataExt`. Full test suite.

- **Exec approval with argv identity binding** (`src/agents/tools/bash.rs`) — `ExecApprovalRecord` struct binding command, argv, CWD, agent_id, session_key, and device_id to an approval decision. `approval_matches_system_run_request()` validates all bound fields. `harden_approved_execution_paths()` validates CWD existence/type and rejects symlinked CWDs for approval-bound executions.

- **Trusted-proxy control-UI bypass policy** (`src/gateway/connect_policy.rs`) — `ControlUiAuthPolicy` struct and `resolve_control_ui_auth_policy()` centralizing control-UI auth decisions. `is_trusted_proxy_control_ui_operator_auth()` checks role + auth mode + method for proxy/tailscale bypass. Full test suite.

- **Unified abort lifecycle** (`src/infra/abort_signal.rs`) — `AbortHandle` wrapping `Arc<tokio::sync::Notify>` for cooperative cancellation. `wait_for_abort()` async fn and `monitor_with_abort_lifecycle()` pattern using `tokio::select!`. Tests for signal propagation and abort monitoring.

- **Message-provider tool filtering** (`src/agents/tools/mod.rs`) — `tool_deny_by_message_provider()` constant mapping voice → tts.speak denial. `apply_message_provider_tool_policy()` function filtering tools by originating message provider to prevent echo loops.

- **Tool result normalization** (`src/agents/mod.rs`) — `normalize_tool_result()` ensures tool results always have valid structure: non-None text (falls back to JSON stringification or empty string), null JSON → None, error flag preserved.

- **Heartbeat direct policy resolution** (`src/infra/delivery.rs`) — `resolve_heartbeat_delivery_target()` respects `DirectPolicy` when selecting DM targets. Updated `resolve_heartbeat_delivery_chat_type()` to accept and respect `DirectPolicy::None`.

- **Model fallback with cooldown tracking** (`src/agents/model_fallback.rs`) — `ModelFallbackState` struct with per-model cooldown tracking via `HashMap<String, SystemTime>`. `FallbackAttempt` and `FailoverReason` types (Timeout, ContextOverflow, AuthError, RateLimit, Unknown). `resolve_next_fallback()` selects from chain skipping failed + cooled models. Stub `resolve_with_fallback()` async function signature.

### Changed

- **SSRF naming parity** (`src/agents/tools/web_fetch.rs`) — Updated `is_private_ip()` documentation to reference OpenClaw's `isBlockedSpecialUseAddress` naming convention. No functional change.

## [2026.2.22] - 2026-02-23

### Synced with OpenClaw v2026.2.22

This release ports features from OpenClaw v2026.2.14 through v2026.2.22 to the Rust codebase.

### Added

- **Mistral provider** (`src/providers/mistral.rs`) — Mistral AI support via their OpenAI-compatible API (`/v1/chat/completions`). Delegates to the shared `openai_compat` module. Default base URL: `https://api.mistral.ai/v1`. Detects model names starting with `mistral`, `pixtral`, or `codestral`. Resolves API key from config or `MISTRAL_API_KEY` env var.

- **`ModelApi::MistralMessages` config variant** — New enum variant for Mistral-specific API routing.

- **`ModelsConfig::apply_mistral_key()`** — Convenience method for programmatic Mistral provider configuration, following the existing pattern.

- **Mistral embedding provider** (`src/memory/embeddings.rs`) — Calls `https://api.mistral.ai/v1/embeddings` with model `mistral-embed`, producing 1024-dimension vectors. Resolves API key from config or `MISTRAL_API_KEY` env var.

- **`EmbeddingProvider::Mistral` variant** — New embedding provider option in configuration.

- **Synology Chat channel** (`src/channels/synology_chat.rs`) — New channel integration for Synology NAS Chat via webhooks. Supports outbound messages via incoming webhook URL with `payload={"text":"...","user_ids":[...]}` format. Inbound webhook validation uses constant-time token comparison. Configurable DM policy (`open`, `allowlist`, `disabled`), rate limiting, and per-user allowlists. Supports `allow_insecure_ssl` for self-signed NAS certificates.

- **`SynologyChatConfig` and `SynologyChatAccountConfig`** — Configuration types for Synology Chat with multi-account support.

- **Exec security hardening** (`src/agents/tools/bash.rs`) — Comprehensive environment variable sanitization for child processes:
  - `DANGEROUS_ENV_VARS` constant: blocks `NODE_OPTIONS`, `BASH_ENV`, `SHELLOPTS`, `PS4`, `SSLKEYLOGFILE`, `PROMPT_COMMAND`, `PYTHONSTARTUP`, `RUBYOPT`, and 20+ other injection vectors.
  - `DANGEROUS_ENV_PREFIXES`: blocks all `DYLD_*`, `LD_*`, `BASH_FUNC_*` variables.
  - `DANGEROUS_ENV_OVERRIDES`: `HOME` and `ZDOTDIR` cannot be set via tool params.
  - Child processes now start with `env_clear()` and selectively re-add only safe vars.
  - **Safe-bin profiles**: when a command matches a configured safe-bin name, args are validated against profile constraints (`max_positional`, `denied_flags`, `allowed_value_flags`).

- **`SafeBinProfile` config type** — New struct with `max_positional`, `allowed_value_flags`, `denied_flags` fields.

- **`ExecToolConfig::safe_bin_profiles`** — Optional map of binary name to `SafeBinProfile` for per-command arg validation.

- **Config merge prototype-pollution protection** (`src/config/mod.rs`) — `is_blocked_key()` helper rejects `__proto__`, `prototype`, and `constructor` keys. `merge_json_values()` recursively merges JSON objects while silently dropping blocked keys with a warning log.

### Changed

- **Enhanced SSRF protection** (`src/agents/tools/web_fetch.rs`) — Significantly expanded IP blocking:
  - **New IPv4 ranges**: unspecified (`0.0.0.0/8`), broadcast (`255.255.255.255`), multicast (`224.0.0.0/4`), reserved (`240.0.0.0/4`), benchmarking (`198.18.0.0/15`), TEST-NET-1/2/3 (`192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24`).
  - **IPv6 transition literal extraction**: NAT64 (`64:ff9b::/96` and `64:ff9b:1::/48`), 6to4 (`2002::/16`), Teredo (`2001:0000::/32` with XOR client extraction), ISATAP (IID marker `0000:5efe`). Embedded IPv4 addresses are extracted and validated against all IPv4 rules.
  - **IPv6 multicast blocking** (`ff00::/8`).
  - Refactored `is_private_ip` into `is_private_ipv4` + `is_private_ip` for reuse by transition extractors.
  - Added TODO note for future async DNS re-check of resolved IPs.

- **`detect_provider()` expanded** — Now handles Mistral model detection before Groq. Models starting with `mistral`, `pixtral`, or `codestral` route to the Mistral provider.

- **`resolve_provider()` expanded** — Now handles 6 providers (anthropic, openai, google, groq, mistral, ollama).

- **`MISTRAL_API_KEY` environment override** — Added to `Config::apply_env_overrides()` alongside existing Anthropic and OpenAI key overrides.

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
