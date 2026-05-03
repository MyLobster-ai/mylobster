# MyLobster ↔ OpenClaw v2026.5.2 Parity Tracker

**Status:** mylobster carries the v2026.4.29 backlog from `PARITY_v2026.4.29.md`. This file tracks the deltas that landed in the v4.29 → v5.2 window (≈1,540 upstream commits, ~155k lines added).

**Source:** `agent/openclaw/CHANGELOG.md` lines 5–394 (v5.2 section). Tag pinned at `v2026.5.2` (commit `8b2a6e5`).

**Scope rule:** items below have a plausible Rust analog in mylobster. Pure npm distribution / Docker / dist packaging / pnpm workspace / TypeBox / generated config schema / `dist/` chunks / Bun image plumbing items from the upstream changelog are out of scope for the Rust port and are not listed here.

**Conventions:**
- ✅ Done in mylobster
- 🚧 Partial
- ❌ Not yet
- *(file)* — natural landing site

---

## Providers (`src/providers/`)

| Item | Status | Notes |
|------|--------|-------|
| xAI Grok 4.3 default chat model + bundled catalog entry | ✅ | `providers/catalog.rs` (`XAI_CATALOG`, `XAI_DEFAULT_MODEL = "grok-4.3"`) |
| xAI `web_search` 60s default timeout + structured timeout error + malformed Responses parse hardening | 🚧 | `agents/tools/web_search.rs::search_grok` (timeout + structured error done; malformed Responses parse hardening still ❌) |
| OpenAI-compat TTS `extraBody` / `extra_body` passthrough (`/audio/speech` `lang` etc.) | ❌ | `openai_compat.rs` + tts pipeline |
| OpenAI Realtime + voice bridge: resolve `keychain:<service>:<account>` `OPENAI_API_KEY` refs (cached) | ❌ | `openai.rs` (we don't yet read keychain refs at all) |
| OpenAI GPT-5 sessions default to SSE Responses transport unless WS explicit | ❌ | `openai.rs` transport selection |
| OpenRouter/DeepSeek replay: fill DeepSeek V4 `reasoning_content` placeholders for `openrouter/deepseek/deepseek-v4-flash`/`-pro` | ❌ | `openrouter.rs` |
| LM Studio binary reasoning normalization (Gemma 4 etc.) | ❌ | `lm_studio.rs` |
| LM Studio `models.providers.lmstudio.params.preload: false` to skip native model-load (let JIT/idle TTL own lifecycle) | ❌ | `lm_studio.rs` |
| Anthropic-compatible stream text deltas that arrive before their matching content block | ✅ | `providers/anthropic_compat.rs` (already permissive — locked in by regression tests) |
| OpenRouter strip trailing assistant prefill turns from verified Anthropic-routed requests when reasoning enabled | ❌ | `openrouter.rs` |
| z.ai-style providers (z.ai direct, openrouter z-ai/\*, in-house GLM) — keep prior context on consecutive turns (no Pi state reset) | ❌ | provider compaction integration |
| Gemini 2.5 Flash-Lite `reasoning: "minimal"` → raise thinking-budget floor to 512 | ✅ | `providers/gemini.rs::effective_thinking_budget` |
| Gemini 3.1 Pro Preview: keep thinking-signature-only stream chunks active during reasoning (no idle timeout) | ❌ | `gemini.rs` |
| Gemini provider abort signals routed into fetches | ❌ | `gemini.rs` |
| Gemini `web_search` reuses Google provider API key/base URL as fallback + freshness/date filters via grounding | ❌ | `gemini.rs` |
| Brave web search: `webSearch.baseUrl` override + endpoint-aware cache keys (web + LLM Context) | 🚧 | `agents/tools/web_search.rs::search_brave` (`base_url` override done; LRU/cache layer not present yet) |
| Brave `brave.http` opt-in diagnostics flag (URLs, status, timing, cache hit/miss) | ✅ | `agents/tools/web_search.rs::search_brave` (`http_diag` → `target: "brave.http"` debug logs) |
| Brave LLM Context freshness/date ranges | ❌ | `brave.rs` |
| Exa `webSearch.baseUrl` override w/ endpoint-partitioned caches | ❌ | new `web_search/exa.rs` |
| MiniMax Search: `MINIMAX_API_KEY` + `MINIMAX_OAUTH_TOKEN` auto-detection; Coding Plan polling derives from configured base URL | ❌ | `minimax.rs` |
| SearXNG: pass-through image result urls; retry empty non-general category once with general | ❌ | `searxng.rs` |
| Firecrawl: reject private/loopback/metadata/non-HTTP(S) hosted scrape targets; allow explicit self-hosted private endpoints | ❌ | `firecrawl.rs` |
| Z.AI manifest-driven catalog (move runtime seed into manifest) | ❌ | `zai.rs` + provider manifest layer |
| Qianfan + Stepfun manifest setup auth metadata (`api-key`, env vars) | ❌ | `qianfan.rs`, `stepfun.rs` |
| DeepInfra manifest catalog discovery (drop duplicated static model data) | ❌ | `deepinfra.rs` |
| OpenAI Codex: preserve existing wrapped Codex streams during OpenAI attribution; strip native-Codex-only unsupported payload fields without touching custom compat endpoints | ❌ | `openai_codex.rs` |
| OpenAI Codex `openai-codex/gpt-5.4-mini` restored | ❌ | `openai_codex.rs` model catalog |
| MCP/OpenAI: normalize parameter-free MCP tool schemas before OpenAI tool submission | ❌ | `mcp.rs` + openai tool prep |
| Music gen: 10-second tool timeout floor; collapse cascading abort fallback errors | ❌ | `media/music.rs` |
| Bedrock Opus 4.7 thinking parity *(carryover from v4.29)* | ❌ | `bedrock.rs` |
| Codex / OpenAI-compat replay/streaming safety *(carryover from v4.29)* | ❌ | `openai_compat.rs`, `openai_codex.rs` |
| PDF/Gemini: send native PDF analysis API keys in `x-goog-api-key` header instead of URL | ❌ | `gemini.rs` |

---

## Channels (`src/channels/`)

| Item | Status | Notes |
|------|--------|-------|
| WhatsApp `@newsletter` outbound target with channel session metadata (not DM routing) | ❌ | `whatsapp.rs` |
| WhatsApp save downloadable quoted image media from reply context as inbound media | ❌ | `whatsapp.rs` |
| WhatsApp Baileys `end(error)` before raw socket close for long-lived sockets | ❌ | `whatsapp.rs` |
| Discord channel-audience DM authorization (`accessGroup:<name>`) | ❌ | `discord.rs` + new `routing/access_groups.rs` |
| Discord configurable gateway READY timeouts (startup + reconnect) | ❌ | `discord.rs` config |
| Discord Components v2 Text Display content from referenced replies and forwarded snapshots | ❌ | `discord.rs` |
| Discord retry queued REST 429s against learned bucket/global cooldowns | ❌ | `discord.rs` rate-limit layer |
| Discord configured outbound mention aliases (rewrite known `@Name` → real Discord user mention) | ❌ | `discord.rs` |
| Discord skip reaction listener registration when DMs disabled and no guild has reaction notifications on | ❌ | `discord.rs` |
| Discord typing indicators alive during long tool runs / auto-compaction | ❌ | `discord.rs` |
| Discord canonical mention prompt hints (`<@USER_ID>`, `<#CHANNEL_ID>`, `<@&ROLE_ID>`) | ❌ | `discord.rs` agent prompt hint |
| Discord/threads PluralKit dedupe by original message id; thread starter context only on first turn | ❌ | `discord.rs` |
| Discord/voice: text-only by default; voice-output policy hides agent `tts` tool; per-channel `systemPrompt` overrides | ❌ | `discord.rs` voice path |
| Discord upload-file message action with agent-scoped media reads | ❌ | `discord.rs` |
| Slack DM routing fixes (dmHistoryLimit, top-level DM stable sessions, target syntax `channel:C…`/`user:U…`/`<@U…>`, public-channel allowlists) | ❌ | `slack.rs` |
| Slack publish App Home tab view on `app_home_opened` + Home tab event in setup manifest | ❌ | `slack.rs` |
| Slack track bot-participated threads across restarts | ❌ | `slack.rs` + sessions persistence |
| Slack: keep typing/temporary status reactions for message-tool-only group/channel turns | ❌ | `slack.rs` |
| Slack: recover full inbound DM text from top-level rich-text blocks | ❌ | `slack.rs` |
| Telegram super group + admin-only commands *(carryover v4.29)* | ❌ | `telegram.rs` |
| Telegram outbound text/typing 60s Bot API guards + safe timeout overrides + typing fallback retries | ❌ | `telegram.rs` |
| Telegram split long markdown sends + media follow-ups into safe HTML chunks | ❌ | `telegram.rs` |
| Telegram inherit process DNS result order; downgrade IPv4 fallback promotions to debug | ❌ | `telegram.rs` networking |
| Telegram clamp low long-polling client timeouts so configured `timeoutSeconds` < poll window doesn't churn HTTPS | ❌ | `telegram.rs` |
| Telegram register/clear command menus in default + group-chat scopes | ❌ | `telegram.rs` |
| Telegram durable message edits for streaming previews (no draft state) | ❌ | `telegram.rs` |
| Telegram echo preflighted DM voice-note transcripts back to originating chat | ❌ | `telegram.rs` |
| Telegram benign delete-message 400s as no-op warnings | ❌ | `telegram.rs` |
| Matrix live approval (exec metadata, chunked fallback, thread targeting) *(carryover v4.29)* | ❌ | `matrix.rs` |
| Signal: group allowlists match inbound group ids and sender ids | ❌ | new `channels/signal.rs` |
| Signal: `getAttachment` HTTP cap from `channels.signal.mediaMaxMb` with base64 headroom | ❌ | `signal.rs` |
| Signal: keep long-lived receive SSE monitor open while idle (no 10s deadline) | ❌ | `signal.rs` |
| BlueBubbles opt-in `replyContextApiFallback` w/ SSRF guard, dedupe coalescing, log redaction (`?password=`/`?token=`/`Authorization:`) | ❌ | new `channels/bluebubbles.rs` |
| BlueBubbles classify Apple UTI audio attachments (`public.audio`, etc.) as audio | ❌ | `bluebubbles.rs` |
| Voice Call: per-number inbound routing (greetings, response agents/models/prompts, TTS voice overrides) | ❌ | new `channels/voice_call.rs` |
| Voice Call: `sessionScope: "per-call"` for fresh per-call agent memory | ❌ | `voice_call.rs` |
| Google Meet: `accessType` + `entryPointAccess` for API-created rooms; `googlemeet end-active-conference`; `test-listen` action; live caption health | ❌ | new `channels/google_meet.rs` |
| Mattermost: refresh native slash command registrations before accepting callbacks; per-command rate limit; body-read timeout | ❌ | new `channels/mattermost.rs` |
| QQBot group chat (history, @-mention gating, FIFO queue, C2C streaming) *(carryover v4.29)* | ❌ | `qqbot.rs` |
| Yuanbao plugin *(carryover v4.29)* | ❌ | `yuanbao.rs` |
| Channels: reject provider-prefixed targets for the wrong channel; let `telegram:123` select telegram on `last` fallback | ❌ | `channels/normalize.rs` or new `routing/target.rs` |
| Hooks: derive `ctx.channelId` from conversation target, not provider name | ❌ | `hooks/mod.rs` |
| Status reactions: remove stale non-terminal lifecycle reactions when run reaches done/error | ❌ | reaction lifecycle module |
| Channel auth: `accessGroup:<name>` reusable allowlists across channel auth paths | ❌ | new `routing/access_groups.rs` |
| `/dock-*` route switches start from direct chats only | ❌ | `routing/dock.rs` |
| Discord/Mattermost/Matrix: keep DM main-session route updates pinned to configured DM owner | ❌ | per-channel route mutator |
| `threadBindings.spawnSessions` (replaces split subagent/ACP toggles); `openclaw doctor --fix` migrates legacy keys | ❌ | `config/types.rs` + doctor migration |

---

## Gateway / RPC (`src/gateway/`)

| Item | Status | Notes |
|------|--------|-------|
| New RPC: `tools.invoke` (SDK-facing, shared HTTP policy, typed approval/refusal) | ❌ | new `gateway/tools_invoke.rs` |
| New RPC: `channels.stop` | ❌ | `gateway/channels_rpc.rs` |
| New RPC: `sessions.describe` | ❌ | `gateway/sessions_rpc.rs` |
| New RPC: `sessions.cleanup` | ❌ | `gateway/sessions_rpc.rs` |
| New RPCs: `artifacts.list` / `artifacts.get` / `artifacts.download` | ❌ | new `gateway/artifacts.rs` |
| `gateway restart --force` and `--wait <duration>`; log active task run IDs before restart deferral | ❌ | `cli/gateway.rs` |
| `gateway start` repair stale managed service definitions before starting | ❌ | `cli/gateway.rs` |
| Skip plugin-backed auth-profile overlays during startup secrets preflight (faster gateway readiness) | ❌ | `gateway/startup.rs` |
| Avoid repeated plugin-tool-descriptor config hashing (large runtime configs blocking reply startup) | ❌ | `gateway/dispatch.rs` |
| `sessions.list` responsive on large stores (list-safe cache/indexes; lightweight checkpoint preview) | ❌ | `gateway/sessions_rpc.rs` |
| Bounded async transcript reads; serialized parent-linked writes for hot transcript paths | ❌ | `sessions/transcript.rs` |
| Stream bounded transcript reads for session detail/history/artifacts/compaction | ❌ | `sessions/transcript.rs` |
| Bound chat-history transcript reads to requested display window | ❌ | `gateway/chat.rs` |
| Defer optional model-pricing catalog refresh until sidecars+channels ready; abort in-flight on shutdown | ❌ | `providers/pricing.rs` |
| Idle liveness samples → telemetry not visible warning logs | ❌ | `gateway/diagnostics.rs` |
| `secrets.reload` / `secrets.resolve`: include caught error msg in warning logs (RPC error stays generic) | ❌ | `gateway/secrets_rpc.rs` |
| Bounded redacted startup error message in stability bundles | ❌ | `gateway/diagnostics.rs` |
| Hot-reload Gateway plugin runtime surfaces after enable/disable (restart-backed for install/update/uninstall) | ❌ | `plugins/loader.rs` |
| Refresh cached health RPC snapshots when channel runtime state diverges | ❌ | `gateway/health.rs` |
| Return shared retryable startup-sidecars error for early control-plane RPCs (sessions.create/send/abort, agent.wait, tools.effective) | ❌ | `gateway/startup.rs` |
| Gateway/systemd: exit sysexits 78 on supervised lock + EADDRINUSE conflicts (stops Restart=always loops) | ❌ | `main.rs` |
| `$include` directives read from operator-approved `OPENCLAW_INCLUDE_ROOTS` directories | ❌ | `config/loader.rs` |
| `gateway.controlUi.chatMessageMaxWidth` validated config (instead of patched bundled CSS) | ❌ | `config/types.rs` |
| Cap oversized plugin-owned schemas in `config.schema` response | ❌ | `gateway/config_rpc.rs` |
| Live WebSocket client count in gateway info | ✅ | already present (commit `d6d228b`) |

---

## Agents (`src/agents/`, `src/gateway/chat.rs`)

| Item | Status | Notes |
|------|--------|-------|
| Mid-turn compaction precheck (`agents.defaults.compaction.midTurnPrecheck`) | ❌ | `gateway/chat.rs` |
| Inferred follow-up commitments + heartbeat delivery *(carryover v4.29)* | ❌ | new `agents/commitments.rs` |
| Active-run queueing default `steer` mode (500ms followup debounce) *(carryover v4.29)* | ❌ | `gateway/chat.rs` |
| `messages.visibleReplies` enforcement *(carryover v4.29)* | ✅ | landed (commit `12d2c5b`) |
| Critical tool-loop circuit-breaker stops returned as blocked tool results (not thrown failures) | ❌ | `agents/tool_loop.rs` |
| Heartbeat: `heartbeat_respond` structured tool for tool-capable heartbeat runs | ❌ | `agents/heartbeat.rs` |
| Heartbeat: active-hours-aware phase scheduling (skip quiet-hours timer arming, honor `activeHours.timezone`) | ❌ | `agents/heartbeat.rs` |
| Heartbeat: gate exec-event/notification/spawn/retry wakes through centralized cooldown; per-agent flood guard 5/60s | ❌ | `agents/heartbeat.rs` |
| Heartbeat: strip legacy `[TOOL_CALL]…[/TOOL_CALL]` / `[TOOL_RESULT]…[/TOOL_RESULT]` blocks from replies before delivery | ❌ | reply sanitization |
| Replies: strip MiniMax / XML tool-call scaffolding from shared user-facing reply sanitization | ❌ | `agents/reply_sanitize.rs` |
| Subagents: bound automatic orphan recovery + wedged-session tombstone | ❌ | `sessions/recovery.rs` |
| Subagents: honor `expectsCompletionMessage: false` (skip parent completion handoff) | ❌ | `agents/subagents.rs` |
| Subagents: avoid duplicate parent-visible replies for `sessions_send` to own persistent native subagent session | ❌ | `agents/subagents.rs` |
| Sessions: route Gateway writes/CLI cleanup/agent-delete purges through dedicated in-process writer | ❌ | `sessions/writer.rs` |
| Sessions: `session.writeLock.acquireTimeoutMs` (default 60s) for transcript lock acquisitions | ❌ | `sessions/lock.rs` |
| Sessions: match cleaned transcript locks by exact lock paths + canonical session fallback (topic-suffixed transcripts resume after restart) | ❌ | `sessions/lock.rs` |
| Sessions: preserve terminal lifecycle state when final run metadata persists from stale in-memory snapshot | ❌ | `sessions/lifecycle.rs` |
| Sessions: keep durable external conversation pointers (group/thread-scoped) out of age/count/disk maintenance eviction | ❌ | `sessions/maintenance.rs` |
| Sessions: keep session-store reads from running stale prune/cap maintenance during startup | ❌ | `sessions/maintenance.rs` |
| Sessions: reject `sessions_send` targets that resolve to thread-scoped chat sessions | ❌ | `sessions/send.rs` |
| Auth/sessions: JSON-clone auth-profile cache/runtime snapshots (no `structuredClone`) | ❌ | `auth/cache.rs` (n/a in Rust — verify equivalent) |
| Compaction: keep prior context on consecutive turns against z.ai/openrouter z-ai/in-house GLM | ❌ | `agents/compaction.rs` |
| Compaction: use active session model fallback chain for implicit summarization failures (Azure content-filter 400 recovery) | ❌ | `agents/compaction.rs` |
| Compaction: submit non-empty runtime-event marker for pre-compaction memory flush turns | ❌ | `agents/compaction.rs` |
| Failover: exempt run-level timeouts during tool execution from model fallback / timeout-triggered compaction | ❌ | `agents/failover.rs` |
| Failover: classify bare `status: internal server error` provider messages as retryable | ❌ | `agents/failover.rs` |
| Failover: carry `sessionId`, `lane`, `provider`, `model`, `profileId` through `FailoverError` | ❌ | `agents/failover.rs` |
| Codex: default app-server dynamic tools to native-first; default Codex-harness direct source replies to `message` tool when visible-reply not configured | ❌ | `agents/codex.rs` |
| Codex/Azure OpenAI Responses: enable malformed tool-call argument repair (generic OpenAI Responses out of repair gate) | ❌ | `agents/codex.rs`, `providers/openai.rs` |
| Codex: stop prompting message-tool-only source turns to finish with `NO_REPLY` | ❌ | `agents/codex.rs` |
| Sessions: avoid reopening large Pi transcript files through synchronous session manager for maintenance rewrites | ❌ | `sessions/transcript.rs` |
| Workspace: `agents.defaults.skipOptionalBootstrapFiles` config | ❌ | `config/types.rs` |
| Sandbox edits: preserve existing workspace file modes on atomic replace (don't collapse 0644 → 0600) | ❌ | `sessions/sandbox.rs` |
| Replies: skip blank visible user prompts at embedded-runner boundary (allow internal runtime-only + media-only) | ❌ | `gateway/chat.rs` |
| Per-agent TTS overrides (`agents.list[].tts`) *(carryover v4.29)* | ❌ | `config/types.rs` AgentEntry + tts |
| `maxActiveTranscriptBytes` preflight compaction trigger *(carryover v4.29)* | ❌ | `agents/compaction.rs` |

---

## Cron (`src/cron/`)

| Item | Status | Notes |
|------|--------|-------|
| `cron_changed` lifecycle helpers (created/updated/removed) *(v4.26)* | ✅ | landed `0b7dc36` |
| Tolerate malformed persisted cron jobs (one bad entry doesn't abort the whole tick) | ✅ | `cron/mod.rs::parse_jobs_lenient` |
| Retry recurring wake-now main-session jobs through temporary heartbeat busy skips before recording success | ❌ | `cron/scheduler.rs` |
| Keep implicit/default isolated cron announce deliveries out of main session awareness queue | ❌ | `cron/dispatch.rs` |
| Run cron announce payloads through normal TTS directive transform before outbound delivery | ❌ | `cron/dispatch.rs` |
| WhatsApp/Cron: keep DM pairing-store approvals out of implicit cron + heartbeat recipient fallback | ❌ | `cron/dispatch.rs` |
| `--wake now` immediate path in cron CLI | ❌ | `cli/cron.rs` |

---

## Memory (`src/memory/`)

| Item | Status | Notes |
|------|--------|-------|
| Per-conversation Active Memory filters | ❌ | `memory/active.rs` |
| Active Memory: configured recall timeout = blocking prompt-build hook budget; `setupGraceTimeoutMs` config | ❌ | `memory/active.rs` |
| Memory Wiki: replace CRLF managed blocks in place; collapse duplicate marker blocks; keep unmanaged markdown | ❌ | `memory/wiki.rs` |
| Memory Wiki: accept relative Markdown links with `.md` suffix during broken-wikilink validation | ❌ | `memory/wiki.rs` |
| Memory: retry transient SQLite index file swaps during atomic reindex on Windows (`EBUSY`/`EPERM`/`EACCES`) | ❌ | `memory/index.rs` |
| Memory-core/dreaming: include primary runtime workspace in multi-agent dreaming sweeps without mixing main-agent transcripts into subagent workspaces | ❌ | `memory/dreaming.rs` |
| Partial recall on timeout; bounded REM preview diagnostics *(carryover v4.29)* | ❌ | `memory/recall.rs` |
| Provenance views *(carryover v4.29)* | ❌ | `memory/provenance.rs` |

---

## TTS / Speech (`src/tts/`)

| Item | Status | Notes |
|------|--------|-------|
| Require explicit user/config audio intent for the agent speech tool (dashboard chats stay text unless audio requested) | ❌ | `tts/policy.rs` |
| Honor explicit short `[[tts:text]]…[[/tts:text]]` blocks while keeping untagged short auto-TTS suppressed | ❌ | `tts/directives.rs` |
| Keep trusted local audio generated by TTS tool queued for voice-note delivery even when run-level tool list omits raw `tts` | ❌ | `tts/queue.rs` |
| Voice Call/Twilio: honor TTS directive text + provider voice/model overrides during telephony synthesis | ❌ | voice call sender |
| OpenAI TTS extra_body passthrough → see Providers section | (link) | |
| ElevenLabs v3 in bundled catalog *(carryover v4.29)* | ❌ | `tts/elevenlabs.rs` |

---

## Plugins (`src/plugins/`)

| Item | Status | Notes |
|------|--------|-------|
| `activation.onStartup` plugin metadata *(carryover v4.29)* | ❌ | `plugins/manifest.rs` |
| `contracts.tools` manifest ownership: reject undeclared runtime tool names | ❌ | `plugins/registry.rs` |
| Plugin tool descriptor planner (descriptor-first visibility, generic availability checks, executor refs); cache descriptors from `api.registerTool` | ❌ | `plugins/descriptor.rs` |
| Plugin manifests declare static tool availability (skip unavailable plugin tool runtimes at startup) | ❌ | `plugins/manifest.rs` |
| Scope broad runtime preloads to effective plugin ids derived from config + startup planning + configured channels + slots + auto-enable rules | ❌ | `plugins/loader.rs` |
| Cache plugin tool factory results only for matching request context (no leaking sandbox/session/browser/delivery/runtime config state) | ❌ | `plugins/loader.rs` |
| Hot-reload Gateway plugin runtime surfaces after enable/disable; restart-backed for install/update/uninstall | ❌ | `plugins/loader.rs` |
| Reuse gateway-bindable plugin loader cache entries for default-mode loads (separate gateway-bound vs default registries) | ❌ | `plugins/loader.rs` |
| Hook `contracts.tools` enforcement (reject undeclared) | ❌ | `plugins/registry.rs` |
| `openclaw plugins list --json` includes package dependency install state | ❌ | `cli/plugins.rs` |
| `openclaw plugins enable/disable` refresh persisted plugin registry policy in place (no full rescan) | ❌ | `cli/plugins.rs` |
| Plugin SDK: `isPrivateIpAddress` re-export from `plugin-sdk/ssrf-runtime` (mylobster equivalent: shared SSRF guard module is already present in `infra/ssrf.rs`) | 🚧 | `infra/ssrf.rs` (verify export shape for plugin authors) |

---

## Skills (`src/skills/`)

| Item | Status | Notes |
|------|--------|-------|
| `openclaw skills check --agent` shows per-agent model and command visibility | ❌ | `cli/skills.rs` |
| Doctor reports / disables unavailable skills allowed for default agent | ❌ | `cli/doctor.rs` |
| Sessions store: stop persisting runtime-only `skillsSnapshot.resolvedSkills` array (rehydrate from disk on cold resume) | ❌ | `sessions/store.rs` |

---

## Routing / Auth (`src/routing/`, hook + access)

| Item | Status | Notes |
|------|--------|-------|
| Reusable message-channel access groups (`accessGroup:<name>` across channel auth paths) | ❌ | new `routing/access_groups.rs` |
| Discord/Mattermost/Matrix DM main-session route ownership pinning | ❌ | per-channel route mutator |
| Channel allowlist matching against bare runtime channel IDs | ❌ | `slack.rs` etc. |
| Auto-reply group-chat tool policy precedes fallback-delivery decision | ❌ | `agents/reply_policy.rs` |
| Auto-reply: fall back to automatic source delivery when `message` tool unavailable for precomputed message-tool-only replies | ❌ | `agents/reply_policy.rs` |

---

## Web / Browser / Fetch (`src/browser/`, web fetch)

| Item | Status | Notes |
|------|--------|-------|
| Share one browser control runtime across HTTP control server + `browser.request`; refresh browser profile config from source snapshot (honor `browser.executablePath`, `headless`, `noSandbox`) | ❌ | `browser/control.rs` |
| Web search provider validation: validate explicit `tools.web.search.provider` against bundled+installed plugin manifests (warn for stale 3rd-party config) | ❌ | `config/validate.rs` |
| Late-bind managed-agent `web_search` calls to current runtime config snapshot | ❌ | `gateway/chat.rs` |
| Resolve external `webFetchProviders` for non-sandboxed fetches | ❌ | `infra/web_fetch.rs` |
| Accept home-relative `MEDIA:~/...` attachment paths under existing file-read policy | ❌ | `media/attach.rs` |
| Web fetch SSRF guard: DNS re-check + IPv6 fix | ✅ | landed (`24f7a27`) |
| Web fetch IPv6 ULA opt-in for trusted proxy stacks | ❌ | `infra/web_fetch.rs` |
| Proxy/audio: convert standard FormData bodies before proxy-backed fetches (audio transcription / multipart) | ❌ | `infra/proxy.rs` |
| `openclaw proxy validate` CLI (effective config + reachability + allow/deny) | ❌ | `cli/proxy.rs` |

---

## CLI / Doctor (`src/cli/`)

| Item | Status | Notes |
|------|--------|-------|
| `openclaw gateway restart --force` and `--wait <duration>` | ❌ | `cli/gateway.rs` |
| `openclaw matrix encryption setup` *(carryover v4.29)* | ❌ | `cli/matrix.rs` |
| `infer model run --json` reports gateway model fallback attempts; avoid double-prefixing `openrouter/auto` in `models status` | ❌ | `cli/infer.rs`, `cli/models.rs` |
| Reject `--agent` on `openclaw models set` and `set-image` | ❌ | `cli/models.rs` |
| Stop treating non-plugin `auth` command root as bundled plugin id | ❌ | `cli/auth.rs` / `plugins/allow.rs` |
| Stop treating legacy singular `openclaw tool …` token as plugin id under `plugins.allow` | ❌ | `plugins/allow.rs` |
| `openclaw doctor --fix` migration for `threadBindings.spawnSessions` | ❌ | `cli/doctor.rs` |
| `openclaw doctor --fix` warn when default Telegram/Discord accounts have no token + env fallback unavailable | ❌ | `cli/doctor.rs` |
| `openclaw doctor` warn when `hooks.transformsDir` outside canonical directory | ❌ | `cli/doctor.rs` |
| Doctor: warn when restrictive `plugins.allow` paired with wildcard or plugin-owned tool allowlists | ❌ | `cli/doctor.rs` |
| Doctor: stop warning that non-existent unconfigured user-bin dirs are required in Gateway service PATH | ❌ | `cli/doctor.rs` |
| Doctor: 2026.5.2 configured-plugin install repair based on `meta.lastTouchedVersion`; preserve unmanaged third-party `node_modules` | ❌ | `cli/doctor.rs` |
| `gateway start` repair stale managed service definitions; concrete next-steps when probes fail and Bonjour finds no local gateway | ❌ | `cli/gateway.rs` |

---

## Misc / Infra (`src/infra/`, `src/logging/`, etc.)

| Item | Status | Notes |
|------|--------|-------|
| Logging: redaction patterns for Tencent Cloud, Alibaba Cloud, HuggingFace, Replicate API keys | ✅ | `logging/redact.rs` |
| Logging: redact payment credential field names (card number, CVC/CVV, shared payment token) | ✅ | `logging/redact.rs` |
| Logging: extend `sanitizeForLog` to redact `?password=`/`?token=` query params and `Authorization:` headers | ✅ | `logging/redact.rs::redact_text` |
| Path guards: fast path for canonical absolute POSIX containment checks | ❌ | `infra/path.rs` |
| Security: block `COMSPEC` in workspace `.env`; case-insensitive Windows shell trust-root family | ❌ | `infra/env.rs` |
| Security/audit: keep plain `security audit` on cold config/filesystem path; reserve plugin runtime collectors for `--deep` | ❌ | `cli/security.rs` |
| Diagnostics: reset stuck-session timers on reply/tool/status/block/ACP progress events; back off repeated `session.stuck` diagnostics | ❌ | `gateway/diagnostics.rs` |
| Diagnostics: stability bundle includes bounded redacted startup error message | ❌ | `gateway/diagnostics.rs` |
| Process/exec: stdin error listener in `runCommandWithTimeout` (swallow EPIPE from prematurely-exited child) | ❌ | `infra/exec.rs` |
| MCP/stdio: settle send() from write callback (reject on async write errors) | ❌ | `infra/mcp_stdio.rs` |
| Webhook/Nextcloud Talk: padded timing-safe compare even when supplied signature length wrong; normalized header lookup | ❌ | new `channels/nextcloud_talk.rs` |
| Process: ignore plugin install backup/disabled/dependency debris when enumerating plugin roots (avoid false positives for `.openclaw-install-backups`) | ❌ | `plugins/scan.rs` |

---

## Carryover items still pending from `PARITY_v2026.4.29.md`

The full v4.1→v4.29 backlog (still ~100 unchecked items) remains valid; the deltas above are **on top of** that file. When in doubt, treat that file as the parent backlog.

