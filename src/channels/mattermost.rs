//! Mattermost channel: REST API v4 transport plus the native slash-command,
//! threading, attachment, and channel-kind behavior of the OpenClaw
//! Mattermost plugin.
//!
//! Ports the observable behavior of OpenClaw v2026.7.1
//! `extensions/mattermost/src/mattermost/slash-commands.ts`, `slash-http.ts`,
//! `slash-state.ts`, `monitor-gating.ts`, `monitor.ts` (system-post +
//! reply-root resolution), `thread-participation.ts`, and `client.ts`
//! (file upload):
//!
//! - v2026.5.2: native slash-command registrations are refreshed/reconciled
//!   before callbacks are accepted (owned-vs-foreign trigger reconciliation,
//!   fail-closed listing), per-command validation-lookup rate limit (token
//!   bucket, burst 20 / refill 500 ms), and a bounded body read (64 KiB /
//!   5 s timeout) on webhook callback reads.
//! - v2026.7.1 row 88: message-tool replies stay in threads (reply root
//!   resolution), the `/oc_queue` slash command, thread participation that
//!   survives restarts (SQLite store, 7-day idle expiry), attachments via the
//!   `/api/v4/files` upload API (`file_ids` on posts), and channel-vs-group
//!   identification from the Mattermost `channel_type` (`D`/`G`/`P`/`O`).
//!
//! Live WebSocket event-stream wiring is an integration point (see
//! `start_account`); all decision logic is implemented here as testable
//! state machines and pure functions in house style.

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};
use crate::config::Config;
use crate::gateway::GatewayState;

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tracing::{info, warn};

// ============================================================================
// Extension configuration (config.channels.extensions["mattermost"])
// ============================================================================

/// Tri-state toggle used by `commands.native` / `commands.nativeSkills`
/// (upstream `boolean | "auto"`; `"auto"` resolves to `false` — opt-in).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SlashToggle {
    On,
    Off,
    #[default]
    Auto,
}

impl<'de> Deserialize<'de> for SlashToggle {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(match value {
            serde_json::Value::Bool(true) => SlashToggle::On,
            serde_json::Value::Bool(false) => SlashToggle::Off,
            _ => SlashToggle::Auto,
        })
    }
}

impl Serialize for SlashToggle {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            SlashToggle::On => serializer.serialize_bool(true),
            SlashToggle::Off => serializer.serialize_bool(false),
            SlashToggle::Auto => serializer.serialize_str("auto"),
        }
    }
}

/// Slash-command section of the Mattermost extension config
/// (upstream `MattermostSlashCommandConfig`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MattermostCommandsConfig {
    #[serde(default)]
    pub native: SlashToggle,
    #[serde(default)]
    pub native_skills: SlashToggle,
    pub callback_path: Option<String>,
    pub callback_url: Option<String>,
}

/// Mattermost channel configuration read from the flattened
/// `channels.extensions` map (there is no typed `ChannelsConfig` entry).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MattermostExtensionConfig {
    pub enabled: Option<bool>,
    pub server_url: Option<String>,
    /// Bot access token (aliases: `botToken`).
    pub token: Option<String>,
    pub bot_token: Option<String>,
    pub commands: Option<MattermostCommandsConfig>,
    /// Markdown table handling for outbound text (`bullets` converts
    /// pipe tables into bullet lists for narrow clients).
    pub table_mode: Option<String>,
}

impl MattermostExtensionConfig {
    pub fn effective_token(&self) -> Option<&str> {
        self.token.as_deref().or(self.bot_token.as_deref())
    }
}

/// Resolves the Mattermost extension config from the channels extensions map.
/// Accepts the `mattermost` key.
pub fn resolve_mattermost_extension_config(config: &Config) -> Option<MattermostExtensionConfig> {
    let raw = config.channels.extensions.get("mattermost")?;
    serde_json::from_value(raw.clone()).ok()
}

// ============================================================================
// Slash commands (slash-commands.ts)
// ============================================================================

/// Mattermost rejects command descriptions above 128 UTF-8 bytes.
pub const MATTERMOST_COMMAND_DESCRIPTION_MAX_BYTES: usize = 128;
/// Callback registration method (`POST`).
pub const MATTERMOST_SLASH_POST_METHOD: &str = "P";

const DEFAULT_CALLBACK_PATH: &str = "/api/channels/mattermost/command";

/// A slash command to register natively with Mattermost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MattermostCommandSpec {
    pub trigger: &'static str,
    /// Original gateway command name (`oc_status` → `status`).
    pub original_name: &'static str,
    pub description: &'static str,
    pub auto_complete: bool,
    pub auto_complete_hint: Option<&'static str>,
}

/// Built-in commands mirrored from upstream `DEFAULT_COMMAND_SPECS`,
/// including `/oc_queue` (v2026.7.1 active-run queue controls).
pub fn default_command_specs() -> Vec<MattermostCommandSpec> {
    vec![
        MattermostCommandSpec {
            trigger: "oc_status",
            original_name: "status",
            description: "Show session status (model, usage, uptime)",
            auto_complete: true,
            auto_complete_hint: None,
        },
        MattermostCommandSpec {
            trigger: "oc_model",
            original_name: "model",
            description: "View or change the current model",
            auto_complete: true,
            auto_complete_hint: Some("[model-name] [--runtime runtime]"),
        },
        MattermostCommandSpec {
            trigger: "oc_models",
            original_name: "models",
            description: "Browse available models",
            auto_complete: true,
            auto_complete_hint: Some("[provider]"),
        },
        MattermostCommandSpec {
            trigger: "oc_new",
            original_name: "new",
            description: "Start a new conversation session",
            auto_complete: true,
            auto_complete_hint: None,
        },
        MattermostCommandSpec {
            trigger: "oc_help",
            original_name: "help",
            description: "Show available commands",
            auto_complete: true,
            auto_complete_hint: None,
        },
        MattermostCommandSpec {
            trigger: "oc_think",
            original_name: "think",
            description: "Set thinking/reasoning level",
            auto_complete: true,
            auto_complete_hint: Some("[off|low|medium|high]"),
        },
        MattermostCommandSpec {
            trigger: "oc_reasoning",
            original_name: "reasoning",
            description: "Toggle reasoning mode",
            auto_complete: true,
            auto_complete_hint: Some("[on|off]"),
        },
        MattermostCommandSpec {
            trigger: "oc_verbose",
            original_name: "verbose",
            description: "Toggle verbose mode",
            auto_complete: true,
            auto_complete_hint: Some("[on|off]"),
        },
        MattermostCommandSpec {
            trigger: "oc_queue",
            original_name: "queue",
            description: "Adjust active-run queue behavior",
            auto_complete: true,
            auto_complete_hint: Some(
                "[steer|followup|collect|interrupt] [debounce:2s] [cap:N] [drop:old|new|summarize]",
            ),
        },
    ]
}

/// Truncates a command description to the Mattermost 128-UTF-8-byte limit
/// without splitting a character (upstream
/// `truncateMattermostCommandDescription`). Portable descriptions stay intact
/// until this API boundary.
pub fn truncate_command_description(description: &str) -> String {
    if description.len() <= MATTERMOST_COMMAND_DESCRIPTION_MAX_BYTES {
        return description.to_string();
    }
    let mut bytes = 0usize;
    let mut end = 0usize;
    for ch in description.chars() {
        let char_bytes = ch.len_utf8();
        if bytes + char_bytes > MATTERMOST_COMMAND_DESCRIPTION_MAX_BYTES {
            break;
        }
        bytes += char_bytes;
        end += char_bytes;
    }
    description[..end].to_string()
}

/// `"/oc_status"` → `"oc_status"`.
pub fn normalize_slash_command_trigger(command: &str) -> String {
    command.trim_start_matches('/').trim().to_string()
}

/// Maps a trigger word back to the gateway command text
/// (`"oc_queue"`, `" collect drop:summarize "` → `"/queue collect drop:summarize"`).
pub fn resolve_command_text(
    trigger: &str,
    text: &str,
    trigger_map: Option<&HashMap<String, String>>,
) -> String {
    let command_name = trigger_map
        .and_then(|map| map.get(trigger).cloned())
        .unwrap_or_else(|| {
            trigger
                .strip_prefix("oc_")
                .unwrap_or(trigger)
                .to_string()
        });
    let args = text.trim();
    if args.is_empty() {
        format!("/{}", command_name)
    } else {
        format!("/{} {}", command_name, args)
    }
}

/// Whether native slash commands are enabled. `"auto"` resolves to `false`
/// for Mattermost (opt-in).
pub fn is_slash_commands_enabled(commands: &MattermostCommandsConfig) -> bool {
    matches!(commands.native, SlashToggle::On)
}

/// Ensures the callback path starts with a leading `/` so derived URLs are
/// never malformed (`http://host:portapi/...`).
pub fn normalize_callback_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return DEFAULT_CALLBACK_PATH.to_string();
    }
    if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{}", trimmed)
    }
}

fn is_wildcard_bind_host(raw_host: &str) -> bool {
    let trimmed = raw_host.trim();
    if trimmed.is_empty() {
        return false;
    }
    let host = if trimmed.starts_with('[') && trimmed.ends_with(']') {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    // Wildcard listen hosts are valid bind addresses but not routable callback
    // destinations; never emit `http://0.0.0.0:PORT/...`.
    matches!(host, "0.0.0.0" | "::" | "0:0:0:0:0:0:0:0" | "::0")
}

/// Builds the callback URL Mattermost will POST to when a command is invoked.
/// Explicit `callbackUrl` wins; otherwise derive from gateway host/port with
/// wildcard-bind hosts replaced by `localhost` and IPv6 literals bracketed.
pub fn resolve_callback_url(
    commands: &MattermostCommandsConfig,
    gateway_port: u16,
    gateway_host: Option<&str>,
) -> String {
    if let Some(url) = commands
        .callback_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        return url.to_string();
    }
    let mut host = match gateway_host {
        Some(h) if !h.trim().is_empty() && !is_wildcard_bind_host(h) => h.trim().to_string(),
        _ => "localhost".to_string(),
    };
    if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
        host = format!("[{}]", host);
    }
    let path = normalize_callback_path(commands.callback_path.as_deref().unwrap_or(""));
    format!("http://{}:{}{}", host, gateway_port, path)
}

// ============================================================================
// Command registration reconciliation (registerSlashCommands)
// ============================================================================

/// An existing custom command on the Mattermost team, as returned by
/// `GET /api/v4/commands?team_id=...&custom_only=true`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExistingMattermostCommand {
    pub id: String,
    pub token: String,
    pub team_id: String,
    pub trigger: String,
    #[serde(default)]
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub creator_id: Option<String>,
    #[serde(default)]
    pub delete_at: Option<i64>,
}

/// A registered command tracked for callback validation and cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredMattermostCommand {
    pub id: String,
    pub trigger: String,
    pub team_id: String,
    pub token: String,
    pub url: String,
    /// True when this process created the command and should delete it on
    /// shutdown.
    pub managed: bool,
}

/// Decision for one command trigger during startup reconciliation.
///
/// Registration must run (and complete) **before** callbacks are accepted:
/// callback validation routes on the tokens gathered here, and a failed
/// listing fails closed (no create/update attempted) so a partial token set
/// never silently rejects callbacks until restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandRegistrationPlan {
    /// Registered with the correct callback URL/method — reuse token as-is.
    Reuse { id: String, token: String },
    /// Owned command drifted (URL or method) — update in place, falling back
    /// to delete+recreate when the update fails.
    Update { id: String },
    /// No owned command exists — create it.
    Create,
    /// The trigger is owned by a non-OpenClaw integration — never mutate
    /// external integrations.
    SkipForeign,
}

/// Reconciles one command spec against the team's existing commands
/// (upstream `registerSlashCommands` per-spec decision logic).
pub fn plan_command_registration(
    existing: &[ExistingMattermostCommand],
    trigger: &str,
    creator_user_id: &str,
    callback_url: &str,
) -> CommandRegistrationPlan {
    let creator = creator_user_id.trim();
    let for_trigger: Vec<&ExistingMattermostCommand> =
        existing.iter().filter(|c| c.trigger == trigger).collect();
    let owned: Vec<&ExistingMattermostCommand> = for_trigger
        .iter()
        .copied()
        .filter(|c| c.creator_id.as_deref().map(str::trim) == Some(creator))
        .collect();
    let foreign_exists = for_trigger.len() > owned.len();

    if owned.is_empty() {
        if foreign_exists {
            return CommandRegistrationPlan::SkipForeign;
        }
        return CommandRegistrationPlan::Create;
    }
    // Multiple owned commands: use the first, leave extras untouched.
    let cmd = owned[0];
    let needs_update = cmd.url != callback_url || cmd.method != MATTERMOST_SLASH_POST_METHOD;
    if needs_update {
        CommandRegistrationPlan::Update { id: cmd.id.clone() }
    } else {
        CommandRegistrationPlan::Reuse {
            id: cmd.id.clone(),
            token: cmd.token.clone(),
        }
    }
}

/// True when a fetched command has been soft-deleted upstream
/// (`delete_at > 0`).
pub fn is_deleted_mattermost_command(delete_at: Option<i64>) -> bool {
    matches!(delete_at, Some(ts) if ts > 0)
}

// ============================================================================
// Callback payload parsing + bounded body read (slash-http.ts)
// ============================================================================

/// Payload sent by Mattermost when a slash command is invoked. Arrives as
/// `application/x-www-form-urlencoded` or `application/json`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MattermostSlashCommandPayload {
    pub token: String,
    pub team_id: String,
    pub team_domain: Option<String>,
    pub channel_id: String,
    pub channel_name: Option<String>,
    pub user_id: String,
    pub user_name: Option<String>,
    /// e.g. `"/oc_status"`.
    pub command: String,
    pub text: String,
    pub trigger_id: Option<String>,
    pub response_url: Option<String>,
}

/// Parses a slash-command callback body. Returns `None` when required fields
/// (token, team_id, channel_id, user_id, command) are missing.
pub fn parse_slash_command_payload(
    body: &str,
    content_type: Option<&str>,
) -> Option<MattermostSlashCommandPayload> {
    if body.is_empty() {
        return None;
    }
    if content_type.is_some_and(|ct| ct.contains("application/json")) {
        let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
        let get = |key: &str| parsed.get(key).and_then(|v| v.as_str()).map(String::from);
        let payload = MattermostSlashCommandPayload {
            token: get("token")?,
            team_id: get("team_id")?,
            team_domain: get("team_domain"),
            channel_id: get("channel_id")?,
            channel_name: get("channel_name"),
            user_id: get("user_id")?,
            user_name: get("user_name"),
            command: get("command")?,
            text: get("text").unwrap_or_default(),
            trigger_id: get("trigger_id"),
            response_url: get("response_url"),
        };
        if payload.token.is_empty()
            || payload.team_id.is_empty()
            || payload.channel_id.is_empty()
            || payload.user_id.is_empty()
            || payload.command.is_empty()
        {
            return None;
        }
        return Some(payload);
    }
    // Default: application/x-www-form-urlencoded.
    let mut fields: HashMap<String, String> = HashMap::new();
    for (key, value) in url::form_urlencoded::parse(body.as_bytes()) {
        fields.insert(key.into_owned(), value.into_owned());
    }
    let required = |key: &str| fields.get(key).filter(|v| !v.is_empty()).cloned();
    Some(MattermostSlashCommandPayload {
        token: required("token")?,
        team_id: required("team_id")?,
        team_domain: fields.get("team_domain").cloned(),
        channel_id: required("channel_id")?,
        channel_name: fields.get("channel_name").cloned(),
        user_id: required("user_id")?,
        user_name: fields.get("user_name").cloned(),
        command: required("command")?,
        text: fields.get("text").cloned().unwrap_or_default(),
        trigger_id: fields.get("trigger_id").cloned(),
        response_url: fields.get("response_url").cloned(),
    })
}

/// Maximum callback body size accepted before parsing (64 KiB).
pub const MATTERMOST_CALLBACK_MAX_BODY_BYTES: usize = 64 * 1024;
/// Body-read timeout (5 s): a slow/never-finishing client must not tie up
/// the callback handler indefinitely (Slowloris).
pub const MATTERMOST_CALLBACK_BODY_TIMEOUT_MS: u64 = 5_000;

/// Outcome of pushing a chunk into [`BoundedBodyReader`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyReadStatus {
    /// Keep reading.
    Continue,
    /// `413 Payload Too Large`.
    TooLarge,
    /// `408 Request Body Timeout`.
    TimedOut,
}

/// Incremental bounded body reader: enforces a byte cap and a wall-clock
/// deadline across chunk arrivals. The HTTP server integration feeds chunks
/// via [`BoundedBodyReader::push`]; decision logic is pure so both limits are
/// unit-testable without sockets.
#[derive(Debug)]
pub struct BoundedBodyReader {
    max_bytes: usize,
    deadline_ms: u64,
    buffer: Vec<u8>,
}

impl BoundedBodyReader {
    pub fn new(max_bytes: usize, started_at_ms: u64, timeout_ms: u64) -> Self {
        Self {
            max_bytes,
            deadline_ms: started_at_ms.saturating_add(timeout_ms),
            buffer: Vec::new(),
        }
    }

    /// Default policy reader (64 KiB / 5 s).
    pub fn with_defaults(started_at_ms: u64) -> Self {
        Self::new(
            MATTERMOST_CALLBACK_MAX_BODY_BYTES,
            started_at_ms,
            MATTERMOST_CALLBACK_BODY_TIMEOUT_MS,
        )
    }

    pub fn push(&mut self, chunk: &[u8], now_ms: u64) -> BodyReadStatus {
        if now_ms > self.deadline_ms {
            return BodyReadStatus::TimedOut;
        }
        if self.buffer.len() + chunk.len() > self.max_bytes {
            return BodyReadStatus::TooLarge;
        }
        self.buffer.extend_from_slice(chunk);
        BodyReadStatus::Continue
    }

    pub fn into_string(self) -> String {
        String::from_utf8_lossy(&self.buffer).into_owned()
    }
}

// ============================================================================
// Callback validation refresh + rate limit (slash-http.ts)
// ============================================================================

const COMMAND_VALIDATION_FAILURE_CACHE_MS: u64 = 5_000;
const COMMAND_VALIDATION_FAILURE_CACHE_MAX_KEYS: usize = 2_000;
const COMMAND_VALIDATION_LOOKUP_BURST: u32 = 20;
const COMMAND_VALIDATION_LOOKUP_REFILL_MS: u64 = 500;
const COMMAND_VALIDATION_LOOKUP_LIMIT_LOG_MS: u64 = 5_000;
const COMMAND_VALIDATION_LOOKUP_RATE_LIMIT_MAX_KEYS: usize = 2_000;

/// Decision for an inbound slash callback token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCallbackValidation {
    /// Token matches the registration snapshot — accept immediately.
    AcceptKnownToken,
    /// Token drifted (Mattermost rotated it): refresh the registration by
    /// re-fetching the command from the API before deciding
    /// ("refresh native slash-command registrations before accepting
    /// callbacks").
    RefreshLookup { command_id: String },
    /// A recent lookup for this command failed — reject without hitting the
    /// API again (negative cache, 5 s TTL).
    RejectCachedFailure,
    /// Lookup budget exhausted for this command (burst 20, refill 500 ms).
    /// `should_log` throttles the warning to once per 5 s.
    RejectRateLimited { should_log: bool },
    /// No registered command matches this team/trigger at all.
    RejectUnknown,
}

#[derive(Debug)]
struct LookupRateLimitEntry {
    tokens: u32,
    updated_at: u64,
    /// `None` until this key has actually been logged as rate-limited. Using a
    /// `0` sentinel instead conflated "never logged" with "logged at t=0", so
    /// the very first limit hit was suppressed whenever it happened within
    /// `COMMAND_VALIDATION_LOOKUP_LIMIT_LOG_MS` of the clock origin.
    last_limited_log_at: Option<u64>,
}

/// Guards live command-validation lookups triggered by token drift: a
/// token-bucket rate limit per command plus a short negative cache, both
/// bounded in key count so hostile callback floods cannot grow memory.
#[derive(Debug, Default)]
pub struct CommandValidationGuard {
    failure_cache: HashMap<String, u64>,
    rate_limits: HashMap<String, LookupRateLimitEntry>,
}

impl CommandValidationGuard {
    pub fn new() -> Self {
        Self::default()
    }

    fn sweep(&mut self, now_ms: u64) {
        self.failure_cache.retain(|_, expires| *expires > now_ms);
        while self.failure_cache.len() > COMMAND_VALIDATION_FAILURE_CACHE_MAX_KEYS {
            let Some(key) = self.failure_cache.keys().next().cloned() else {
                break;
            };
            self.failure_cache.remove(&key);
        }
        let stale_after =
            COMMAND_VALIDATION_LOOKUP_REFILL_MS * u64::from(COMMAND_VALIDATION_LOOKUP_BURST) * 2;
        self.rate_limits
            .retain(|_, e| now_ms.saturating_sub(e.updated_at) <= stale_after);
        while self.rate_limits.len() > COMMAND_VALIDATION_LOOKUP_RATE_LIMIT_MAX_KEYS {
            let Some(key) = self.rate_limits.keys().next().cloned() else {
                break;
            };
            self.rate_limits.remove(&key);
        }
    }

    /// Records a failed live lookup so repeats within 5 s short-circuit.
    pub fn cache_failure(&mut self, key: &str, now_ms: u64) {
        self.sweep(now_ms);
        self.failure_cache
            .insert(key.to_string(), now_ms + COMMAND_VALIDATION_FAILURE_CACHE_MS);
    }

    pub fn has_cached_failure(&mut self, key: &str, now_ms: u64) -> bool {
        self.sweep(now_ms);
        self.failure_cache
            .get(key)
            .is_some_and(|expires| *expires > now_ms)
    }

    /// Reserves one lookup slot for `key` (token bucket: burst 20, one token
    /// refilled per 500 ms). Returns `Err(should_log)` when limited.
    pub fn reserve_lookup(&mut self, key: &str, now_ms: u64) -> Result<(), bool> {
        self.sweep(now_ms);
        let entry = self
            .rate_limits
            .entry(key.to_string())
            .or_insert(LookupRateLimitEntry {
                tokens: COMMAND_VALIDATION_LOOKUP_BURST,
                updated_at: now_ms,
                last_limited_log_at: None,
            });
        let refill = now_ms.saturating_sub(entry.updated_at) / COMMAND_VALIDATION_LOOKUP_REFILL_MS;
        if refill > 0 {
            entry.tokens =
                (u64::from(entry.tokens) + refill).min(u64::from(COMMAND_VALIDATION_LOOKUP_BURST))
                    as u32;
            entry.updated_at = now_ms;
        }
        if entry.tokens > 0 {
            entry.tokens -= 1;
            return Ok(());
        }
        // The first time a key is limited always logs; afterwards the log is
        // throttled to one line per COMMAND_VALIDATION_LOOKUP_LIMIT_LOG_MS.
        let should_log = match entry.last_limited_log_at {
            None => true,
            Some(prev) => {
                now_ms.saturating_sub(prev) >= COMMAND_VALIDATION_LOOKUP_LIMIT_LOG_MS
            }
        };
        if should_log {
            entry.last_limited_log_at = Some(now_ms);
        }
        Err(should_log)
    }

    /// Validates one callback payload against the registration snapshot.
    pub fn validate(
        &mut self,
        registered: &[RegisteredMattermostCommand],
        payload: &MattermostSlashCommandPayload,
        now_ms: u64,
    ) -> SlashCallbackValidation {
        let trigger = normalize_slash_command_trigger(&payload.command);
        let Some(cmd) = registered
            .iter()
            .find(|c| c.team_id == payload.team_id && c.trigger == trigger)
        else {
            return SlashCallbackValidation::RejectUnknown;
        };
        if constant_time_eq(cmd.token.as_bytes(), payload.token.as_bytes()) {
            return SlashCallbackValidation::AcceptKnownToken;
        }
        let key = format!("{}:{}", cmd.team_id, cmd.id);
        if self.has_cached_failure(&key, now_ms) {
            return SlashCallbackValidation::RejectCachedFailure;
        }
        match self.reserve_lookup(&key, now_ms) {
            Ok(()) => SlashCallbackValidation::RefreshLookup {
                command_id: cmd.id.clone(),
            },
            Err(should_log) => SlashCallbackValidation::RejectRateLimited { should_log },
        }
    }
}

/// Constant-time byte comparison for callback tokens.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ============================================================================
// Channel-vs-group identification + threads (monitor-gating.ts / monitor.ts)
// ============================================================================

/// Chat kind derived from the Mattermost `channel_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MattermostChatKind {
    Direct,
    Group,
    Channel,
}

/// Maps the Mattermost channel type to a chat kind:
/// `D` → direct, `G`/`P` → group, everything else (`O`, unknown) → channel.
/// Missing/empty types default to direct.
pub fn map_channel_type_to_chat_kind(channel_type: Option<&str>) -> MattermostChatKind {
    let normalized = channel_type.map(str::trim).unwrap_or("").to_uppercase();
    match normalized.as_str() {
        "" | "D" => MattermostChatKind::Direct,
        "G" | "P" => MattermostChatKind::Group,
        _ => MattermostChatKind::Channel,
    }
}

/// Resolves the trusted chat kind: a present `channel_type` from the event
/// payload wins over the caller's fallback.
pub fn resolve_trusted_chat_kind(
    channel_type: Option<&str>,
    fallback: MattermostChatKind,
) -> MattermostChatKind {
    match channel_type.map(str::trim) {
        Some(ct) if !ct.is_empty() => map_channel_type_to_chat_kind(Some(ct)),
        _ => fallback,
    }
}

/// A Mattermost post with a non-empty `type` is a system post
/// (join/leave/header-change…), never a user message.
pub fn is_system_post(post_type: Option<&str>) -> bool {
    post_type.map(str::trim).is_some_and(|t| !t.is_empty())
}

/// Resolves the `root_id` for an outbound reply so message-tool replies stay
/// in threads: direct chats never thread; group/channel replies anchor to the
/// inbound thread root, falling back to the message being replied to.
pub fn resolve_reply_root_id(
    kind: MattermostChatKind,
    thread_root_id: Option<&str>,
    reply_to_id: Option<&str>,
) -> Option<String> {
    if kind == MattermostChatKind::Direct {
        return None;
    }
    let non_empty = |v: Option<&str>| v.map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    non_empty(thread_root_id).or_else(|| non_empty(reply_to_id))
}

// ============================================================================
// Thread participation across restarts (thread-participation.ts)
// ============================================================================

/// 7-day idle expiry for thread participation.
pub const THREAD_PARTICIPATION_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const THREAD_PARTICIPATION_MAX_ENTRIES: usize = 1000;

/// SQLite-backed record of threads the bot has replied in, so it can
/// auto-respond to thread follow-ups without a re-mention **across
/// restarts**. Entries expire after 7 idle days and the store is capped at
/// 1000 rows (oldest evicted first).
pub struct MattermostThreadParticipationStore {
    conn: rusqlite::Connection,
}

impl MattermostThreadParticipationStore {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(rusqlite::Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(rusqlite::Connection::open_in_memory()?)
    }

    fn init(conn: rusqlite::Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS mattermost_thread_participation (
                key TEXT PRIMARY KEY,
                agent_id TEXT,
                replied_at INTEGER NOT NULL
            );",
        )?;
        Ok(Self { conn })
    }

    fn key(account_id: &str, channel_id: &str, thread_root_id: &str) -> String {
        format!("{}:{}:{}", account_id, channel_id, thread_root_id)
    }

    /// Records that the bot replied in a thread. Empty components are ignored.
    pub fn record(
        &self,
        account_id: &str,
        channel_id: &str,
        thread_root_id: &str,
        agent_id: Option<&str>,
        now_ms: u64,
    ) -> Result<()> {
        if account_id.is_empty() || channel_id.is_empty() || thread_root_id.is_empty() {
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO mattermost_thread_participation (key, agent_id, replied_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET agent_id = ?2, replied_at = ?3",
            rusqlite::params![
                Self::key(account_id, channel_id, thread_root_id),
                agent_id,
                now_ms as i64
            ],
        )?;
        self.prune(now_ms)?;
        Ok(())
    }

    /// True when the bot participated in the thread within the last 7 days.
    pub fn has_participation(
        &self,
        account_id: &str,
        channel_id: &str,
        thread_root_id: &str,
        now_ms: u64,
    ) -> Result<bool> {
        if account_id.is_empty() || channel_id.is_empty() || thread_root_id.is_empty() {
            return Ok(false);
        }
        let cutoff = now_ms.saturating_sub(THREAD_PARTICIPATION_TTL_MS) as i64;
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM mattermost_thread_participation
             WHERE key = ?1 AND replied_at > ?2",
            rusqlite::params![Self::key(account_id, channel_id, thread_root_id), cutoff],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    fn prune(&self, now_ms: u64) -> Result<()> {
        let cutoff = now_ms.saturating_sub(THREAD_PARTICIPATION_TTL_MS) as i64;
        self.conn.execute(
            "DELETE FROM mattermost_thread_participation WHERE replied_at <= ?1",
            rusqlite::params![cutoff],
        )?;
        self.conn.execute(
            "DELETE FROM mattermost_thread_participation WHERE key NOT IN (
                SELECT key FROM mattermost_thread_participation
                ORDER BY replied_at DESC LIMIT ?1
            )",
            rusqlite::params![THREAD_PARTICIPATION_MAX_ENTRIES as i64],
        )?;
        Ok(())
    }
}

// ============================================================================
// Post payload + attachments (client.ts / send.ts)
// ============================================================================

/// Builds the JSON payload for `POST /api/v4/posts`. `root_id` keeps replies
/// in threads; `file_ids` attaches previously uploaded files.
pub fn build_post_payload(
    channel_id: &str,
    message: &str,
    root_id: Option<&str>,
    file_ids: &[String],
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "channel_id": channel_id,
        "message": message,
    });
    if let Some(root) = root_id.map(str::trim).filter(|r| !r.is_empty()) {
        payload["root_id"] = serde_json::Value::String(root.to_string());
    }
    if !file_ids.is_empty() {
        payload["file_ids"] = serde_json::Value::Array(
            file_ids
                .iter()
                .map(|id| serde_json::Value::String(id.clone()))
                .collect(),
        );
    }
    payload
}

// ============================================================================
// Mattermost Channel Implementation
// ============================================================================

/// Mattermost channel integration via the Mattermost REST API v4.
///
/// Communicates with a Mattermost server using a bot access token or
/// personal access token. Messages are sent via `POST /api/v4/posts`;
/// attachments upload through `POST /api/v4/files` and attach via
/// `file_ids`.
///
/// Mattermost API docs: <https://api.mattermost.com/>
pub struct MattermostChannel {
    /// Mattermost server URL (e.g. `https://mattermost.example.com`).
    server_url: Option<String>,
    /// Bot access token or personal access token.
    token: Option<String>,
    /// Whether this channel is enabled.
    enabled: Option<bool>,
    /// HTTP client for API calls.
    client: Client,
}

impl MattermostChannel {
    pub fn new() -> Self {
        Self {
            server_url: None,
            token: None,
            enabled: None,
            client: Client::new(),
        }
    }

    /// Create a configured Mattermost channel.
    pub fn with_config(server_url: String, token: String) -> Self {
        Self {
            server_url: Some(server_url),
            token: Some(token),
            enabled: Some(true),
            client: Client::new(),
        }
    }

    /// Create a channel from the flattened extensions config
    /// (`channels.extensions["mattermost"]`).
    pub fn from_config(config: &Config) -> Self {
        match resolve_mattermost_extension_config(config) {
            Some(ext) => Self {
                enabled: ext.enabled,
                token: ext.effective_token().map(String::from),
                server_url: ext.server_url,
                client: Client::new(),
            },
            None => Self::new(),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    fn api_base(&self) -> Result<String> {
        let server_url = self
            .server_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Mattermost server_url not configured"))?;
        Ok(format!("{}/api/v4", server_url.trim_end_matches('/')))
    }

    fn auth_token(&self) -> Result<&str> {
        self.token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Mattermost token not configured"))
    }

    /// Uploads a file to a channel via `POST /api/v4/files` and returns its
    /// `file_id` for use in a post's `file_ids` (attachments via upload API,
    /// v2026.7.1 row 88).
    pub async fn upload_file(
        &self,
        channel_id: &str,
        file_name: &str,
        bytes: Vec<u8>,
    ) -> Result<String> {
        let url = format!("{}/files", self.api_base()?);
        let token = self.auth_token()?;
        let name = if file_name.trim().is_empty() {
            "upload".to_string()
        } else {
            file_name.trim().to_string()
        };
        let part = reqwest::multipart::Part::bytes(bytes).file_name(name);
        let form = reqwest::multipart::Form::new()
            .text("channel_id", channel_id.to_string())
            .part("files", part);
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .multipart(form)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Mattermost file upload failed ({}): {}", status, text);
        }
        let body: serde_json::Value = resp.json().await?;
        body["file_infos"][0]["id"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("Mattermost file upload returned no file id"))
    }

    /// Creates a post, optionally threaded (`root_id`) and with attachments
    /// (`file_ids`).
    pub async fn create_post(
        &self,
        channel_id: &str,
        message: &str,
        root_id: Option<&str>,
        file_ids: &[String],
    ) -> Result<()> {
        let url = format!("{}/posts", self.api_base()?);
        let token = self.auth_token()?;
        let body = build_post_payload(channel_id, message, root_id, file_ids);
        info!(channel_id = %channel_id, threaded = root_id.is_some(), "Mattermost: creating post");
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Mattermost post creation failed ({}): {}", status, text);
        }
        Ok(())
    }
}

#[async_trait]
impl ChannelPlugin for MattermostChannel {
    fn id(&self) -> &str {
        "mattermost"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Mattermost".to_string(),
            description: "Mattermost channel via REST API v4".to_string(),
            enabled: self.is_enabled(),
            multi_account: true,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![
            ChannelCapability::SendText,
            ChannelCapability::ReceiveText,
            ChannelCapability::SendMedia,
            ChannelCapability::ReceiveMedia,
            ChannelCapability::Reactions,
            ChannelCapability::Groups,
            ChannelCapability::Threads,
            ChannelCapability::EditMessage,
            ChannelCapability::DeleteMessage,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        let server_url = match &self.server_url {
            Some(url) => url,
            None => {
                warn!("Mattermost channel enabled but no server_url configured");
                return Ok(());
            }
        };

        let token = match &self.token {
            Some(t) => t,
            None => {
                warn!("Mattermost channel enabled but no token configured");
                return Ok(());
            }
        };

        info!(server_url = %server_url, "Mattermost channel starting");

        // Verify credentials by calling the /users/me endpoint.
        let me_url = format!("{}/api/v4/users/me", server_url.trim_end_matches('/'));

        match self
            .client
            .get(&me_url)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = resp.json().await.unwrap_or_default();
                let username = body["username"].as_str().unwrap_or("unknown");
                info!(username = %username, "Mattermost: authenticated successfully");
            }
            Ok(resp) => {
                warn!("Mattermost: auth check returned status {}", resp.status());
            }
            Err(e) => {
                warn!("Mattermost: failed to verify credentials: {}", e);
            }
        }

        // Integration point: the live event stream connects to
        // `wss://<server>/api/v4/websocket`. On connect, native slash
        // commands must be reconciled via `plan_command_registration` before
        // any callback is accepted (see `CommandValidationGuard`), and
        // inbound `posted` events flow through `is_system_post`,
        // `resolve_trusted_chat_kind`, and the thread-participation store.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.is_enabled() {
            info!("Mattermost channel stopping");
            // Integration point: close the WebSocket event stream and delete
            // managed slash commands (RegisteredMattermostCommand::managed).
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        // `to` is a Mattermost channel ID (26-char alphanumeric string).
        self.create_post(to, message, None, &[]).await
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mattermost_description_truncates_on_char_boundary() {
        let short = "Show status";
        assert_eq!(truncate_command_description(short), short);
        // 127 ASCII bytes + a 3-byte char must drop the wide char, not split it.
        let long = format!("{}€", "a".repeat(127));
        let truncated = truncate_command_description(&long);
        assert_eq!(truncated.len(), 127);
        assert!(truncated.chars().all(|c| c == 'a'));
    }

    #[test]
    fn mattermost_default_specs_include_oc_queue() {
        let specs = default_command_specs();
        let queue = specs.iter().find(|s| s.trigger == "oc_queue").unwrap();
        assert_eq!(queue.original_name, "queue");
        assert!(queue.auto_complete_hint.unwrap().contains("drop:old|new|summarize"));
    }

    #[test]
    fn mattermost_resolve_command_text_maps_trigger() {
        let mut map = HashMap::new();
        map.insert("oc_queue".to_string(), "queue".to_string());
        assert_eq!(
            resolve_command_text("oc_queue", " collect drop:summarize ", Some(&map)),
            "/queue collect drop:summarize"
        );
        assert_eq!(resolve_command_text("oc_status", "", None), "/status");
        assert_eq!(resolve_command_text("custom", "x", None), "/custom x");
    }

    #[test]
    fn mattermost_callback_url_replaces_wildcard_hosts_and_brackets_ipv6() {
        let cfg = MattermostCommandsConfig::default();
        assert_eq!(
            resolve_callback_url(&cfg, 3015, Some("0.0.0.0")),
            "http://localhost:3015/api/channels/mattermost/command"
        );
        assert_eq!(
            resolve_callback_url(&cfg, 3015, Some("::1")),
            "http://[::1]:3015/api/channels/mattermost/command"
        );
        let explicit = MattermostCommandsConfig {
            callback_url: Some("https://proxy.example/cb".to_string()),
            ..Default::default()
        };
        assert_eq!(resolve_callback_url(&explicit, 1, None), "https://proxy.example/cb");
        let relative = MattermostCommandsConfig {
            callback_path: Some("api/x".to_string()),
            ..Default::default()
        };
        assert_eq!(resolve_callback_url(&relative, 80, None), "http://localhost:80/api/x");
    }

    #[test]
    fn mattermost_slash_auto_is_opt_in() {
        assert!(!is_slash_commands_enabled(&MattermostCommandsConfig::default()));
        assert!(is_slash_commands_enabled(&MattermostCommandsConfig {
            native: SlashToggle::On,
            ..Default::default()
        }));
    }

    fn existing(trigger: &str, creator: &str, url: &str, method: &str) -> ExistingMattermostCommand {
        ExistingMattermostCommand {
            id: format!("id-{}", trigger),
            token: format!("tok-{}", trigger),
            team_id: "team".to_string(),
            trigger: trigger.to_string(),
            method: method.to_string(),
            url: url.to_string(),
            creator_id: Some(creator.to_string()),
            delete_at: None,
        }
    }

    #[test]
    fn mattermost_registration_plan_reuse_update_create_skip() {
        let cb = "http://localhost:3015/cb";
        // Reuse: owned, same url + POST method.
        let cmds = vec![existing("oc_status", "bot", cb, "P")];
        assert_eq!(
            plan_command_registration(&cmds, "oc_status", "bot", cb),
            CommandRegistrationPlan::Reuse {
                id: "id-oc_status".to_string(),
                token: "tok-oc_status".to_string()
            }
        );
        // Update: owned but URL drifted (callback migration).
        let cmds = vec![existing("oc_status", "bot", "http://old/cb", "P")];
        assert_eq!(
            plan_command_registration(&cmds, "oc_status", "bot", cb),
            CommandRegistrationPlan::Update { id: "id-oc_status".to_string() }
        );
        // SkipForeign: only a non-OpenClaw command owns the trigger.
        let cmds = vec![existing("oc_status", "someone-else", cb, "P")];
        assert_eq!(
            plan_command_registration(&cmds, "oc_status", "bot", cb),
            CommandRegistrationPlan::SkipForeign
        );
        // Create: no command for the trigger.
        assert_eq!(
            plan_command_registration(&[], "oc_status", "bot", cb),
            CommandRegistrationPlan::Create
        );
    }

    #[test]
    fn mattermost_parse_payload_form_and_json() {
        let form = "token=t1&team_id=tm&channel_id=ch&user_id=u&command=%2Foc_status&text=hi";
        let payload = parse_slash_command_payload(form, None).unwrap();
        assert_eq!(payload.command, "/oc_status");
        assert_eq!(payload.text, "hi");

        let json = r#"{"token":"t1","team_id":"tm","channel_id":"ch","user_id":"u","command":"/oc_model","text":""}"#;
        let payload = parse_slash_command_payload(json, Some("application/json")).unwrap();
        assert_eq!(payload.command, "/oc_model");

        // Missing required field → None.
        assert!(parse_slash_command_payload("token=t1&team_id=tm", None).is_none());
        assert!(parse_slash_command_payload("", None).is_none());
    }

    #[test]
    fn mattermost_body_reader_enforces_size_and_timeout() {
        let mut reader = BoundedBodyReader::new(10, 0, 5_000);
        assert_eq!(reader.push(b"12345", 100), BodyReadStatus::Continue);
        assert_eq!(reader.push(b"678901", 200), BodyReadStatus::TooLarge);

        let mut reader = BoundedBodyReader::new(1024, 0, 5_000);
        assert_eq!(reader.push(b"x", 5_001), BodyReadStatus::TimedOut);

        let mut reader = BoundedBodyReader::with_defaults(0);
        assert_eq!(reader.push(b"ok", 4_999), BodyReadStatus::Continue);
        assert_eq!(reader.into_string(), "ok");
    }

    fn registered(trigger: &str, token: &str) -> RegisteredMattermostCommand {
        RegisteredMattermostCommand {
            id: format!("id-{}", trigger),
            trigger: trigger.to_string(),
            team_id: "team".to_string(),
            token: token.to_string(),
            url: "http://localhost/cb".to_string(),
            managed: true,
        }
    }

    fn payload(trigger: &str, token: &str) -> MattermostSlashCommandPayload {
        MattermostSlashCommandPayload {
            token: token.to_string(),
            team_id: "team".to_string(),
            channel_id: "ch".to_string(),
            user_id: "u".to_string(),
            command: format!("/{}", trigger),
            ..Default::default()
        }
    }

    #[test]
    fn mattermost_callback_validation_refresh_and_rate_limit() {
        let mut guard = CommandValidationGuard::new();
        let cmds = vec![registered("oc_status", "good-token")];

        // Known token accepted directly.
        assert_eq!(
            guard.validate(&cmds, &payload("oc_status", "good-token"), 0),
            SlashCallbackValidation::AcceptKnownToken
        );
        // Rotated token triggers a live refresh lookup.
        assert_eq!(
            guard.validate(&cmds, &payload("oc_status", "rotated"), 0),
            SlashCallbackValidation::RefreshLookup { command_id: "id-oc_status".to_string() }
        );
        // Unknown trigger rejected.
        assert_eq!(
            guard.validate(&cmds, &payload("oc_nope", "x"), 0),
            SlashCallbackValidation::RejectUnknown
        );
        // Burst exhaustion: 19 more lookups drain the bucket, then limited.
        for _ in 0..19 {
            let v = guard.validate(&cmds, &payload("oc_status", "rotated"), 1);
            assert!(matches!(v, SlashCallbackValidation::RefreshLookup { .. }));
        }
        assert_eq!(
            guard.validate(&cmds, &payload("oc_status", "rotated"), 1),
            SlashCallbackValidation::RejectRateLimited { should_log: true }
        );
        // Limited-log throttled to once per 5 s.
        assert_eq!(
            guard.validate(&cmds, &payload("oc_status", "rotated"), 2),
            SlashCallbackValidation::RejectRateLimited { should_log: false }
        );
        // Refill: one token per 500 ms.
        assert!(matches!(
            guard.validate(&cmds, &payload("oc_status", "rotated"), 600),
            SlashCallbackValidation::RefreshLookup { .. }
        ));
    }

    #[test]
    fn mattermost_validation_failure_cache_expires() {
        let mut guard = CommandValidationGuard::new();
        let cmds = vec![registered("oc_status", "good-token")];
        guard.cache_failure("team:id-oc_status", 0);
        assert_eq!(
            guard.validate(&cmds, &payload("oc_status", "rotated"), 100),
            SlashCallbackValidation::RejectCachedFailure
        );
        // After the 5 s TTL the guard falls back to live refresh.
        assert!(matches!(
            guard.validate(&cmds, &payload("oc_status", "rotated"), 5_100),
            SlashCallbackValidation::RefreshLookup { .. }
        ));
    }

    #[test]
    fn mattermost_chat_kind_mapping() {
        assert_eq!(map_channel_type_to_chat_kind(Some("D")), MattermostChatKind::Direct);
        assert_eq!(map_channel_type_to_chat_kind(Some("G")), MattermostChatKind::Group);
        assert_eq!(map_channel_type_to_chat_kind(Some("P")), MattermostChatKind::Group);
        assert_eq!(map_channel_type_to_chat_kind(Some("O")), MattermostChatKind::Channel);
        assert_eq!(map_channel_type_to_chat_kind(Some("o")), MattermostChatKind::Channel);
        assert_eq!(map_channel_type_to_chat_kind(None), MattermostChatKind::Direct);
        assert_eq!(
            resolve_trusted_chat_kind(Some("O"), MattermostChatKind::Direct),
            MattermostChatKind::Channel
        );
        assert_eq!(
            resolve_trusted_chat_kind(Some("  "), MattermostChatKind::Group),
            MattermostChatKind::Group
        );
    }

    #[test]
    fn mattermost_system_posts_detected() {
        assert!(is_system_post(Some("system_join_channel")));
        assert!(!is_system_post(Some("")));
        assert!(!is_system_post(None));
    }

    #[test]
    fn mattermost_reply_root_keeps_threads() {
        // Direct chats never thread.
        assert_eq!(
            resolve_reply_root_id(MattermostChatKind::Direct, Some("root"), Some("msg")),
            None
        );
        // Thread root wins over reply-to.
        assert_eq!(
            resolve_reply_root_id(MattermostChatKind::Channel, Some("root"), Some("msg")),
            Some("root".to_string())
        );
        // Fallback to the message being replied to.
        assert_eq!(
            resolve_reply_root_id(MattermostChatKind::Group, None, Some("msg")),
            Some("msg".to_string())
        );
        assert_eq!(resolve_reply_root_id(MattermostChatKind::Group, None, None), None);
    }

    #[test]
    fn mattermost_thread_participation_persists_and_expires() {
        let store = MattermostThreadParticipationStore::open_in_memory().unwrap();
        store.record("acct", "chan", "root1", Some("agent"), 1_000).unwrap();
        assert!(store.has_participation("acct", "chan", "root1", 2_000).unwrap());
        assert!(!store.has_participation("acct", "chan", "other", 2_000).unwrap());
        // 7-day idle expiry.
        let later = 1_000 + THREAD_PARTICIPATION_TTL_MS + 1;
        assert!(!store.has_participation("acct", "chan", "root1", later).unwrap());
        // Empty components are no-ops.
        store.record("", "chan", "root", None, 0).unwrap();
        assert!(!store.has_participation("", "chan", "root", 0).unwrap());
    }

    #[test]
    fn mattermost_post_payload_includes_root_and_files() {
        let payload = build_post_payload(
            "chan",
            "hello",
            Some("root-id"),
            &["f1".to_string(), "f2".to_string()],
        );
        assert_eq!(payload["channel_id"], "chan");
        assert_eq!(payload["root_id"], "root-id");
        assert_eq!(payload["file_ids"][1], "f2");
        let plain = build_post_payload("chan", "hello", None, &[]);
        assert!(plain.get("root_id").is_none());
        assert!(plain.get("file_ids").is_none());
    }

    #[test]
    fn mattermost_extension_config_parses_aliases() {
        let raw = serde_json::json!({
            "enabled": true,
            "serverUrl": "https://mm.example.com",
            "botToken": "tok",
            "commands": { "native": true, "callbackPath": "cb" }
        });
        let cfg: MattermostExtensionConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(cfg.effective_token(), Some("tok"));
        let commands = cfg.commands.unwrap();
        assert!(is_slash_commands_enabled(&commands));
        assert_eq!(normalize_callback_path(commands.callback_path.as_deref().unwrap()), "/cb");
    }
}
