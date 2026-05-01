# MyLobster ↔ OpenClaw v2026.4.29 Parity Tracker

**Status:** mylobster is at OpenClaw `v2026.4.1` parity (commit `3f5b13b "Implement full OpenClaw v2026.4.1 parity across 23 files"`). This file tracks deltas needed to reach `v2026.4.29`.

**Scope:** OpenClaw added 14,350 commits / +1.75M lines / -545K lines across 28 days (v2026.4.1 → v2026.4.29). Most are refactors and bug fixes; the user-visible feature additions captured below are the actionable parity items.

**Source:** `agent/openclaw/CHANGELOG.md` lines 5-3227 (v2026.4.29 down to v2026.4.1). Cross-referenced with `agent/ironclaw/FEATURE_PARITY.md` (full matrix per OpenClaw subsystem).

**Conventions:**
- ✅ Done in mylobster
- 🚧 Partial
- ❌ Not yet
- *(file)* — touching this file is the natural place to land the change

---

## Providers (`src/providers/`)

| Item | Version | Status | Notes |
|------|---------|--------|-------|
| Cerebras OpenAI-compatible | v2026.4.26 | ✅ | `OPENAI_COMPAT_PROVIDERS` (mod.rs) |
| DeepInfra OpenAI-compatible (chat only) | v2026.4.27 | ✅ | `OPENAI_COMPAT_PROVIDERS` (mod.rs) |
| DeepInfra image generation/editing | v2026.4.27 | ❌ | Multi-modal extension; new path under `media/` |
| DeepInfra audio understanding | v2026.4.27 | ❌ | Multi-modal extension |
| DeepInfra TTS + text embeddings | v2026.4.27 | ❌ | Multi-modal extension |
| NVIDIA bundled provider w/ static catalog | v2026.4.29 | 🚧 | Already routes via OAI-compat; needs catalog metadata + literal model-ref picker |
| Bedrock Opus 4.7 thinking parity | v2026.4.29 | ❌ | `bedrock.rs` |
| Codex / OpenAI-compat replay/streaming safety | v2026.4.29 | ❌ | `openai_compat.rs`, `openai_codex.rs` |
| Manifest-backed provider catalog (Qianfan, Xiaomi, NVIDIA, Cerebras, Mistral, Moonshot, DeepSeek, StepFun) | v2026.4.27 | ❌ | New catalog layer; not currently in mylobster |
| Model suppression (prevent stale inline Codex bypassing manifest) | v2026.4.29 | ❌ | `mod.rs` resolution path |
| `modelCatalog.aliases` / `.suppressions` | v2026.4.27 | ❌ | Plugin manifest wiring |

### TTS / Speech (new bundled providers, all v2026.4.25)
| Provider | Status | Notes |
|----------|--------|-------|
| Azure Speech (Ogg/Opus + telephony) | ❌ | New module under `media/` |
| Xiaomi MiMo TTS | ❌ | |
| Inworld (streaming TTS, voice listing) | ❌ | |
| Volcengine/BytePlus Seed Speech | ❌ | |
| Local CLI TTS | ❌ | |
| ElevenLabs v3 in bundled catalog | ❌ | `tts_tool.rs` already has ElevenLabs at older version |

### Image / Video providers
| Provider | Version | Status |
|----------|---------|--------|
| LiteLLM image generation | v2026.4.25 | ❌ |
| Seedance 2.0 reference-to-video | v2026.4.25 | ❌ |

---

## Channels (`src/channels/`)

| Channel/Feature | Version | Status | Notes |
|-----------------|---------|--------|-------|
| QQBot group chat (history, @-mention gating, FIFO queue) | v2026.4.27 | ❌ | New file `channels/qqbot.rs` + ChannelsConfig field |
| QQBot C2C streaming | v2026.4.27 | ❌ | Same file |
| Yuanbao plugin | v2026.4.29 | ❌ | New file `channels/yuanbao.rs` |
| Telegram super group support (admin-only commands) | v2026.4.9 | ❌ | `telegram.rs` + admin check |
| Discord text command parsing (interactive dialogs) | v2026.4.8 | ❌ | `discord.rs` |
| Discord voice channel LLM override (`channels.discord.voice.model`) | v2026.4.25 | ❌ | `discord.rs` + `DiscordConfig` |
| WhatsApp `/tts latest` read-aloud | v2026.4.25 | ❌ | `whatsapp.rs` + `tts_tool.rs` integration |
| Matrix live approval (exec metadata, chunked fallback, thread targeting) | v2026.4.27 | ❌ | `matrix.rs` |
| Matrix streaming tool-progress updates | v2026.4.27 | ❌ | `matrix.rs` + agent loop hooks |
| Matrix E2EE setup flow + `openclaw matrix encryption setup` CLI | v2026.4.26 | ❌ | `matrix.rs` + `cli/mod.rs` |
| Non-image attachments via `chat.send` | v2026.4.27 | ❌ | `channels/normalize.rs` + each channel sender |
| Feishu and QQBot deep-merge account TTS overrides | v2026.4.27 | ❌ | `feishu.rs`, new `qqbot.rs` |
| Generic account TTS resolution (Feishu/QQBot) | v2026.4.25 | ❌ | Same as above |

---

## Agent System (`src/agents/`, `src/gateway/chat.rs`)

| Item | Version | Status | Notes |
|------|---------|--------|-------|
| Inferred follow-up commitments (hidden batched extraction, per-agent/channel scoping, heartbeat delivery) | v2026.4.29 | ❌ | New module; agent loop integration |
| Active-run queueing default `steer` mode (500ms followup debounce) | v2026.4.29 | ❌ | `gateway/chat.rs` agent loop |
| `messages.visibleReplies` enforcement | v2026.4.29 | ❌ | Config flag + agent loop check |
| `activation.onStartup` plugin metadata | v2026.4.27 | ❌ | `plugins/mod.rs` |
| Per-agent TTS overrides (`agents.list[].tts`) | v2026.4.26 | ❌ | `config/types.rs` AgentEntry + `tts_tool.rs` |
| `maxActiveTranscriptBytes` preflight compaction trigger | v2026.4.26 | ❌ | Compaction logic in gateway/chat.rs |
| `before_agent_finalize` hook + bounded native permission fingerprints | v2026.4.25 | ❌ | `hooks/mod.rs` |
| Per-agent voices + `/tts persona` + provider-aware TTS personas | v2026.4.25 | ❌ | `tts_tool.rs` + `cli/mod.rs` |
| Spawned subagent routing metadata (`spawnedBy` on chat/broadcast) | v2026.4.29 | ❌ | gateway protocol + `agents/mod.rs` |
| `/subagents spawn` command | (existing) | ❌ | `cli/mod.rs` |

---

## Memory & Knowledge (`src/memory/`)

| Item | Version | Status | Notes |
|------|---------|--------|-------|
| People-aware wiki (canonical aliases, person cards, relationship graphs, privacy/provenance) | v2026.4.29 | ❌ | New layer above existing memory schema |
| Per-conversation `allowedChatIds`/`deniedChatIds` Active Memory filters | v2026.4.29 | ❌ | `memory/search.rs` + filter wiring |
| Bounded partial recall summaries on timeout (temporary-transcript path recovery) | v2026.4.29 | ❌ | `memory/manager.rs` + agent compaction |
| REM dreaming + `doctor.memory.remHarness` preview RPC | v2026.4.29 | ❌ | New module |
| `memorySearch.inputType` (asymmetric embedding endpoints) | v2026.4.26 | ❌ | `memory/embeddings.rs` provider trait |
| Model-specific retrieval query prefixes (nomic-embed-text, qwen3-embedding, mxbai-embed-large for Ollama) | v2026.4.26 | ❌ | `memory/embeddings.rs` |

---

## Gateway / Architecture (`src/gateway/`)

| Item | Version | Status | Notes |
|------|---------|--------|-------|
| ACP-based channel binding registry rewrite | v2026.4.x | ❌ | Major: replaces per-channel files in OpenClaw; mylobster keeps per-file model so impact is contained but the registry layer is missing |
| SQLite-backed plugin state store (TTL, eviction, isolation) | v2026.4.29 | ❌ | `plugins/mod.rs` |
| Active-run steering / queueing | v2026.4.29 | ❌ | See Agent System |
| Slow-host startup diagnostics + event-loop readiness checks | v2026.4.27 | ❌ | `gateway/server.rs` |
| Stale-session recovery + version-scoped update caches | v2026.4.27 | ❌ | `gateway/server.rs` |
| Authenticated `node.presence.alive` protocol event | v2026.4.27 | ❌ | `gateway/protocol.rs`, `gateway/websocket.rs` |
| Plugin metadata snapshot reuse (avoid rebuild on startup) | v2026.4.27 | ❌ | `plugins/mod.rs` |
| Cold manifest/capability lookups (plugin index) | v2026.4.25 | ❌ | `plugins/mod.rs` |
| Startup diagnostics timeline | v2026.4.29 | ❌ | `infra/doctor.rs` |
| `spawnedBy` on subagent broadcast | v2026.4.29 | ❌ | gateway protocol |
| Read-only `doctor.memory.remHarness` RPC | v2026.4.29 | ❌ | gateway routes |
| Bridge-rs v2 routes for full v2026.4.29 RPC parity | v2026.4.29 | 🚧 | Done in `bridge-rs` (Phase 1 commit `668cd1c`); mylobster gateway side may need matching changes |

---

## Configuration (`src/config/`)

| Item | Version | Status | Notes |
|------|---------|--------|-------|
| Implicit profile widening security policy (`alsoAllow`, startup warnings) | v2026.4.29 | ❌ | `config/validation.rs` |
| Deprecated implicit startup sidecar loading + `activation.onStartup` migration | v2026.4.27 | ❌ | `plugins/mod.rs` + `config/types.rs` |
| Plugin compatibility warnings for deprecated startup loading | v2026.4.27 | ❌ | `plugins/mod.rs` |
| Control UI raw config diff panel (JSON5 + redaction) | v2026.4.26 | ❌ | Web UI; not in scope until web UI exists |
| `OPENCLAW_PLUGIN_STAGE_DIR` (layered runtime-dependency roots) | v2026.4.26 | ❌ | `plugins/mod.rs` |

---

## Plugins / Extensions (`src/plugins/`)

| Item | Version | Status | Notes |
|------|---------|--------|-------|
| Plugin SDK deprecation metadata + legacy alias guard | v2026.4.29 | ❌ | New SDK surface |
| Bundled plugin manifest migrations to explicit startup declarations | v2026.4.29 | ❌ | Plugin loader |
| Bundled-channel schema SDK | v2026.4.27 | ❌ | New module |
| Generic host hooks (session state, trusted-tool policy, UI descriptors, events, cleanup) | v2026.4.27 | ❌ | Hooks lifecycle |
| Persisted plugin registry snapshot (cold lookup paths) | v2026.4.25 | ❌ | `plugins/mod.rs` |
| Local plugin registry auto-migration on package install/update | v2026.4.25 | ❌ | `plugins/mod.rs` |
| Plugin registry alias normalization | v2026.4.25 | ❌ | `plugins/mod.rs` |
| `cron_changed` typed hook (gateway-owned cron lifecycle) | v2026.4.26 | ❌ | `hooks/mod.rs` + `cron/mod.rs` |
| Codex MCP hook relay (`PreToolUse`, `PostToolUse`, `PermissionRequest`) | v2026.4.25 | ❌ | `hooks/mod.rs` |

---

## CLI (`src/cli/`)

| Item | Version | Status | Notes |
|------|---------|--------|-------|
| `openclaw migrate` (plan, dry-run, JSON, backup) + Hermes/Claude importers | v2026.4.26 | ❌ | New subcommand |
| `openclaw matrix encryption setup` | v2026.4.27 | ❌ | New subcommand; depends on Matrix work |
| `openclaw plugins registry` | v2026.4.25 | ❌ | New subcommand |
| `openclaw plugins list` (cold registry snapshot default) | v2026.4.25 | ❌ | Update existing |
| `/plugins enable` / `/plugins disable` chat slash commands | v2026.4.25 | ❌ | Agent dispatcher |
| `openclaw browser start --headless` | v2026.4.25 | ❌ | Update `browser` subcommand |
| `infer image generate --background` / `infer image edit --background` | v2026.4.25 | ❌ | New subcommands |
| `/tts audio` / `/tts status` / `/tts persona` slash commands | v2026.4.25 | ❌ | Agent dispatcher + tts_tool |
| `OPENCLAW_SKIP_ONBOARDING` env (automated installs) | v2026.4.29 | ❌ | `cli/mod.rs` |
| `openclaw doctor --fix` (plugin index/cold registry refresh) | v2026.4.25 | ❌ | `infra/doctor.rs` |
| TUI Crestodian (interactive first-run setup) | v2026.4.25 | ❌ | New subcommand |

---

## Security (`src/gateway/auth.rs`, validation, sandbox)

| Item | Version | Status | Notes |
|------|---------|--------|-------|
| OpenGrep scanning (rulepack compiler, provenance, PR/full scan workflows) | v2026.4.29 | ❌ | CI/CD; out of code scope |
| Implicit profile widening guard | v2026.4.29 | ❌ | See Config above |
| Docker sandbox GPU passthrough (`sandbox.docker.gpus`) | v2026.4.27 | ❌ | mylobster doesn't have docker sandbox yet |
| Outbound proxy routing (strict forward-proxy validation, loopback-only Gateway bypass, IPv6 ULA) | v2026.4.27 | ❌ | New layer |
| Owner-only `/diagnostics` core with sensitive-data preamble | v2026.4.27 | ❌ | `gateway/routes.rs` |

---

## Telemetry / Build (`src/logging/`, build system)

| Item | Version | Status | Notes |
|------|---------|--------|-------|
| OpenTelemetry semantic conventions (GenAI attributes) | v2026.4.25 | ❌ | `logging/mod.rs` |
| Diagnostics-prometheus bundled plugin (protected scrape route) | v2026.4.25 | ❌ | `gateway/routes.rs` |
| GenAI token usage / model-call duration metrics (bounded attrs) | v2026.4.25 | ❌ | `logging/mod.rs` + provider call sites |
| Agent harness lifecycle OTEL spans + duration metrics | v2026.4.25 | ❌ | gateway/chat.rs |
| W3C `traceparent` propagation to providers | v2026.4.25 | ❌ | provider HTTP clients |
| Bounded telemetry exporter health diagnostics | v2026.4.25 | ❌ | `logging/mod.rs` |
| Memory pressure spans + histograms (bounded) | v2026.4.25 | ❌ | `memory/manager.rs` |
| Exec-process diagnostics (`openclaw.exec` spans) | v2026.4.25 | ❌ | `tools/bash.rs` |
| `OPENCLAW_OTEL_PRELOADED=1` for SDK reuse | v2026.4.25 | ❌ | `logging/mod.rs` |
| Signal-specific OTLP endpoint overrides | v2026.4.25 | ❌ | env/config |
| Central compatibility registry (dated owners, replacements, three-month removal) | v2026.4.25 | ❌ | docs/process |
| Workspace runtime/plugin/tooling refresh (ACP, Pi, AWS SDK, TypeBox, pnpm, oxlint, jsdom, pdfjs, ciao) | v2026.4.29 | ➖ | TS-only refresh; not applicable to Rust port |

---

## Web Interface (`src/gateway/routes.rs` for HTTP, separate web UI)

mylobster doesn't currently ship a separate Control UI. v2026.4.x web UI features are not directly applicable until a UI is added. Tracked here for completeness:

- v2026.4.29: Control UI i18n (Persian, Dutch, Vietnamese, Italian, Arabic, Thai)
- v2026.4.26: PWA install + Web Push notifications
- v2026.4.26: Raw config diff panel (JSON5 + redaction)
- v2026.4.26: Quick settings dashboard responsive grid

---

## Browser (`src/browser/`)

| Item | Version | Status | Notes |
|------|---------|--------|-------|
| Browser config for slow hosts (Raspberry Pi CDP timeouts) | v2026.4.25 | ❌ | `browser/mod.rs` |
| Browser `/discovery` route (frame-aware refs + attach prep) | v2026.4.25 | ❌ | `browser/mod.rs` |
| Browser `--headless` mode for one-shot managed launch | v2026.4.25 | ❌ | `browser` subcommand |
| Browser safe tab URLs + CDP role snapshots (cursor-clickable detection) | v2026.4.29 | ❌ | `browser/mod.rs` |
| Browser `extraArgs` config (custom Chrome launch args) | v2026.4.x | ❌ | `browser/mod.rs` config |

---

## Mobile / macOS Apps

OpenClaw v2026.4.x added iOS/Android `node.presence.alive` protocol, Android Talk Mode, macOS Peekaboo 3.0.0-beta4. mylobster has no mobile/macOS apps; flagged as not in scope unless added separately.

---

## Suggested Execution Order

1. **Provider catalog metadata layer** — manifest-backed catalogs unblock NVIDIA/Cerebras/DeepInfra/Bedrock model-ref pickers and `modelCatalog.aliases`/`.suppressions`. Touches `providers/mod.rs` and `config/types.rs`.
2. **Hooks: `before_agent_finalize`, `cron_changed`, Codex MCP relay** — tractable additions to `hooks/mod.rs` already familiar from recent commit `9937c5e`.
3. **Active-run steering / queueing + `messages.visibleReplies`** — single PR in `gateway/chat.rs` agent loop.
4. **DeepInfra multi-modal (image gen/edit, audio, TTS, embeddings)** — extends `media/` and `memory/embeddings.rs`.
5. **Telemetry plane (OTEL semconv + GenAI metrics + traceparent)** — `logging/mod.rs` + provider HTTP clients.
6. **Memory: people-aware wiki, allowedChatIds filters, REM dreaming, retrieval prefixes, `memorySearch.inputType`** — `memory/` overhaul.
7. **Channels: QQBot, Yuanbao, Telegram super groups, Discord text commands, Matrix E2EE+live approval+streaming** — per-channel feature adds.
8. **Plugins: SQLite-backed state store, `activation.onStartup` migration, generic host hooks, registry snapshots** — `plugins/mod.rs` overhaul.
9. **CLI: `migrate` + Hermes/Claude importers, `matrix encryption setup`, `plugins registry/list`, `/tts` slash commands, `doctor --fix`** — `cli/mod.rs`.
10. **Security: outbound proxy routing, owner-only diagnostics, sandbox GPU passthrough** — gateway + sandbox.
11. **Browser: safe tab URLs, CDP role snapshots, `/discovery`, slow-host timeouts, `--headless`** — `browser/mod.rs`.

This rough ordering favors unlocks-future-work first (catalogs, hooks, telemetry) over feature scope (channels, browser).
