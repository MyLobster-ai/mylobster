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
| xAI `web_search` 60s default timeout + structured timeout error + malformed Responses parse hardening | ✅ | `agents/tools/web_search.rs::search_grok` (lenient extraction) + `providers/xai.rs::extract_xai_web_search_content` / `resolve_xai_response_text_and_citations` (reusable hardened parser + tests) |
| OpenAI-compat TTS `extraBody` / `extra_body` passthrough (`/audio/speech` `lang` etc.) | ✅ | `openai_compat.rs::build_speech_request_body` (extra_body spread last, proto-key sanitize) + `openai_compat_speech` POST helper. Provider-side only — TTS pipeline should call these (cross-cluster hook) |
| OpenAI Realtime + voice bridge: resolve `keychain:<service>:<account>` `OPENAI_API_KEY` refs (cached) | ✅ | `openai.rs::parse_keychain_ref` + `resolve_openai_api_key_ref` (macOS `security find-generic-password`, bounded cache) |
| OpenAI GPT-5 sessions default to SSE Responses transport unless WS explicit | ✅ | `openai.rs::resolve_openai_transport` / `is_gpt5_responses_model` (SSE default; explicit WS wins) |
| OpenRouter/DeepSeek replay: fill DeepSeek V4 `reasoning_content` placeholders for `openrouter/deepseek/deepseek-v4-flash`/`-pro` | ✅ | `openrouter.rs::sanitize_completions_reasoning_replay_fields` — implements the *current* v2026.7.1 contract (#76018→#82150): empty placeholders stripped, non-empty `reasoning_content` preserved for DeepSeek V4/Kimi/MiMo ids (tier-suffix aware) |
| LM Studio binary reasoning normalization (Gemma 4 etc.) | ✅ | `lm_studio.rs::resolve_reasoning_capability` (allowed_options/default normalization, "off" disabled) |
| LM Studio `models.providers.lmstudio.params.preload: false` to skip native model-load (let JIT/idle TTL own lifecycle) | ✅ | `lm_studio.rs::should_preload` + `LmStudioProvider` (skips `/api/v1/models/load`); `config/types.rs::ModelProviderConfig.params` added |
| Anthropic-compatible stream text deltas that arrive before their matching content block | ✅ | `providers/anthropic_compat.rs` (already permissive — locked in by regression tests) |
| OpenRouter strip trailing assistant prefill turns from verified Anthropic-routed requests when reasoning enabled | ✅ | `openrouter.rs::strip_trailing_assistant_prefill_turns` / `prepare_openrouter_messages`; wired via `OpenRouterProvider::shape_request` (mod.rs routes `openrouter` through it) |
| z.ai-style providers (z.ai direct, openrouter z-ai/\*, in-house GLM) — keep prior context on consecutive turns (no Pi state reset) | 🚧 | provider-side classifier ✅ `zai.rs::keeps_prior_context(provider, model_id, base_url)` (port of `isSilentOverflowProneModel`); compaction integration in `gateway/chat.rs`/agents is the agents-cluster half |
| Gemini 2.5 Flash-Lite `reasoning: "minimal"` → raise thinking-budget floor to 512 | ✅ | `providers/gemini.rs::effective_thinking_budget` |
| Gemini 3.1 Pro Preview: keep thinking-signature-only stream chunks active during reasoning (no idle timeout) | ✅ | `providers/gemini.rs::classify_stream_part` (signature-only parts emit empty `Thinking` keep-alive; real SSE streaming via `:streamGenerateContent?alt=sse`) |
| Gemini provider abort signals routed into fetches | ✅ | `providers/gemini.rs::with_abort_handle` (chat + stream fetches race `AbortHandle`) |
| Gemini `web_search` reuses Google provider API key/base URL as fallback + freshness/date filters via grounding | ✅ | `agents/tools/web_search/gemini.rs` (key: config → `GEMINI_API_KEY` → `models.providers.google.apiKey`; base URL fallback + `google_search.timeRangeFilter`, day-freshness soft hint) |
| Brave web search: `webSearch.baseUrl` override + endpoint-aware cache keys (web + LLM Context) | ✅ | `agents/tools/web_search/brave.rs` (base-URL origin + endpoint paths, endpoint/mode-partitioned cache keys) + `web_search/cache.rs` (bounded 100-entry TTL cache, oldest-inserted eviction) |
| Brave `brave.http` opt-in diagnostics flag (URLs, status, timing, cache hit/miss) | ✅ | `agents/tools/web_search/brave.rs::log_brave_http` (`target: "brave.http"`: request, response, cache hit/miss/write) |
| Brave LLM Context freshness/date ranges | ✅ | `agents/tools/web_search/brave.rs` (llm-context mode: bounded/derived freshness ranges, future date_after + date_before-only rejection, ui_lang unsupported) |
| Exa `webSearch.baseUrl` override w/ endpoint-partitioned caches | ✅ | `agents/tools/web_search/exa.rs` (`/search` endpoint normalization, scheme validation, cache keys partitioned by endpoint/type/contents) |
| MiniMax Search: `MINIMAX_API_KEY` + `MINIMAX_OAUTH_TOKEN` auto-detection; Coding Plan polling derives from configured base URL | ✅ | `minimax.rs::resolve_minimax_credentials` (config → API key → OAuth token) + `coding_plan_usage_url` (derives from configured base, no CN default); web_search side: `agents/tools/web_search/minimax_search.rs` (env order `MINIMAX_CODE_PLAN_KEY` → `MINIMAX_CODING_API_KEY` → `MINIMAX_OAUTH_TOKEN` → `MINIMAX_API_KEY`; CN/global `coding_plan/search` endpoint from region/`MINIMAX_API_HOST`/provider base URL) |
| SearXNG: pass-through image result urls; retry empty non-general category once with general | ✅ | `agents/tools/web_search/searxng.rs` (`img_src` passthrough; empty non-general category retries once with `general`) |
| Firecrawl: reject private/loopback/metadata/non-HTTP(S) hosted scrape targets; allow explicit self-hosted private endpoints | ✅ | `agents/tools/web_search/firecrawl.rs` (`assert_firecrawl_scrape_target_allowed` + `validate_firecrawl_base_url`: official host strict, private/loopback self-hosted, public non-official rejected) |
| Z.AI manifest-driven catalog (move runtime seed into manifest) | ✅ | `providers/manifest.rs::ZAI_MANIFEST_MODELS` (v2026.7.1 GLM rows) + `zai.rs::zai_catalog`/`zai_auth_env_vars`; `mod.rs` zai default base moved to `api.z.ai` |
| Qianfan + Stepfun manifest setup auth metadata (`api-key`, env vars) | ✅ | `providers/manifest.rs` (auth methods + env vars) + `qianfan.rs`/`stepfun.rs` accessors; stepfun/deepseek added to `OPENAI_COMPAT_PROVIDERS` |
| DeepInfra manifest catalog discovery (drop duplicated static model data) | ✅ | `providers/manifest.rs::DEEPINFRA_MANIFEST_MODELS` + `deepinfra.rs::deepinfra_catalog` (runtime fallback catalog derives from manifest) |
| OpenAI Codex: preserve existing wrapped Codex streams during OpenAI attribution; strip native-Codex-only unsupported payload fields without touching custom compat endpoints | ✅ | `openai_codex.rs::is_native_codex_responses_base_url` + `sanitize_codex_responses_params` (chatgpt.com backend-api only; payload patched in place, wrapped WS stream preserved) |
| OpenAI Codex `openai-codex/gpt-5.4-mini` restored | ✅ | `openai_codex.rs::OPENAI_CODEX_ROUTABLE_MODEL_IDS` + `is_codex_routable_model` (gpt-5.4-mini in the routable set, v2026.7.1 state) |
| MCP/OpenAI: normalize parameter-free MCP tool schemas before OpenAI tool submission | ✅ | `openai_compat.rs::normalize_parameter_free_tool_schemas`, applied in `build_request` (covers all OpenAI-compat providers) |
| Music gen: 10-second tool timeout floor; collapse cascading abort fallback errors | ✅ | `media/music.rs::normalize_music_generation_timeout_ms` (current upstream floor 120s / default 300s) + `format_capability_failure_attempts` abort collapse |
| Bedrock Opus 4.7 thinking parity *(carryover from v4.29)* | ✅ | `bedrock.rs::bedrock_thinking_profile` (Opus 4.7/4.8 → xhigh/adaptive/max; 4.6 adaptive-default) + Converse `additionalModelRequestFields.thinking` passthrough |
| Codex / OpenAI-compat replay/streaming safety *(carryover from v4.29)* | ✅ | `openai_compat.rs` (lenient SSE: bare-JSON chunk lines per #96503) + `openai_codex.rs` (in-place payload sanitize, ProviderRequest field stripping on native backend) |
| PDF/Gemini: send native PDF analysis API keys in `x-goog-api-key` header instead of URL | ✅ | `providers/gemini.rs::gemini_analyze_pdf` (header key, never URL) + provider chat/stream also moved off `?key=` to `x-goog-api-key` |

---

## Channels (`src/channels/`)

| Item | Status | Notes |
|------|--------|-------|
| WhatsApp `@newsletter` outbound target with channel session metadata (not DM routing) | ❌ | `whatsapp.rs` |
| WhatsApp save downloadable quoted image media from reply context as inbound media | ❌ | `whatsapp.rs` |
| WhatsApp Baileys `end(error)` before raw socket close for long-lived sockets | ❌ | `whatsapp.rs` |
| Discord channel-audience DM authorization (`accessGroup:<name>`) | ✅ | `discord.rs` `is_dm_sender_authorized*` + `authorize_discord_dm`; expands `accessGroup:` via `routing::access_groups::resolve` (placeholder module added, routing cluster to replace) |
| Discord configurable gateway READY timeouts (startup + reconnect) | ✅ | `gatewayReadyTimeoutMs` (15s) / `gatewayRuntimeReadyTimeoutMs` (30s) in `DiscordAccountConfig`; `GatewayReadyWatch` state machine in `discord.rs` |
| Discord Components v2 Text Display content from referenced replies and forwarded snapshots | ✅ | `extract_components_v2_text` (type 10, recursive) + `resolve_forwarded_snapshots_text` + referenced-forward fallback in `resolve_discord_message_text` |
| Discord retry queued REST 429s against learned bucket/global cooldowns | ✅ | `RateLimitBook` (X-RateLimit-Bucket binding, global cooldown, Retry-After header/body parse) + `DiscordRestClient` queued retries in `discord.rs` |
| Discord configured outbound mention aliases (rewrite known `@Name` → real Discord user mention) | ✅ | `mentionAliases` config + `rewrite_discord_known_mentions` (code-span/fence skipping, discriminator handling); applied in outbound `send_message` |
| Discord skip reaction listener registration when DMs disabled and no guild has reaction notifications on | ✅ | `should_register_reaction_listeners` mirrors provider.startup gate (guild `reactionNotifications != off`, groupPolicy=disabled) |
| Discord typing indicators alive during long tool runs / auto-compaction | ✅ | `DiscordReplyTypingFeedback` (8s heartbeat, 20min TTL, `restart_for_dispatch` rotation, keeps ticking through tools/compaction) + `resolve_accepted_typing_prestart` policy |
| Discord canonical mention prompt hints (`<@USER_ID>`, `<#CHANNEL_ID>`, `<@&ROLE_ID>`) | ✅ | `DISCORD_MENTION_PROMPT_HINT` + `discord_message_tool_hints()` + `format_user/channel/role_mention` |
| Discord/threads PluralKit dedupe by original message id; thread starter context only on first turn | ✅ | `resolve_canonical_message_id` + `build_inbound_replay_key` + `InboundReplayGuard` (5min/5000 claim-commit-release); `should_include_thread_starter` first-turn gate |
| Discord/voice: text-only by default; voice-output policy hides agent `tts` tool; per-channel `systemPrompt` overrides | ✅ | `DiscordVoiceConfig` + `DISCORD_VOICE_SPOKEN_OUTPUT_CONTRACT`, `is_tool_hidden_for_voice("tts")`, `resolve_channel_system_prompt`; realtime/bidi provider stack not ported (no dep) |
| Discord upload-file message action with agent-scoped media reads | ✅ | `uploadFile` action in `discord_actions.rs` (multipart) + `resolve_agent_scoped_media_path` workspace scoping with alias-escape guard |
| Slack DM routing fixes (dmHistoryLimit, top-level DM stable sessions, target syntax `channel:C…`/`user:U…`/`<@U…>`, public-channel allowlists) | ❌ | `slack.rs` |
| Slack publish App Home tab view on `app_home_opened` + Home tab event in setup manifest | ❌ | `slack.rs` |
| Slack track bot-participated threads across restarts | ❌ | `slack.rs` + sessions persistence |
| Slack: keep typing/temporary status reactions for message-tool-only group/channel turns | ❌ | `slack.rs` |
| Slack: recover full inbound DM text from top-level rich-text blocks | ❌ | `slack.rs` |
| Telegram super group + admin-only commands *(carryover v4.29)* | ✅ | `telegram_commands.rs`: supergroup = group, `should_allow_group_command` (admin/creator via getChatMember; `adminOnlyCommands` group→account override; allowlist bypass) |
| Telegram outbound text/typing 60s Bot API guards + safe timeout overrides + typing fallback retries | ✅ | `telegram_net.rs` per-method table (60s sendMessage/sendChatAction, config only extends); `telegram.rs::send_typing` retries timed-out typing once + 401/transient guard |
| Telegram split long markdown sends + media follow-ups into safe HTML chunks | ✅ | `telegram_format.rs`: faithful `splitTelegramHtmlChunks` port (UTF-16 limits, tag reopen, entity/code-point safe splits) + caption 1024 split → chunked follow-up |
| Telegram inherit process DNS result order; downgrade IPv4 fallback promotions to debug | ✅ | `telegram_net.rs`: dnsResultOrder env/config decision (unset = inherit process/getaddrinfo order); sticky IPv4 promotions+recoveries log debug, pinned-IP stays warn |
| Telegram clamp low long-polling client timeouts so configured `timeoutSeconds` < poll window doesn't churn HTTPS | ✅ | `telegram_net.rs::resolve_telegram_long_poll_timeout_seconds` (default 30s, cap 40s = 45s HTTP guard − 5s margin); wired in `TelegramApi::get_updates` |
| Telegram register/clear command menus in default + group-chat scopes | ✅ | `telegram.rs::register_commands/clear_commands` iterate default + all_group_chats scopes; ≤100 cmds, 5700-char budget fitting in `telegram_commands.rs` |
| Telegram durable message edits for streaming previews (no draft state) | ✅ | `telegram.rs::edit_message_durable` + `StreamingPreview::apply_update`; not-modified/no-text/message-gone 400s benign, HTML→plain edit fallback |
| Telegram echo preflighted DM voice-note transcripts back to originating chat | ✅ | `telegram.rs::send_transcript_echo` (best-effort, thread-id aware) gated on `tools.media.audio.echoTranscript`/`echoFormat`, default `📝 "{transcript}"` |
| Telegram benign delete-message 400s as no-op warnings | ✅ | `telegram_net.rs` MESSAGE_DELETE_NOOP classifier + `TelegramApi::delete_message_benign`; wired in `telegram_actions.rs` deleteMessage |
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
| Bounded async transcript reads; serialized parent-linked writes for hot transcript paths | ✅ | `sessions/transcript.rs`: `read_transcript_head`/`read_transcript_tail` (byte+entry bounded, truncation-flagged, reverse chunked tail) + per-path async write locks; `append_parent_linked` serializes child+link writes through the parent's lock |
| Stream bounded transcript reads for session detail/history/artifacts/compaction | ✅ | `sessions/transcript.rs::TranscriptReadOptions` — purpose-sized bounded streamed reads; gateway detail/history/artifacts/compaction call sites should pass their window (gateway cluster adoption) |
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
| Subagents: bound automatic orphan recovery + wedged-session tombstone | ✅ | `sessions/recovery.rs`: `OrphanRecovery::plan_sweep` (bounded per-scan, per-session attempt cap, counter reset on success) + persisted `Tombstone` markers (`write_tombstone`/`load_tombstones`, seeded across restarts) |
| Subagents: honor `expectsCompletionMessage: false` (skip parent completion handoff) | ❌ | `agents/subagents.rs` |
| Subagents: avoid duplicate parent-visible replies for `sessions_send` to own persistent native subagent session | ❌ | `agents/subagents.rs` |
| Sessions: route Gateway writes/CLI cleanup/agent-delete purges through dedicated in-process writer | ✅ | `sessions/writer.rs::SessionWriterHandle` — single serialized command queue for appends / parent-linked appends / CLI cleanup (with lock-file sweep) / agent-delete purges; gateway+CLI call sites should route through the handle |
| Sessions: `session.writeLock.acquireTimeoutMs` (default 60s) for transcript lock acquisitions | ✅ | `sessions/lock.rs::SessionWriteLocks` + `config/types.rs::SessionWriteLockConfig` (`session.writeLock.acquireTimeoutMs`, default 60_000; `maxHoldMs` default 300_000); tokio-time tests for timeout + reclaim |
| Sessions: match cleaned transcript locks by exact lock paths + canonical session fallback (topic-suffixed transcripts resume after restart) | ✅ | `sessions/lock.rs::matches_cleaned_lock` (exact lock path → canonical `.topic-*`/`-topic-*` strip fallback); swept by `writer.rs` cleanup |
| Sessions: preserve terminal lifecycle state when final run metadata persists from stale in-memory snapshot | ✅ | `sessions/lifecycle.rs::merge_final_run_metadata` (terminal never downgraded by stale snapshots, even later-wallclock non-terminal flushes) + `store.rs::persist_final_run_metadata` |
| Sessions: keep durable external conversation pointers (group/thread-scoped) out of age/count/disk maintenance eviction | ✅ | `sessions/maintenance.rs::select_eviction_candidates` — `is_durable_external_pointer` (group/thread/topic/channel-scoped keys) exempt from all three eviction reasons |
| Sessions: keep session-store reads from running stale prune/cap maintenance during startup | ✅ | `sessions/maintenance.rs::MaintenanceGate` — `read_may_trigger_maintenance()` false until `mark_startup_complete()`; passes serialized |
| Sessions: reject `sessions_send` targets that resolve to thread-scoped chat sessions | 🚧 | `sessions/send.rs::validate_sessions_send_target` (`:thread:`/`:topic:` lanes + `.topic-*` transcript identities) implemented + tested; needs call from `sessions_send` tool dispatch (agents cluster, `agents/tools/sessions_tool.rs`) |
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
| Sessions: avoid reopening large Pi transcript files through synchronous session manager for maintenance rewrites | ✅ | `sessions/transcript.rs::rewrite_transcript_streaming` (line-streamed temp-file rewrite, atomic mode-preserving replace, never whole-file load) + `requires_streaming_rewrite` (≥8 MiB) surfaced on `maintenance.rs` eviction candidates |
| Workspace: `agents.defaults.skipOptionalBootstrapFiles` config | ❌ | `config/types.rs` |
| Sandbox edits: preserve existing workspace file modes on atomic replace (don't collapse 0644 → 0600) | ✅ | `sessions/sandbox.rs::atomic_replace_preserving_mode` / `persist_preserving_mode` (existing mode kept, new files 0644 not the 0600 tempfile default); reused by transcript rewrites |
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
| Sessions store: stop persisting runtime-only `skillsSnapshot.resolvedSkills` array (rehydrate from disk on cold resume) | ✅ | `sessions/store.rs::strip_runtime_skills_snapshot` + `persist_skills_snapshot` (SQLite store never persists `resolvedSkills`) |

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
| Resolve external `webFetchProviders` for non-sandboxed fetches | ✅ | `infra/web_fetch.rs::resolve_external_web_fetch_provider` (sandboxed → bundled only; explicit provider w/ credential check, else credential auto-detect; Firecrawl wired) |
| Accept home-relative `MEDIA:~/...` attachment paths under existing file-read policy | ✅ | `media/attach.rs` (`normalize_media_reference_source` + `resolve_local_media_path`: `~/` expansion; `~user`, `..`-escapes, null bytes, unsupported schemes rejected) |
| Web fetch SSRF guard: DNS re-check + IPv6 fix | ✅ | landed (`24f7a27`) |
| Web fetch IPv6 ULA opt-in for trusted proxy stacks | ✅ | `agents/tools/web_fetch.rs::SsrfPolicy` (`tools.web.fetch.ssrfPolicy.allowIpv6UniqueLocalRange` exempts fc00::/7 only; `allowRfc2544BenchmarkRange` for 198.18.0.0/15) |
| Proxy/audio: convert standard FormData bodies before proxy-backed fetches (audio transcription / multipart) | ✅ | `infra/proxy.rs` (`normalize_multipart_headers` drops stale boundary/length so the client owns the multipart boundary; env proxy resolution + NO_PROXY matching) |
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

