//! Slack channel implementation.
//!
//! Ports the behavior of the OpenClaw Slack extension (TypeScript, tag
//! v2026.7.1, `extensions/slack/src/`) to idiomatic Rust. Live Socket Mode
//! wiring is not present yet; every behavior is implemented as testable pure
//! logic + resolvers that the future socket loop calls. Each section cites
//! the upstream file it mirrors.
//!
//! Integration points for the future event loop are documented on
//! [`SlackChannel::start_account`].

use crate::config::{
    Config, ReplyToMode, SlackAccountConfig, SlackAllowBots, SlackAllowBotsMode,
    SlackChannelConfig, SlackRelayConfig, StatusReactionsEmojiConfig,
};
use crate::gateway::GatewayState;

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use regex::Regex;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, info, warn};

// ============================================================================
// v2026.2.26: NO_REPLY Suppression
// ============================================================================

/// Sentinel value indicating the agent chose not to reply.
///
/// When the agent returns this exact string as a response, the Slack channel
/// suppresses the API call — no message is sent and no error is raised.
/// This prevents empty or unwanted messages from being posted to Slack.
pub const NO_REPLY_SENTINEL: &str = "NO_REPLY";

/// Check if a message should be suppressed (not sent to Slack).
pub fn should_suppress_message(message: &str) -> bool {
    let trimmed = message.trim();
    trimmed == NO_REPLY_SENTINEL || trimmed.is_empty()
}

// ============================================================================
// v2026.7.1: SecretRef-Tolerant Token Resolution
// (upstream: `token.ts`, plugin-sdk `secret-input.ts`, `types.secrets.ts`)
// ============================================================================

/// Resolve a Slack secret input string, tolerating SecretRef shapes.
///
/// Accepted forms (upstream `coerceSecretRef` + `normalizeResolvedSecretInputString`):
/// - literal token strings (returned as-is, trimmed)
/// - `$NAME` / `${NAME}` env shorthand
/// - legacy `secretref-env:NAME` and `__env__:NAME` markers
///
/// Env-backed refs resolve from the process environment; unresolvable refs
/// return `None` (fail-soft) instead of leaking the marker as a token.
pub fn resolve_slack_secret_input(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let env_name = if let Some(rest) = trimmed.strip_prefix("${") {
        rest.strip_suffix('}').map(str::trim)
    } else if let Some(rest) = trimmed.strip_prefix("secretref-env:") {
        Some(rest.trim())
    } else if let Some(rest) = trimmed.strip_prefix("__env__:") {
        Some(rest.trim())
    } else if let Some(rest) = trimmed.strip_prefix('$') {
        // Only treat `$NAME` as an env ref for uppercase env-var-shaped names.
        if rest.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            && !rest.is_empty()
        {
            Some(rest)
        } else {
            return Some(trimmed.to_string());
        }
    } else {
        return Some(trimmed.to_string());
    };
    let name = env_name.filter(|n| !n.is_empty())?;
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// Resolve a Slack secret from a JSON config value, tolerating structured
/// SecretRefs (`{ "source": "env" | "file", "id": "..." }`) in addition to
/// plain strings (upstream `types.secrets.ts` `isSecretRef`).
pub fn resolve_slack_secret_value(raw: &Value) -> Option<String> {
    match raw {
        Value::String(s) => resolve_slack_secret_input(s),
        Value::Object(map) => {
            let source = map.get("source").and_then(Value::as_str)?;
            let id = map.get("id").and_then(Value::as_str)?.trim();
            if id.is_empty() {
                return None;
            }
            match source {
                "env" => std::env::var(id).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()),
                "file" => std::fs::read_to_string(id)
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty()),
                // exec refs are not resolvable without a sandbox runner; fail soft.
                _ => None,
            }
        }
        _ => None,
    }
}

/// Resolve the effective bot token for an account (config → `SLACK_BOT_TOKEN`).
pub fn resolve_slack_bot_token(account: &SlackAccountConfig) -> Option<String> {
    account
        .bot_token
        .as_deref()
        .and_then(resolve_slack_secret_input)
        .or_else(|| std::env::var("SLACK_BOT_TOKEN").ok().filter(|v| !v.trim().is_empty()))
}

/// Resolve the effective app-level token for an account (config → `SLACK_APP_TOKEN`).
pub fn resolve_slack_app_token(account: &SlackAccountConfig) -> Option<String> {
    account
        .app_token
        .as_deref()
        .and_then(resolve_slack_secret_input)
        .or_else(|| std::env::var("SLACK_APP_TOKEN").ok().filter(|v| !v.trim().is_empty()))
}

/// Resolve the effective signing secret for an account.
pub fn resolve_slack_signing_secret(account: &SlackAccountConfig) -> Option<String> {
    account.signing_secret.as_deref().and_then(resolve_slack_secret_input)
}

// ============================================================================
// v2026.7.1: Bot-Token-As-User Warning (upstream: `token.ts`)
// ============================================================================

/// Format the warning emitted when `auth.test` identifies the configured bot
/// token as a *user* token (user_id present without bot_id). Until replaced,
/// explicit bot-mention detection is disabled and required-mention channels
/// fail closed.
pub fn format_slack_bot_token_identity_warning(
    auth_user_id: Option<&str>,
    auth_bot_id: Option<&str>,
    account_id: Option<&str>,
) -> Option<String> {
    let user_id = auth_user_id.map(str::trim).filter(|s| !s.is_empty())?;
    if auth_bot_id.map(str::trim).filter(|s| !s.is_empty()).is_some() {
        // Slack documents bot_id only for bot-token auth.test responses.
        return None;
    }
    let account_id = account_id.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("default");
    let token_path = if account_id == "default" {
        "channels.slack.botToken, channels.slack.accounts.default.botToken, or SLACK_BOT_TOKEN"
            .to_string()
    } else {
        format!("channels.slack.accounts.{account_id}.botToken")
    };
    Some(format!(
        "Slack auth.test identified account \"{account_id}\" as user {user_id} without bot_id. \
         {token_path} appears to contain a user token; replace it with a Bot User OAuth Token. \
         Until replaced, explicit bot-mention detection is disabled and required-mention \
         channels fail closed."
    ))
}

// ============================================================================
// v2026.5.2/v2026.7.1: Target Parsing (upstream: `target-parsing.ts`, `targets.ts`)
// ============================================================================

/// Kind of a parsed Slack messaging target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackTargetKind {
    User,
    Channel,
}

/// A parsed Slack messaging target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackTarget {
    pub kind: SlackTargetKind,
    pub id: String,
    /// Canonical `kind:ID` form used for comparisons.
    pub normalized: String,
}

static SLACK_MENTION_TARGET_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^<@([A-Z0-9]+)>$").expect("valid regex"));
static SLACK_PLAIN_ID_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^[A-Z0-9]+$").expect("valid regex"));
static SLACK_TARGET_ID_SHAPE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^[CUWGD][A-Z0-9]{8,}$").expect("valid regex"));

fn build_slack_target(kind: SlackTargetKind, id: &str) -> SlackTarget {
    let kind_label = match kind {
        SlackTargetKind::User => "user",
        SlackTargetKind::Channel => "channel",
    };
    let id = id.trim().to_string();
    SlackTarget {
        normalized: format!("{kind_label}:{}", id.to_uppercase()),
        kind,
        id,
    }
}

/// Parse a Slack target string. Accepted forms (upstream `parseSlackTarget`):
/// - `<@U123>` mention → user target
/// - `user:U123`, `channel:C123`, `slack:U123` prefixes
/// - `@U123` (id required — `@name` errors)
/// - `#C123` (id required — `#name` errors)
/// - bare strings fall back to `default_kind` (default: channel)
pub fn parse_slack_target(
    raw: &str,
    default_kind: Option<SlackTargetKind>,
) -> Result<Option<SlackTarget>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if let Some(caps) = SLACK_MENTION_TARGET_RE.captures(trimmed) {
        return Ok(Some(build_slack_target(SlackTargetKind::User, &caps[1])));
    }
    for (prefix, kind) in [
        ("user:", SlackTargetKind::User),
        ("channel:", SlackTargetKind::Channel),
        ("slack:", SlackTargetKind::User),
    ] {
        if let Some(rest) = strip_prefix_ci(trimmed, prefix) {
            let candidate = rest.trim();
            // Tolerate `user:<@U123>` decoration.
            let candidate = SLACK_MENTION_TARGET_RE
                .captures(candidate)
                .map(|c| c.get(1).expect("group").as_str().to_string())
                .unwrap_or_else(|| candidate.to_string());
            if candidate.is_empty() {
                bail!("Slack target requires an id after `{prefix}`");
            }
            return Ok(Some(build_slack_target(kind, &candidate)));
        }
    }
    if let Some(rest) = trimmed.strip_prefix('@') {
        let candidate = rest.trim();
        if !SLACK_PLAIN_ID_RE.is_match(candidate) {
            bail!("Slack DMs require a user id (use user:<id> or <@id>)");
        }
        return Ok(Some(build_slack_target(SlackTargetKind::User, candidate)));
    }
    if let Some(rest) = trimmed.strip_prefix('#') {
        let candidate = rest.trim();
        if !SLACK_PLAIN_ID_RE.is_match(candidate) {
            bail!("Slack channels require a channel id (use channel:<id>)");
        }
        return Ok(Some(build_slack_target(SlackTargetKind::Channel, candidate)));
    }
    let kind = default_kind.unwrap_or(SlackTargetKind::Channel);
    Ok(Some(build_slack_target(kind, trimmed)))
}

fn strip_prefix_ci<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    if value.len() >= prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&value[prefix.len()..])
    } else {
        None
    }
}

/// Resolve a raw target string into a channel id, erroring on user targets
/// (upstream `resolveSlackChannelId`).
pub fn resolve_slack_channel_id(raw: &str) -> Result<String> {
    let target = parse_slack_target(raw, Some(SlackTargetKind::Channel))?
        .ok_or_else(|| anyhow!("Slack target must not be empty"))?;
    match target.kind {
        SlackTargetKind::Channel => Ok(target.id),
        SlackTargetKind::User => bail!("Slack target {raw} is a user, expected a channel"),
    }
}

/// Normalize a target string for comparison (upstream `normalizeSlackMessagingTarget`).
pub fn normalize_slack_messaging_target(raw: &str) -> Option<String> {
    parse_slack_target(raw, Some(SlackTargetKind::Channel))
        .ok()
        .flatten()
        .map(|t| t.normalized)
}

/// True when two target strings refer to the same conversation
/// (upstream `slackTargetsMatch`).
pub fn slack_targets_match(left: &str, right: &str) -> bool {
    match (normalize_slack_messaging_target(left), normalize_slack_messaging_target(right)) {
        (Some(l), Some(r)) => l == r,
        _ => false,
    }
}

/// Heuristic: does the string look like a Slack target id
/// (upstream `looksLikeSlackTargetId`).
pub fn looks_like_slack_target_id(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    if SLACK_MENTION_TARGET_RE.is_match(trimmed) {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("user:") || lower.starts_with("channel:") || lower.starts_with("slack:") {
        return true;
    }
    if trimmed.starts_with('@') || trimmed.starts_with('#') {
        return true;
    }
    SLACK_TARGET_ID_SHAPE_RE.is_match(trimmed)
}

/// Match a message-tool `target` against the current conversation context.
/// Core target resolution removes the `user:` prefix before auto-thread
/// selection, so bare resolved user ids also match (upstream `targets.ts`).
pub fn slack_context_targets_match(
    target: &str,
    current_channel_id: Option<&str>,
    current_messaging_target: Option<&str>,
) -> bool {
    if let Some(current) = current_messaging_target {
        if slack_targets_match(target, current) {
            return true;
        }
        static RESOLVED_USER_RE: Lazy<Regex> =
            Lazy::new(|| Regex::new(r"(?i)^[UW][A-Z0-9]+$").expect("valid regex"));
        if RESOLVED_USER_RE.is_match(target.trim()) {
            if let Ok(Some(parsed)) = parse_slack_target(current, None) {
                if parsed.kind == SlackTargetKind::User
                    && parsed.id.eq_ignore_ascii_case(target.trim())
                {
                    return true;
                }
            }
        }
    }
    if let Some(channel) = current_channel_id {
        if slack_targets_match(target, channel) {
            return true;
        }
    }
    false
}

// ============================================================================
// Thread ts Helpers (upstream: `thread-ts.ts`)
// ============================================================================

static SLACK_THREAD_TS_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\d+\.\d+$").expect("valid regex"));

/// Normalize a candidate thread timestamp; only `seconds.fraction` shapes pass.
pub fn normalize_slack_thread_ts_candidate(value: Option<&str>) -> Option<String> {
    let normalized = value?.trim();
    if !normalized.is_empty() && SLACK_THREAD_TS_RE.is_match(normalized) {
        Some(normalized.to_string())
    } else {
        None
    }
}

/// Resolve the effective thread ts from replyTo/thread ids
/// (upstream `resolveSlackThreadTsValue`).
pub fn resolve_slack_thread_ts_value(
    reply_to_id: Option<&str>,
    thread_id: Option<&str>,
) -> Option<String> {
    normalize_slack_thread_ts_candidate(reply_to_id)
        .or_else(|| normalize_slack_thread_ts_candidate(thread_id))
}

// ============================================================================
// v2026.5.2: Allowlist + Channel Config Resolution
// (upstream: `monitor/allow-list.ts`, `monitor/channel-config.ts`)
// ============================================================================

/// Normalize a user-facing name into a permissive lowercase hyphen slug
/// (upstream `normalizeHyphenSlug`; keeps `#@._+-`).
pub fn normalize_slack_slug(raw: &str) -> String {
    let trimmed = raw.trim().to_lowercase();
    let mut out = String::with_capacity(trimmed.len());
    let mut last_dash = false;
    for c in trimmed.chars() {
        let mapped = if c.is_whitespace() {
            '-'
        } else if c.is_alphanumeric() || matches!(c, '#' | '@' | '.' | '_' | '+' | '-') {
            c
        } else {
            '-'
        };
        if mapped == '-' {
            if last_dash {
                continue;
            }
            last_dash = true;
        } else {
            last_dash = false;
        }
        out.push(mapped);
    }
    out.trim_matches(|c| c == '-' || c == '.').to_string()
}

/// True when an allowlist entry matches a runtime channel, including bare
/// runtime channel IDs matched against decorated entries (v2026.5.2 routing
/// row): entries like `C0123`, `channel:C0123`, `#C0123`, or `#name` all
/// match runtime channel id `C0123` (id comparisons case-insensitive).
pub fn slack_allowlist_entry_matches_channel(
    entry: &str,
    channel_id: &str,
    channel_name: Option<&str>,
) -> bool {
    let entry = entry.trim();
    if entry.is_empty() || channel_id.trim().is_empty() {
        return false;
    }
    if entry == "*" {
        return true;
    }
    let bare_entry = strip_prefix_ci(entry, "channel:")
        .or_else(|| strip_prefix_ci(entry, "slack:"))
        .unwrap_or(entry);
    let bare_entry = bare_entry.strip_prefix('#').unwrap_or(bare_entry).trim();
    let channel_id = channel_id.trim();
    if bare_entry.eq_ignore_ascii_case(channel_id) {
        return true;
    }
    if let Some(name) = channel_name {
        let name = name.strip_prefix('#').unwrap_or(name);
        if !name.is_empty()
            && (bare_entry.eq_ignore_ascii_case(name)
                || normalize_slack_slug(bare_entry) == normalize_slack_slug(name))
        {
            return true;
        }
    }
    false
}

/// True when any allowlist entry matches the channel (empty list ⇒ no match).
pub fn slack_allowlist_matches_channel(
    entries: &[String],
    channel_id: &str,
    channel_name: Option<&str>,
) -> bool {
    entries
        .iter()
        .any(|entry| slack_allowlist_entry_matches_channel(entry, channel_id, channel_name))
}

/// Resolved per-channel policy (upstream `SlackChannelConfigResolved`).
#[derive(Debug, Clone, PartialEq)]
pub struct SlackResolvedChannelConfig {
    pub allowed: bool,
    pub require_mention: bool,
    pub ignore_other_mentions: Option<bool>,
    pub reply_to_mode: Option<ReplyToMode>,
    pub allow_bots: Option<SlackAllowBots>,
    pub users: Option<Vec<String>>,
    pub skills: Option<Vec<String>>,
    pub system_prompt: Option<String>,
    /// The config key that matched (for diagnostics).
    pub match_key: Option<String>,
    /// True when the match came from a name key instead of a channel ID.
    pub matched_by_name: bool,
}

/// Resolve the `channels` config entry for a runtime channel.
///
/// Candidate keys, in order (upstream `resolveSlackChannelConfig`): raw id,
/// lowercase id, uppercase id, `channel:` decorated variants, then — only when
/// `allow_name_matching` — `#name`, `name`, and the hyphen slug. Falls back to
/// the `"*"` wildcard entry. With a non-empty channel map and no match, the
/// channel is not allowed (public-channel allowlist semantics, v2026.5.2).
pub fn resolve_slack_channel_config(
    channels: Option<&HashMap<String, SlackChannelConfig>>,
    channel_id: &str,
    channel_name: Option<&str>,
    default_require_mention: bool,
    allow_name_matching: bool,
) -> SlackResolvedChannelConfig {
    let empty = HashMap::new();
    let entries = channels.unwrap_or(&empty);
    if entries.is_empty() {
        return SlackResolvedChannelConfig {
            allowed: true,
            require_mention: default_require_mention,
            ignore_other_mentions: None,
            reply_to_mode: None,
            allow_bots: None,
            users: None,
            skills: None,
            system_prompt: None,
            match_key: None,
            matched_by_name: false,
        };
    }

    let id_lower = channel_id.to_lowercase();
    let id_upper = channel_id.to_uppercase();
    let name = channel_name.map(|n| n.strip_prefix('#').unwrap_or(n).trim().to_string());
    let slug = name.as_deref().map(normalize_slack_slug);

    let mut candidates: Vec<(String, bool)> = vec![
        (channel_id.to_string(), false),
        (id_lower, false),
        (id_upper, false),
        (format!("channel:{channel_id}"), false),
        (format!("channel:{}", channel_id.to_lowercase()), false),
        (format!("channel:{}", channel_id.to_uppercase()), false),
    ];
    if allow_name_matching {
        if let Some(name) = &name {
            candidates.push((format!("#{name}"), true));
            candidates.push((name.clone(), true));
        }
        if let Some(slug) = &slug {
            candidates.push((slug.clone(), true));
        }
    }

    let mut matched: Option<(&SlackChannelConfig, String, bool)> = None;
    for (candidate, by_name) in &candidates {
        if candidate.is_empty() {
            continue;
        }
        if let Some(entry) = entries.get(candidate) {
            matched = Some((entry, candidate.clone(), *by_name));
            break;
        }
    }
    let wildcard = entries.get("*");

    let (entry, match_key, matched_by_name) = match (&matched, wildcard) {
        (Some((entry, key, by_name)), _) => (Some(*entry), Some(key.clone()), *by_name),
        (None, Some(entry)) => (Some(entry), Some("*".to_string()), false),
        (None, None) => (None, None, false),
    };

    let Some(entry) = entry else {
        return SlackResolvedChannelConfig {
            allowed: false,
            require_mention: default_require_mention,
            ignore_other_mentions: None,
            reply_to_mode: None,
            allow_bots: None,
            users: None,
            skills: None,
            system_prompt: None,
            match_key: None,
            matched_by_name: false,
        };
    };

    let fallback = wildcard;
    let first = |a: Option<bool>, b: Option<bool>| a.or(b);
    let allowed = first(
        entry.enabled.or(entry.allow),
        fallback.and_then(|f| f.enabled.or(f.allow)),
    )
    .unwrap_or(true);
    let require_mention = first(
        entry.require_mention,
        fallback.and_then(|f| f.require_mention),
    )
    .unwrap_or(default_require_mention);

    SlackResolvedChannelConfig {
        allowed,
        require_mention,
        ignore_other_mentions: entry
            .ignore_other_mentions
            .or_else(|| fallback.and_then(|f| f.ignore_other_mentions)),
        reply_to_mode: entry
            .reply_to_mode
            .or_else(|| fallback.and_then(|f| f.reply_to_mode)),
        allow_bots: entry.allow_bots.or_else(|| fallback.and_then(|f| f.allow_bots)),
        users: entry.users.clone().or_else(|| fallback.and_then(|f| f.users.clone())),
        skills: entry.skills.clone().or_else(|| fallback.and_then(|f| f.skills.clone())),
        system_prompt: entry
            .system_prompt
            .clone()
            .or_else(|| fallback.and_then(|f| f.system_prompt.clone())),
        match_key,
        matched_by_name,
    }
}

/// Warn about `channels` config keys that look like channel *names* when name
/// matching is disabled — those entries silently never match runtime channel
/// IDs (v2026.7.1 name-vs-ID channel map warnings).
pub fn slack_channel_map_name_key_warnings(
    channel_keys: &[String],
    allow_name_matching: bool,
) -> Vec<String> {
    if allow_name_matching {
        return Vec::new();
    }
    channel_keys
        .iter()
        .filter(|key| {
            let key = key.trim();
            if key.is_empty() || key == "*" {
                return false;
            }
            let bare = strip_prefix_ci(key, "channel:").unwrap_or(key);
            let bare = bare.strip_prefix('#').unwrap_or(bare);
            !SLACK_TARGET_ID_SHAPE_RE.is_match(bare)
        })
        .map(|key| {
            format!(
                "slack channels config key \"{key}\" looks like a channel name; Slack delivers \
                 channel IDs (e.g. C0123456789). Use the channel ID or set \
                 channels.slack.allowNameMatching: true."
            )
        })
        .collect()
}

// ============================================================================
// v2026.7.1: Mention Detection (fail-closed)
// (upstream: `monitor/message-handler/prepare.ts`)
// ============================================================================

static SLACK_USER_MENTION_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<@([A-Z0-9]+)(?:\|[^>]*)?>").expect("valid regex"));
static SLACK_SUBTEAM_MENTION_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<!subteam\^([A-Z0-9]+)(?:\|[^>]*)?>").expect("valid regex"));
static SLACK_ANY_MENTION_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<[@!]").expect("valid regex"));

/// Mention metadata extracted from an inbound message text.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SlackMentionMetadata {
    pub mentioned_user_ids: Vec<String>,
    pub mentioned_subteam_ids: Vec<String>,
    pub has_any_mention: bool,
    pub has_subteam_mention: bool,
}

/// Collect unique mention ids from inbound text (upstream
/// `collectSlackMentionMetadata`).
pub fn collect_slack_mention_metadata(text: &str) -> SlackMentionMetadata {
    let mut user_ids = Vec::new();
    for caps in SLACK_USER_MENTION_RE.captures_iter(text) {
        let id = caps[1].to_string();
        if !user_ids.contains(&id) {
            user_ids.push(id);
        }
    }
    let mut subteam_ids = Vec::new();
    for caps in SLACK_SUBTEAM_MENTION_RE.captures_iter(text) {
        let id = caps[1].to_string();
        if !subteam_ids.contains(&id) {
            subteam_ids.push(id);
        }
    }
    SlackMentionMetadata {
        has_any_mention: SLACK_ANY_MENTION_RE.is_match(text),
        has_subteam_mention: text.contains("<!subteam^"),
        mentioned_user_ids: user_ids,
        mentioned_subteam_ids: subteam_ids,
    }
}

/// Fail-closed explicit mention detection: an unknown bot user id means the
/// bot is treated as NOT mentioned (required-mention channels then drop the
/// message rather than replying to everything).
pub fn slack_explicitly_mentioned_bot(
    bot_user_id: Option<&str>,
    metadata: &SlackMentionMetadata,
) -> bool {
    let Some(bot_user_id) = bot_user_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return false; // fail closed
    };
    metadata
        .mentioned_user_ids
        .iter()
        .any(|id| id.eq_ignore_ascii_case(bot_user_id))
}

/// Whether mention detection is possible at all (bot id known or explicit
/// mention patterns configured). When false and a mention is required, the
/// message is dropped (upstream: "mention-detection-unavailable").
pub fn slack_can_detect_mention(bot_user_id: Option<&str>, mention_pattern_count: usize) -> bool {
    bot_user_id.map(str::trim).filter(|s| !s.is_empty()).is_some() || mention_pattern_count > 0
}

/// Resolved allow-bots mode (upstream `allowBotsMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackAllowBotsResolved {
    Off,
    All,
    Mentions,
}

/// Resolve the allow-bots mode with channel-over-account precedence
/// (default: off).
pub fn resolve_slack_allow_bots_mode(
    channel_setting: Option<SlackAllowBots>,
    account_setting: Option<SlackAllowBots>,
) -> SlackAllowBotsResolved {
    match channel_setting.or(account_setting) {
        Some(SlackAllowBots::Mode(SlackAllowBotsMode::Mentions)) => SlackAllowBotsResolved::Mentions,
        Some(SlackAllowBots::Flag(true)) => SlackAllowBotsResolved::All,
        Some(SlackAllowBots::Flag(false)) | None => SlackAllowBotsResolved::Off,
    }
}

/// Gate a bot-authored inbound message. `allowBots:"mentions"` admits bot
/// messages only in DMs or when this bot was effectively mentioned.
pub fn slack_bot_message_allowed(
    mode: SlackAllowBotsResolved,
    is_direct_message: bool,
    effective_was_mentioned: bool,
) -> bool {
    match mode {
        SlackAllowBotsResolved::Off => false,
        SlackAllowBotsResolved::All => true,
        SlackAllowBotsResolved::Mentions => is_direct_message || effective_was_mentioned,
    }
}

/// `ignoreOtherMentions`: drop unmentioned channel messages that mention
/// someone else. Only an explicit bot mention escapes this gate, and native
/// bot identity is required to distinguish bot pings from other mentions.
pub fn slack_should_drop_other_mention(
    ignore_other_mentions: bool,
    is_room: bool,
    bot_user_id: Option<&str>,
    metadata: &SlackMentionMetadata,
    was_mentioned: bool,
) -> bool {
    is_room
        && ignore_other_mentions
        && bot_user_id.map(str::trim).filter(|s| !s.is_empty()).is_some()
        && metadata.has_any_mention
        && !was_mentioned
}

// ============================================================================
// v2026.5.2/v2026.7.1: DM Canonicalization + Session Routing
// (upstream: `monitor/message-handler/prepare-routing.ts`, `monitor/context.ts`)
// ============================================================================

/// Chat kind for session-key purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackChatKind {
    Direct,
    Group,
    Channel,
}

/// Canonical base conversation id: DMs canonicalize to the *peer user*
/// (`user:U…`), not the ephemeral `D…` IM channel id, so top-level DMs get
/// stable session keys across DM channel churn (upstream
/// `resolveSlackBaseConversationId`).
pub fn resolve_slack_base_conversation_id(
    is_direct_message: bool,
    user_id: Option<&str>,
    channel_id: &str,
) -> String {
    if is_direct_message {
        format!("user:{}", user_id.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("unknown"))
    } else {
        channel_id.to_string()
    }
}

/// Build the session key for an inbound conversation.
///
/// - top-level DM: `slack:{account}:user:{U}` — stable across restarts and
///   independent of the `D…` channel id or message ts (v2026.5.2 DM routing).
/// - group DM: `slack:{account}:group:{C}`
/// - channel: `slack:{account}:channel:{C}`, thread replies get a
///   thread-scoped suffix `:thread:{ts}` (v2026.7.1 assistant threads route
///   thread-scoped sessions the same way).
pub fn resolve_slack_session_key(
    account_id: &str,
    chat_kind: SlackChatKind,
    peer_id: &str,
    thread_ts: Option<&str>,
) -> String {
    let account = if account_id.trim().is_empty() { "default" } else { account_id.trim() };
    let base = match chat_kind {
        SlackChatKind::Direct => format!("slack:{account}:user:{}", peer_id.trim()),
        SlackChatKind::Group => format!("slack:{account}:group:{}", peer_id.trim()),
        SlackChatKind::Channel => format!("slack:{account}:channel:{}", peer_id.trim()),
    };
    match (chat_kind, normalize_slack_thread_ts_candidate(thread_ts)) {
        // Top-level DMs (and DM thread replies) canonicalize to the peer
        // session: history stays in one place per human.
        (SlackChatKind::Direct, _) => base,
        (_, Some(ts)) => format!("{base}:thread:{ts}"),
        (_, None) => base,
    }
}

/// Effective DM history limit: `dmHistoryLimit` wins over `historyLimit`;
/// 0 disables backfill (v2026.5.2 honors `dmHistoryLimit`).
pub fn resolve_slack_dm_history_limit(account: &SlackAccountConfig) -> u32 {
    account.dm_history_limit.or(account.history_limit).unwrap_or(0)
}

/// Whether an inbound top-level DM should backfill history: only when a
/// positive limit is configured and there is no prior context
/// (upstream prepare.ts `shouldSeedDmHistory`).
pub fn slack_should_seed_dm_history(
    is_direct_message: bool,
    is_thread_reply: bool,
    dm_history_limit: u32,
    has_previous_timestamp: bool,
) -> bool {
    is_direct_message && !is_thread_reply && dm_history_limit > 0 && !has_previous_timestamp
}

// ============================================================================
// v2026.5.2: App Home Tab (upstream: `monitor/events/home.ts`, `setup-shared.ts`)
// ============================================================================

/// Build the App Home tab view payload published on `app_home_opened`
/// (upstream `buildSlackHomeView`). Pure JSON builder — the events-API hook
/// calls `views.publish` with `{ user_id, view: build_slack_home_view() }`.
pub fn build_slack_home_view() -> Value {
    build_slack_home_view_named("MyLobster")
}

/// Named variant for custom bot branding.
pub fn build_slack_home_view_named(bot_name: &str) -> Value {
    let name = if bot_name.trim().is_empty() { "MyLobster" } else { bot_name.trim() };
    json!({
        "type": "home",
        "callback_id": "mylobster:home",
        "blocks": [
            {
                "type": "header",
                "text": { "type": "plain_text", "text": name }
            },
            {
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": format!(
                        "Send a DM, mention {name} in a channel, or use `/mylobster` to start a session."
                    )
                }
            },
            {
                "type": "context",
                "elements": [{
                    "type": "mrkdwn",
                    "text": "This Home tab is safe to show to any workspace member who opens the app."
                }]
            }
        ]
    })
}

/// Decide whether an `app_home_opened` event should publish the Home view.
/// The messages tab never publishes (upstream home.ts skips `tab == "messages"`).
pub fn should_publish_slack_home_view(user: Option<&str>, tab: Option<&str>) -> bool {
    user.map(str::trim).filter(|u| !u.is_empty()).is_some() && tab != Some("messages")
}

/// Bot events required in the app manifest, including the Home tab and
/// assistant thread lifecycle events (upstream `buildSlackManifest`).
pub fn slack_manifest_bot_events() -> Vec<&'static str> {
    vec![
        "app_home_opened",
        "app_mention",
        "assistant_thread_context_changed",
        "assistant_thread_started",
        "channel_rename",
        "member_joined_channel",
        "member_left_channel",
        "message.channels",
        "message.groups",
        "message.im",
        "message.mpim",
        "pin_added",
        "pin_removed",
        "reaction_added",
        "reaction_removed",
    ]
}

/// Build the Slack app setup manifest with Home tab, assistant view with
/// suggested prompts, and Socket Mode events (upstream `buildSlackManifest`).
pub fn build_slack_manifest(bot_name: &str) -> Value {
    let safe_name = if bot_name.trim().is_empty() { "MyLobster" } else { bot_name.trim() };
    json!({
        "display_information": {
            "name": safe_name,
            "description": format!("{safe_name} connector for MyLobster"),
        },
        "features": {
            "bot_user": { "display_name": safe_name, "always_online": true },
            "app_home": {
                "home_tab_enabled": true,
                "messages_tab_enabled": true,
                "messages_tab_read_only_enabled": false
            },
            "assistant_view": {
                "assistant_description":
                    format!("{safe_name} connects Slack assistant threads to MyLobster agents."),
                "suggested_prompts": default_slack_assistant_prompts_json()
            },
            "slash_commands": [{
                "command": "/mylobster",
                "description": format!("Send a message to {safe_name}"),
                "should_escape": false
            }]
        },
        "oauth_config": {
            "scopes": {
                "bot": [
                    "app_mentions:read", "assistant:write", "channels:history",
                    "channels:read", "chat:write", "commands", "emoji:read",
                    "files:read", "files:write", "groups:history", "groups:read",
                    "im:history", "im:read", "im:write", "mpim:history",
                    "mpim:read", "mpim:write", "pins:read", "pins:write",
                    "reactions:read", "reactions:write", "usergroups:read",
                    "users:read"
                ]
            }
        },
        "settings": {
            "socket_mode_enabled": true,
            "event_subscriptions": { "bot_events": slack_manifest_bot_events() }
        }
    })
}

// ============================================================================
// v2026.7.1: Native Assistant Threads
// (upstream: `monitor/events/assistant.ts`, `monitor/context.ts`,
//  `monitor/message-handler/dispatch.ts`)
// ============================================================================

/// A suggested prompt for assistant threads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackAssistantSuggestedPrompt {
    pub title: &'static str,
    pub message: &'static str,
}

/// Default suggested prompts pushed on `assistant_thread_started`.
pub const DEFAULT_SLACK_ASSISTANT_PROMPTS: [SlackAssistantSuggestedPrompt; 3] = [
    SlackAssistantSuggestedPrompt {
        title: "What can you do?",
        message: "What can you help me with?",
    },
    SlackAssistantSuggestedPrompt {
        title: "Summarize this channel",
        message: "Summarize the recent activity in this channel.",
    },
    SlackAssistantSuggestedPrompt {
        title: "Draft a reply",
        message: "Help me draft a reply.",
    },
];

fn default_slack_assistant_prompts_json() -> Value {
    Value::Array(
        DEFAULT_SLACK_ASSISTANT_PROMPTS
            .iter()
            .map(|p| json!({ "title": p.title, "message": p.message }))
            .collect(),
    )
}

/// Tracked assistant-thread context (upstream `SlackAssistantThreadContext`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SlackAssistantThreadContext {
    pub assistant_channel_id: String,
    pub thread_ts: String,
    pub user_id: Option<String>,
    pub channel_id: Option<String>,
    pub team_id: Option<String>,
    pub enterprise_id: Option<String>,
}

fn assistant_ctx_str(event: &Value, path: &[&str]) -> Option<String> {
    let mut cur = event;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Normalize an `assistant_thread_started` / `assistant_thread_context_changed`
/// event payload into a thread context, merging over the previously cached
/// context (upstream `normalizeAssistantThread`).
pub fn normalize_slack_assistant_thread(
    event: &Value,
    previous: Option<&SlackAssistantThreadContext>,
) -> Option<SlackAssistantThreadContext> {
    let thread = event.get("assistant_thread")?;
    let channel_id = assistant_ctx_str(thread, &["channel_id"])?;
    let thread_ts = assistant_ctx_str(thread, &["thread_ts"])?;
    let resolve = |key: &str| {
        assistant_ctx_str(thread, &["context", key])
            .or_else(|| assistant_ctx_str(event, &["context", key]))
    };
    Some(SlackAssistantThreadContext {
        user_id: assistant_ctx_str(thread, &["user_id"])
            .or_else(|| previous.and_then(|p| p.user_id.clone())),
        channel_id: resolve("channel_id").or_else(|| previous.and_then(|p| p.channel_id.clone())),
        team_id: resolve("team_id").or_else(|| previous.and_then(|p| p.team_id.clone())),
        enterprise_id: resolve("enterprise_id")
            .or_else(|| previous.and_then(|p| p.enterprise_id.clone())),
        assistant_channel_id: channel_id,
        thread_ts,
    })
}

/// Build the `assistant.threads.setSuggestedPrompts` payload
/// (upstream `setSlackAssistantSuggestedPrompts`).
pub fn build_slack_suggested_prompts_payload(
    channel_id: &str,
    thread_ts: &str,
    title: &str,
    prompts: &[SlackAssistantSuggestedPrompt],
) -> Value {
    json!({
        "channel_id": channel_id,
        "thread_ts": thread_ts,
        "title": title,
        "prompts": prompts
            .iter()
            .map(|p| json!({ "title": p.title, "message": p.message }))
            .collect::<Vec<_>>(),
    })
}

/// Rotating loading messages shown as native thread status while the agent
/// works (upstream `SLACK_THREAD_LOADING_MESSAGES`).
pub const SLACK_THREAD_LOADING_MESSAGES: [&str; 4] = [
    "Reading the thread...",
    "Checking context...",
    "Working through the request...",
    "Putting it all together...",
];

/// Rotating loading message selector: cycles through the list.
pub fn slack_thread_loading_message(step: usize) -> &'static str {
    SLACK_THREAD_LOADING_MESSAGES[step % SLACK_THREAD_LOADING_MESSAGES.len()]
}

/// Build the `assistant.threads.setStatus` payload with rotating loading
/// messages (max 10, upstream `monitor/context.ts`).
pub fn build_slack_assistant_status_payload(
    channel_id: &str,
    thread_ts: &str,
    status: &str,
    loading_messages: &[&str],
) -> Value {
    let mut payload = json!({
        "channel_id": channel_id,
        "thread_ts": thread_ts,
        "status": status,
    });
    if !loading_messages.is_empty() {
        payload["loading_messages"] = Value::Array(
            loading_messages
                .iter()
                .take(10)
                .map(|m| Value::String((*m).to_string()))
                .collect(),
        );
    }
    payload
}

/// Native thread status + rotating loading messages are gated by
/// `messages.statusReactions.enabled` (v2026.7.1).
pub fn slack_native_thread_status_enabled(config: &Config) -> bool {
    config
        .messages
        .status_reactions
        .as_ref()
        .and_then(|s| s.enabled)
        .unwrap_or(false)
}

// ============================================================================
// v2026.5.2: Persistent Thread Participation Store
// (upstream: `sent-thread-cache.ts` — persisted keyed store, 24h TTL)
// ============================================================================

/// TTL for thread-participation records (24 hours, upstream `TTL_MS`).
pub const SLACK_THREAD_PARTICIPATION_TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// Maximum persisted entries (upstream `PERSISTENT_MAX_ENTRIES`).
pub const SLACK_THREAD_PARTICIPATION_MAX_ENTRIES: usize = 1000;

/// Tracks Slack threads the bot has participated in, so thread replies
/// auto-respond without requiring a fresh @mention — and survives restarts
/// (v2026.5.2 "track bot-participated threads across restarts").
///
/// Backed by SQLite; use [`SlackThreadParticipationStore::open`] with a path
/// under `config.state_dir` (e.g. `state_dir/slack-thread-participation.sqlite3`).
pub struct SlackThreadParticipationStore {
    conn: parking_lot::Mutex<Connection>,
}

impl SlackThreadParticipationStore {
    /// Open (or create) the persistent store at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Self::init(Connection::open(path)?)
    }

    /// In-memory store (tests / fallback when state dir is unavailable).
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS slack_thread_participation (
                key TEXT PRIMARY KEY,
                agent_id TEXT,
                replied_at INTEGER NOT NULL
            );",
        )?;
        Ok(Self { conn: parking_lot::Mutex::new(conn) })
    }

    fn make_key(account_id: &str, channel_id: &str, thread_ts: &str) -> String {
        format!("{account_id}:{channel_id}:{thread_ts}")
    }

    fn now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    /// Record that the bot replied in a thread (upstream
    /// `recordSlackThreadParticipation`). Empty components are ignored.
    pub fn record(
        &self,
        account_id: &str,
        channel_id: &str,
        thread_ts: &str,
        agent_id: Option<&str>,
    ) -> Result<()> {
        if account_id.is_empty() || channel_id.is_empty() || thread_ts.is_empty() {
            return Ok(());
        }
        let key = Self::make_key(account_id, channel_id, thread_ts);
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO slack_thread_participation (key, agent_id, replied_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET agent_id = ?2, replied_at = ?3",
            rusqlite::params![key, agent_id, Self::now_ms()],
        )?;
        // Prune expired rows, then cap total entries (oldest first).
        conn.execute(
            "DELETE FROM slack_thread_participation WHERE replied_at < ?1",
            rusqlite::params![Self::now_ms() - SLACK_THREAD_PARTICIPATION_TTL_MS],
        )?;
        conn.execute(
            "DELETE FROM slack_thread_participation WHERE key NOT IN (
                SELECT key FROM slack_thread_participation
                ORDER BY replied_at DESC LIMIT ?1
            )",
            rusqlite::params![SLACK_THREAD_PARTICIPATION_MAX_ENTRIES as i64],
        )?;
        Ok(())
    }

    /// True when the bot participated in the thread within the TTL
    /// (upstream `hasSlackThreadParticipationWithPersistence`).
    pub fn has(&self, account_id: &str, channel_id: &str, thread_ts: &str) -> bool {
        if account_id.is_empty() || channel_id.is_empty() || thread_ts.is_empty() {
            return false;
        }
        let key = Self::make_key(account_id, channel_id, thread_ts);
        let cutoff = Self::now_ms() - SLACK_THREAD_PARTICIPATION_TTL_MS;
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT 1 FROM slack_thread_participation WHERE key = ?1 AND replied_at >= ?2",
            rusqlite::params![key, cutoff],
            |_| Ok(()),
        )
        .is_ok()
    }

    /// Number of live (unexpired) records.
    pub fn len(&self) -> usize {
        let cutoff = Self::now_ms() - SLACK_THREAD_PARTICIPATION_TTL_MS;
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM slack_thread_participation WHERE replied_at >= ?1",
            rusqlite::params![cutoff],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n as usize)
        .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Implicit mention kinds for thread replies (upstream `implicitMentionKindWhen`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackImplicitMentionKind {
    /// Replying to a message authored by the bot.
    ReplyToBot,
    /// The bot previously participated in this thread.
    BotThreadParticipant,
}

/// Decide implicit-mention kinds for an unmentioned thread reply. Only
/// consulted when the bot user id is known and explicit thread mentions are
/// not required (upstream prepare.ts).
pub fn resolve_slack_implicit_mention_kinds(
    is_direct_message: bool,
    bot_user_id: Option<&str>,
    thread_ts: Option<&str>,
    thread_require_explicit_mention: bool,
    was_mentioned: bool,
    parent_user_id: Option<&str>,
    store_has_participation: bool,
) -> Vec<SlackImplicitMentionKind> {
    if is_direct_message
        || bot_user_id.map(str::trim).filter(|s| !s.is_empty()).is_none()
        || thread_ts.is_none()
        || thread_require_explicit_mention
        || was_mentioned
    {
        return Vec::new();
    }
    if parent_user_id.is_some() && parent_user_id == bot_user_id {
        return vec![SlackImplicitMentionKind::ReplyToBot];
    }
    if store_has_participation {
        return vec![SlackImplicitMentionKind::BotThreadParticipant];
    }
    Vec::new()
}

// ============================================================================
// v2026.5.2/v2026.7.1: Status Reactions
// (upstream: `monitor/message-handler/dispatch.ts` status controller)
// ============================================================================

/// Lifecycle states for status reactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackStatusState {
    Queued,
    Thinking,
    Tool,
    Done,
    Error,
}

/// Resolve the emoji for a status state, honoring config overrides.
pub fn resolve_slack_status_emoji(
    state: SlackStatusState,
    emojis: Option<&StatusReactionsEmojiConfig>,
) -> String {
    let overridden = emojis.and_then(|e| match state {
        SlackStatusState::Queued => e.queued.clone(),
        SlackStatusState::Thinking => e.thinking.clone(),
        SlackStatusState::Tool => e.tool.clone(),
        SlackStatusState::Done => e.done.clone(),
        SlackStatusState::Error => e.error.clone(),
    });
    overridden.unwrap_or_else(|| {
        match state {
            SlackStatusState::Queued => "hourglass_flowing_sand",
            SlackStatusState::Thinking => "thought_balloon",
            SlackStatusState::Tool => "hammer_and_wrench",
            SlackStatusState::Done => "white_check_mark",
            SlackStatusState::Error => "x",
        }
        .to_string()
    })
}

/// A reaction operation the caller should apply via the Slack API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackReactionOp {
    Add(String),
    Remove(String),
}

/// Pure state machine that turns lifecycle transitions into reaction ops
/// (remove previous, add next). The live event loop applies the ops via
/// `reactions.add` / `reactions.remove`.
#[derive(Debug)]
pub struct SlackStatusReactionPlan {
    enabled: bool,
    emojis: Option<StatusReactionsEmojiConfig>,
    current: Option<String>,
}

impl SlackStatusReactionPlan {
    pub fn new(enabled: bool, emojis: Option<StatusReactionsEmojiConfig>) -> Self {
        Self { enabled, emojis, current: None }
    }

    /// Transition to a state, producing the reaction ops to apply.
    pub fn transition(&mut self, state: SlackStatusState) -> Vec<SlackReactionOp> {
        if !self.enabled {
            return Vec::new();
        }
        let next = resolve_slack_status_emoji(state, self.emojis.as_ref());
        let mut ops = Vec::new();
        if self.current.as_deref() == Some(next.as_str()) {
            return ops;
        }
        if let Some(prev) = self.current.take() {
            ops.push(SlackReactionOp::Remove(prev));
        }
        ops.push(SlackReactionOp::Add(next.clone()));
        self.current = Some(next);
        ops
    }

    /// Clear any active reaction (terminal cleanup after done/error hold).
    pub fn clear(&mut self) -> Vec<SlackReactionOp> {
        match self.current.take() {
            Some(prev) if self.enabled => vec![SlackReactionOp::Remove(prev)],
            _ => Vec::new(),
        }
    }
}

/// v2026.5.2: keep typing/temporary status reactions alive for
/// message-tool-only group/channel turns. When the reply delivery mode is
/// `message_tool_only`, the final channel reply is suppressed, so the status
/// reactions (and tool lifecycle updates) remain the only user-visible
/// progress and MUST be kept until the turn completes (upstream dispatch.ts:
/// `sourceReplyDeliveryMode === "message_tool_only" && statusReactionsEnabled`).
pub fn slack_keep_status_for_message_tool_turn(
    reply_delivery_mode: &str,
    status_reactions_enabled: bool,
) -> bool {
    reply_delivery_mode == "message_tool_only" && status_reactions_enabled
}

// ============================================================================
// v2026.5.2: Rich-Text Block Walker (upstream: `monitor/block-text.ts`)
// ============================================================================

/// Text recovered from Block Kit blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackBlocksText {
    pub text: String,
    pub has_rich_text: bool,
}

fn read_text_object(value: Option<&Value>) -> Option<String> {
    value?
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn render_slack_rich_text_leaf(element: &Value) -> String {
    let str_of = |key: &str| element.get(key).and_then(Value::as_str);
    match element.get("type").and_then(Value::as_str) {
        Some("text") => str_of("text").unwrap_or("").to_string(),
        Some("link") => str_of("text").or_else(|| str_of("url")).unwrap_or("").to_string(),
        Some("user") => str_of("user_id").map(|id| format!("<@{id}>")).unwrap_or_default(),
        Some("channel") => str_of("channel_id").map(|id| format!("<#{id}>")).unwrap_or_default(),
        Some("usergroup") => str_of("usergroup_id")
            .map(|id| format!("<!subteam^{id}>"))
            .unwrap_or_default(),
        Some("broadcast") => str_of("range").map(|r| format!("<!{r}>")).unwrap_or_default(),
        Some("emoji") => str_of("name").map(|n| format!(":{n}:")).unwrap_or_default(),
        _ => String::new(),
    }
}

fn render_slack_rich_text_elements(elements: Option<&Value>) -> String {
    let Some(Value::Array(elements)) = elements else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for element in elements {
        if !element.is_object() {
            continue;
        }
        match element.get("type").and_then(Value::as_str) {
            Some("rich_text_section") | Some("rich_text_preformatted") | Some("rich_text_quote") => {
                parts.push(render_slack_rich_text_elements(element.get("elements")));
            }
            Some("rich_text_list") => {
                let mut list_parts: Vec<String> = Vec::new();
                if let Some(Value::Array(children)) = element.get("elements") {
                    for child in children {
                        let rendered = render_slack_rich_text_elements(child.get("elements"));
                        if !rendered.is_empty() {
                            list_parts.push(rendered);
                        }
                    }
                }
                parts.push(list_parts.join("\n"));
            }
            _ => parts.push(render_slack_rich_text_leaf(element)),
        }
    }
    parts.concat()
}

fn read_slack_block_text(block: &Value) -> Option<String> {
    match block.get("type").and_then(Value::as_str)? {
        "rich_text" => {
            let rendered = render_slack_rich_text_elements(block.get("elements"));
            let trimmed = rendered.trim();
            if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
        }
        "section" => {
            if let Some(text) = read_text_object(block.get("text")) {
                return Some(text);
            }
            let fields = block.get("fields")?.as_array()?;
            let parts: Vec<String> =
                fields.iter().filter_map(|f| read_text_object(Some(f))).collect();
            if parts.is_empty() { None } else { Some(parts.join("\n")) }
        }
        "header" => read_text_object(block.get("text")),
        "context" => {
            let elements = block.get("elements")?.as_array()?;
            let parts: Vec<String> =
                elements.iter().filter_map(|e| read_text_object(Some(e))).collect();
            if parts.is_empty() { None } else { Some(parts.join(" ")) }
        }
        "image" => block
            .get("alt_text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| read_text_object(block.get("title"))),
        "video" => read_text_object(block.get("title")).or_else(|| {
            block
                .get("alt_text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        }),
        _ => None,
    }
}

/// Recover text from top-level Block Kit blocks, flagging rich_text presence
/// (upstream `resolveSlackBlocksText`).
pub fn resolve_slack_blocks_text(blocks: &[Value]) -> Option<SlackBlocksText> {
    if blocks.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    let mut has_rich_text = false;
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("rich_text") {
            has_rich_text = true;
        }
        if let Some(text) = read_slack_block_text(block) {
            parts.push(text);
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(SlackBlocksText { text: parts.join("\n"), has_rich_text })
    }
}

/// Choose the primary inbound text between `message.text` and block-derived
/// text: rich-text blocks win when longer (recovers full DM text that Slack
/// truncates in `message.text`) (upstream `chooseSlackPrimaryText`).
pub fn choose_slack_primary_text(
    message_text: Option<&str>,
    blocks_text: Option<&SlackBlocksText>,
) -> Option<String> {
    let Some(blocks_text) = blocks_text else {
        return message_text.map(str::to_string);
    };
    let Some(message_text) = message_text.filter(|t| !t.is_empty()) else {
        return Some(blocks_text.text.clone());
    };
    if blocks_text.has_rich_text && blocks_text.text.len() > message_text.len() {
        return Some(blocks_text.text.clone());
    }
    if blocks_text.text.len() > message_text.len() && blocks_text.text.starts_with(message_text) {
        Some(blocks_text.text.clone())
    } else {
        Some(message_text.to_string())
    }
}

// ============================================================================
// v2026.7.1: Reasoning-Payload Suppression
// (upstream: `monitor/message-handler/dispatch.ts`)
// ============================================================================

static SLACK_REASONING_TAG_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)<\s*(/?)\s*(?:(?:antml:|mm:)?(?:think(?:ing)?|thought)|antthinking)\b[^<>]*>")
        .expect("valid regex")
});
static SLACK_REASONING_SPAN_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?is)<\s*(?:(?:antml:|mm:)?(?:think(?:ing)?|thought)|antthinking)\b[^<>]*>.*?<\s*/\s*(?:(?:antml:|mm:)?(?:think(?:ing)?|thought)|antthinking)\b[^<>]*>",
    )
    .expect("valid regex")
});
static SLACK_REASONING_LABEL_PREFIX_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^\s*(?:>\s*)?Reasoning:\s*").expect("valid regex"));

/// Strip reasoning tags AND their enclosed content from outbound text so
/// model reasoning payloads never reach the channel (v2026.7.1 suppression).
pub fn strip_slack_reasoning_from_outbound(text: &str) -> String {
    let without_spans = SLACK_REASONING_SPAN_RE.replace_all(text, "");
    let without_tags = SLACK_REASONING_TAG_RE.replace_all(&without_spans, "");
    without_tags.trim().to_string()
}

/// Extract reasoning text enclosed in reasoning tags, if any.
pub fn extract_slack_reasoning_tag_text(text: &str) -> Option<String> {
    let m = SLACK_REASONING_SPAN_RE.find(text)?;
    let inner = SLACK_REASONING_TAG_RE.replace_all(m.as_str(), "");
    let inner = inner.trim();
    if inner.is_empty() { None } else { Some(inner.to_string()) }
}

fn strip_thinking_label_prefix(text: &str) -> &str {
    // Upstream: /^\s*(?:>\s*)?Thinking\.{0,3}(?=\s*(?:\n|_))/i — emulate the
    // lookahead manually since the regex crate has none.
    let trimmed = text.trim_start();
    let after_quote = trimmed.strip_prefix('>').map(str::trim_start).unwrap_or(trimmed);
    let lower = after_quote.to_lowercase();
    if !lower.starts_with("thinking") {
        return text;
    }
    let mut rest = &after_quote["thinking".len()..];
    let mut dots = 0;
    while dots < 3 && rest.starts_with('.') {
        rest = &rest[1..];
        dots += 1;
    }
    let peek = rest.trim_start_matches([' ', '\t']);
    if peek.starts_with('\n') || peek.starts_with('_') || peek.starts_with("\r\n") {
        rest
    } else {
        text
    }
}

/// Normalize a reasoning progress line for the status surface
/// (upstream `normalizeSlackReasoningProgressLine`).
pub fn normalize_slack_reasoning_progress_line(text: &str) -> String {
    let base = extract_slack_reasoning_tag_text(text)
        .unwrap_or_else(|| strip_slack_reasoning_from_outbound(text));
    let base = SLACK_REASONING_LABEL_PREFIX_RE.replace(&base, "");
    let base = strip_thinking_label_prefix(&base);
    base.lines()
        .map(|line| line.trim().trim_matches('_'))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Merge incremental reasoning progress text: snapshots and prefix-extensions
/// replace, otherwise deltas append (upstream `mergeSlackReasoningProgressText`).
pub fn merge_slack_reasoning_progress_text(current: &str, incoming: &str, snapshot: bool) -> String {
    if current.is_empty() {
        return incoming.to_string();
    }
    let normalized_current = normalize_slack_reasoning_progress_line(current);
    let normalized_incoming = normalize_slack_reasoning_progress_line(incoming);
    if normalized_incoming.is_empty() || normalized_incoming == normalized_current {
        return current.to_string();
    }
    if snapshot || normalized_incoming.starts_with(&normalized_current) {
        return incoming.to_string();
    }
    format!("{current}{incoming}")
}

// ============================================================================
// v2026.7.1: Router Relay Mode (upstream: `monitor/relay-source.ts`)
// ============================================================================

/// Maximum relay frame payload (upstream `SLACK_RELAY_MAX_PAYLOAD_BYTES`).
pub const SLACK_RELAY_MAX_PAYLOAD_BYTES: usize = 1024 * 1024;

/// Send identity forwarded by the relay router.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SlackSendIdentity {
    pub username: Option<String>,
    pub icon_url: Option<String>,
    pub icon_emoji: Option<String>,
}

impl SlackSendIdentity {
    pub fn is_empty(&self) -> bool {
        self.username.is_none() && self.icon_url.is_none() && self.icon_emoji.is_none()
    }
}

/// Route metadata attached to relayed events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackRelayRoute {
    /// One of `user_group`, `thread_affinity`, `channel_default`.
    pub kind: String,
    pub key: String,
}

/// A parsed relay frame.
#[derive(Debug, Clone, PartialEq)]
pub enum SlackRelayFrame {
    /// Router hello carrying an optional send identity.
    Hello { identity: Option<SlackSendIdentity> },
    /// A relayed Slack message event. Relay delivery is already authorized by
    /// the router's selected route, so the handler treats it as mentioned.
    Event { delivery_id: String, message: Value, route: SlackRelayRoute },
    /// Valid JSON but not a frame we consume.
    Ignored,
}

/// True when the account operates in router relay mode (central router
/// forwards events to the owning gateway).
pub fn slack_relay_mode_active(account: &SlackAccountConfig) -> bool {
    account.mode.as_deref().map(str::trim) == Some("relay")
}

/// Build the relay WebSocket URL: maps http(s) → ws(s), requires an explicit
/// path, rejects plaintext `ws://` for non-local hosts, and appends the
/// `gateway_id` query param (upstream `buildRelayWebSocketUrl`).
pub fn build_slack_relay_websocket_url(config: &SlackRelayConfig) -> Result<String> {
    let raw = config
        .url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("Slack relay URL is required for relay mode"))?;
    let gateway_id = config
        .gateway_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("Slack relay gatewayId is required for relay mode"))?;
    let mut url = url::Url::parse(raw)?;
    match url.scheme() {
        "http" => url.set_scheme("ws").map_err(|_| anyhow!("cannot set ws scheme"))?,
        "https" => url.set_scheme("wss").map_err(|_| anyhow!("cannot set wss scheme"))?,
        "ws" | "wss" => {}
        other => bail!("Slack relay URL must use http(s) or ws(s): {other}://"),
    }
    if url.scheme() == "ws" && !is_local_relay_host(url.host_str().unwrap_or("")) {
        bail!(
            "Slack relay URL uses plaintext ws:// for non-local host \"{}\". \
             Use wss:// for remote relay URLs; ws:// is only allowed for localhost, \
             127.0.0.1, or [::1].",
            url.host_str().unwrap_or("")
        );
    }
    if url.path().is_empty() || url.path() == "/" {
        bail!("Slack relay URL must include its websocket path: {raw}");
    }
    url.query_pairs_mut().append_pair("gateway_id", gateway_id);
    Ok(url.to_string())
}

fn is_local_relay_host(hostname: &str) -> bool {
    let normalized = hostname.trim().to_lowercase();
    let host = normalized.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(&normalized);
    if host == "localhost" || host == "::1" {
        return true;
    }
    host.parse::<std::net::Ipv4Addr>()
        .map(|ip| ip.octets()[0] == 127)
        .unwrap_or(false)
}

const SLACK_RELAY_ROUTE_KINDS: [&str; 3] = ["user_group", "thread_affinity", "channel_default"];

fn parse_relay_identity(record: &Value) -> Option<SlackSendIdentity> {
    let identity = record.get("slack_identity").or_else(|| record.get("slackIdentity"))?;
    let get = |a: &str, b: &str| {
        identity
            .get(a)
            .or_else(|| identity.get(b))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let parsed = SlackSendIdentity {
        username: get("username", "username"),
        icon_url: get("icon_url", "iconUrl"),
        icon_emoji: get("icon_emoji", "iconEmoji"),
    };
    if parsed.is_empty() { None } else { Some(parsed) }
}

/// Parse a relay frame (upstream `parseRelayFrame` + extractors). Malformed
/// JSON errors; unknown-but-valid frames map to `Ignored`.
pub fn parse_slack_relay_frame(text: &str) -> Result<SlackRelayFrame> {
    let frame: Value = serde_json::from_str(text)
        .map_err(|e| anyhow!("Slack relay received malformed JSON frame: {e}"))?;
    let Some(record) = frame.as_object() else {
        return Ok(SlackRelayFrame::Ignored);
    };
    match record.get("type").and_then(Value::as_str) {
        Some("hello") => Ok(SlackRelayFrame::Hello { identity: parse_relay_identity(&frame) }),
        Some("slack_event") => {
            let delivery_id = record.get("delivery_id").and_then(Value::as_str);
            let route = record.get("route");
            let route_kind = route.and_then(|r| r.get("kind")).and_then(Value::as_str);
            let route_key = route.and_then(|r| r.get("key")).and_then(Value::as_str);
            let event = record.get("payload").and_then(|p| p.get("event"));
            let event_ok = event
                .map(|e| {
                    e.get("type").and_then(Value::as_str) == Some("message")
                        && e.get("channel").and_then(Value::as_str).is_some()
                })
                .unwrap_or(false);
            match (delivery_id, route_kind, route_key, event, event_ok) {
                (Some(delivery_id), Some(kind), Some(key), Some(event), true)
                    if SLACK_RELAY_ROUTE_KINDS.contains(&kind) && !delivery_id.is_empty()
                        && !key.is_empty() =>
                {
                    Ok(SlackRelayFrame::Event {
                        delivery_id: delivery_id.to_string(),
                        message: event.clone(),
                        route: SlackRelayRoute { kind: kind.to_string(), key: key.to_string() },
                    })
                }
                _ => Ok(SlackRelayFrame::Ignored),
            }
        }
        _ => Ok(SlackRelayFrame::Ignored),
    }
}

/// Build the ack frame returned to the router after handling a delivery.
pub fn build_slack_relay_ack(delivery_id: &str) -> Value {
    json!({ "type": "ack", "delivery_id": delivery_id })
}

// ============================================================================
// v2026.7.1: Socket Mode Reconnect Policy (no retry cap)
// (upstream: `monitor/reconnect-policy.ts`)
// ============================================================================

/// Backoff parameters (upstream `SLACK_SOCKET_RECONNECT_POLICY`).
#[derive(Debug, Clone, Copy)]
pub struct SlackReconnectPolicy {
    pub initial_ms: u64,
    pub max_ms: u64,
    pub factor: f64,
    pub jitter: f64,
}

pub const SLACK_SOCKET_RECONNECT_POLICY: SlackReconnectPolicy =
    SlackReconnectPolicy { initial_ms: 2_000, max_ms: 30_000, factor: 1.8, jitter: 0.25 };

/// Compute the reconnect delay for `attempt` (1-based). There is NO retry
/// cap: the caller loops forever, sleeping this long between attempts.
/// `jitter_unit` in [0,1) spreads the delay by ±`policy.jitter`.
pub fn compute_slack_reconnect_delay_ms(
    policy: &SlackReconnectPolicy,
    attempt: u32,
    jitter_unit: f64,
) -> u64 {
    let attempt = attempt.max(1);
    let base = policy.initial_ms as f64 * policy.factor.powi(attempt as i32 - 1);
    let capped = base.min(policy.max_ms as f64);
    let unit = jitter_unit.clamp(0.0, 1.0);
    let jittered = capped * (1.0 + policy.jitter * (2.0 * unit - 1.0));
    jittered.max(0.0) as u64
}

static SLACK_AUTH_ERROR_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)account_inactive|invalid_auth|token_revoked|token_expired|not_authed|org_login_required|team_access_not_granted|user_removed_from_team|team_disabled|missing_scope|cannot_find_service|invalid_token",
    )
    .expect("valid regex")
});

/// Detect permanent Slack account/credential failures. Transient request and
/// HTTP failures stay in the reconnect loop (upstream
/// `isNonRecoverableSlackAuthError`).
pub fn is_non_recoverable_slack_auth_error(error_text: &str) -> bool {
    SLACK_AUTH_ERROR_RE.is_match(error_text)
}

/// Warning emitted when Socket Mode `hello` reports multiple active
/// connections for the same app (upstream
/// `formatSlackSocketModeSharedConnectionWarning`).
pub fn format_slack_shared_connection_warning(active_connections: u64) -> String {
    format!(
        "slack socket mode reports {active_connections} active connections for this Slack app; \
         Slack may deliver each event to any one connection; ensure every gateway sharing this \
         app has equivalent routing and authorization, or use a separate Slack app per gateway, \
         one relay ingress, or HTTP Request URLs behind a load balancer; \
         See https://docs.slack.dev/apis/events-api/using-socket-mode#using-multiple-connections"
    )
}

/// Extract the active connection count from a Socket Mode `hello` frame.
pub fn resolve_slack_socket_mode_connection_count(frame: &str) -> Option<u64> {
    if !frame.contains("\"hello\"") {
        return None;
    }
    let payload: Value = serde_json::from_str(frame).ok()?;
    if payload.get("type").and_then(Value::as_str) != Some("hello") {
        return None;
    }
    payload.get("num_connections").and_then(Value::as_u64)
}

// ============================================================================
// v2026.7.1: Write Serialization + Serialized Inbound Lookups
// (upstream: single-flight write client / per-account queues)
// ============================================================================

static SLACK_WRITE_MUTEX: Lazy<tokio::sync::Mutex<()>> = Lazy::new(|| tokio::sync::Mutex::new(()));
static SLACK_INBOUND_LOOKUP_LOCKS: Lazy<DashMap<String, Arc<tokio::sync::Mutex<()>>>> =
    Lazy::new(DashMap::new);

/// Process-wide Slack write serialization: all mutating Slack Web API calls
/// (postMessage, update, delete, reactions, pins) run one at a time so
/// rate-limit retries do not interleave (v2026.7.1 robustness row).
pub async fn with_slack_write_lock<T, Fut>(fut: Fut) -> T
where
    Fut: Future<Output = T>,
{
    let _guard = SLACK_WRITE_MUTEX.lock().await;
    fut.await
}

/// Per-account async mutex serializing inbound metadata lookups
/// (conversations.info / users.info bursts on event storms).
pub fn slack_inbound_lookup_lock(account_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    let key = if account_id.trim().is_empty() { "default" } else { account_id.trim() };
    SLACK_INBOUND_LOOKUP_LOCKS
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

// ============================================================================
// v2026.7.1: Unbounded Cursor Pagination
// (upstream: `resolve-allowlist-common.ts` `collectSlackCursorItems`)
// ============================================================================

/// Read `response_metadata.next_cursor` from a Slack API response.
pub fn read_slack_next_cursor(response: &Value) -> Option<String> {
    response
        .get("response_metadata")
        .and_then(|m| m.get("next_cursor"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
}

/// Unbounded thread pagination helper: repeatedly fetch pages, following
/// `next_cursor` until exhausted (no page cap — Slack ends the cursor chain).
pub async fn collect_slack_cursor_items<T, F, Fut>(mut fetch_page: F) -> Result<Vec<T>>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<(Vec<T>, Option<String>)>>,
{
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let (page_items, next) = fetch_page(cursor).await?;
        items.extend(page_items);
        match next.map(|c| c.trim().to_string()).filter(|c| !c.is_empty()) {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(items)
}

// ============================================================================
// v2026.7.1: Outbound Payloads — unfurl, replyBroadcast, blocks
// (upstream: `send.ts`, `blocks-input.ts`)
// ============================================================================

/// Maximum Block Kit blocks per message (upstream `SLACK_MAX_BLOCKS`).
pub const SLACK_MAX_BLOCKS: usize = 50;

/// Validate a Block Kit blocks array (upstream `validateSlackBlocksArray`).
pub fn validate_slack_blocks_array(raw: &Value) -> Result<Vec<Value>> {
    let Some(blocks) = raw.as_array() else {
        bail!("blocks must be an array");
    };
    if blocks.is_empty() {
        bail!("blocks must contain at least one block");
    }
    if blocks.len() > SLACK_MAX_BLOCKS {
        bail!("blocks cannot exceed {SLACK_MAX_BLOCKS} items");
    }
    for block in blocks {
        if !block.is_object() {
            bail!("each block must be an object");
        }
        match block.get("type").and_then(Value::as_str) {
            Some(t) if !t.trim().is_empty() => {}
            _ => bail!("each block must include a non-empty string type"),
        }
    }
    Ok(blocks.clone())
}

/// Per-account unfurl options.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SlackUnfurlOptions {
    pub unfurl_links: Option<bool>,
    pub unfurl_media: Option<bool>,
}

impl SlackUnfurlOptions {
    pub fn from_account(account: &SlackAccountConfig) -> Self {
        Self { unfurl_links: account.unfurl_links, unfurl_media: account.unfurl_media }
    }
}

/// Build the `chat.postMessage` payload. `unfurl_links` defaults to **false**
/// so bot messages don't expand inline link previews unless the operator opts
/// in via `channels.slack.unfurlLinks: true`; `unfurl_media` is only sent when
/// explicitly configured. `reply_broadcast` only applies together with
/// `thread_ts` (upstream `buildSlackPostMessagePayload`).
pub fn build_slack_post_message_payload(
    channel_id: &str,
    text: &str,
    thread_ts: Option<&str>,
    reply_broadcast: bool,
    blocks: Option<&[Value]>,
    unfurl: SlackUnfurlOptions,
) -> Value {
    let mut payload = json!({
        "channel": channel_id,
        "text": text,
        "unfurl_links": unfurl.unfurl_links.unwrap_or(false),
    });
    if let Some(unfurl_media) = unfurl.unfurl_media {
        payload["unfurl_media"] = Value::Bool(unfurl_media);
    }
    if let Some(ts) = thread_ts.map(str::trim).filter(|t| !t.is_empty()) {
        payload["thread_ts"] = Value::String(ts.to_string());
        if reply_broadcast {
            payload["reply_broadcast"] = Value::Bool(true);
        }
    }
    if let Some(blocks) = blocks.filter(|b| !b.is_empty()) {
        payload["blocks"] = Value::Array(blocks.to_vec());
    }
    payload
}

// ============================================================================
// v2026.7.1: Block Kit Rich Progress Rendering
// (upstream: `progress-blocks.ts`)
// ============================================================================

/// Progress render mode gated by `streaming.progress.render`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SlackProgressRender {
    #[default]
    Legacy,
    Rich,
}

/// Resolve the progress render mode from a raw config value: `"rich"`
/// activates plan/task streaming chunks.
pub fn resolve_slack_progress_render(raw: Option<&str>) -> SlackProgressRender {
    match raw.map(str::trim) {
        Some(r) if r.eq_ignore_ascii_case("rich") => SlackProgressRender::Rich,
        _ => SlackProgressRender::Legacy,
    }
}

/// One progress line in a streaming draft.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlackProgressLine {
    pub id: Option<String>,
    pub label: String,
    pub detail: Option<String>,
    pub status: Option<String>,
    pub icon: Option<String>,
    pub tool_name: Option<String>,
    pub kind: Option<String>,
    pub text: String,
}

const SLACK_PROGRESS_FIELD_MAX: usize = 1800;
const SLACK_PROGRESS_DETAIL_MAX: usize = 120;
const SLACK_PROGRESS_TASK_DETAIL_MAX: usize = 48;
const SLACK_PROGRESS_CHUNK_TEXT_MAX: usize = 256;
const SLACK_PROGRESS_TASK_TITLE_MAX: usize = 120;
const SLACK_PROGRESS_PLAN_FALLBACK_TITLE: &str = "Thinking";

/// Truncate on char boundaries with a trailing ellipsis (upstream
/// `truncateSlackText`; Rust chars avoid split surrogate halves natively).
pub fn truncate_slack_text(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return text.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let mut out: String = chars[..max_chars - 1].iter().collect();
    out.push('…');
    out
}

/// Escape mrkdwn control characters (upstream `escapeSlackMrkdwn`).
pub fn escape_slack_mrkdwn(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn compact_detail(value: &str, max_chars: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = normalized.chars().collect();
    if chars.len() <= max_chars {
        return normalized;
    }
    if max_chars <= 1 {
        return "…".to_string();
    }
    let keep_start = ((max_chars - 1) as f64 * 0.45).ceil().max(1.0) as usize;
    let keep_end = (max_chars - keep_start - 1).max(1);
    let start: String = chars[..keep_start].iter().collect();
    let end: String = chars[chars.len() - keep_end..].iter().collect();
    format!("{}…{}", start.trim_end(), end.trim_start())
}

fn line_detail_parts(line: &SlackProgressLine) -> Vec<String> {
    let mut parts = Vec::new();
    if let Some(detail) = line.detail.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        parts.push(detail.to_string());
    }
    if let Some(status) = line.status.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if status != "completed" && !line.detail.as_deref().unwrap_or("").contains(status) {
            parts.push(status.to_string());
        }
    }
    parts
}

/// Task status for rich plan chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackPlanTaskStatus {
    InProgress,
    Complete,
    Error,
}

impl SlackPlanTaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            SlackPlanTaskStatus::InProgress => "in_progress",
            SlackPlanTaskStatus::Complete => "complete",
            SlackPlanTaskStatus::Error => "error",
        }
    }
}

fn line_task_status(line: &SlackProgressLine) -> SlackPlanTaskStatus {
    let Some(status) = line.status.as_deref() else {
        return SlackPlanTaskStatus::InProgress;
    };
    let normalized = status.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    if normalized.is_empty() {
        return SlackPlanTaskStatus::InProgress;
    }
    match normalized.as_str() {
        "complete" | "completed" | "done" | "ok" | "success" | "succeeded" | "successful"
        | "exit 0" => SlackPlanTaskStatus::Complete,
        "error" | "failed" | "failure" => SlackPlanTaskStatus::Error,
        s if s.starts_with("exit ") => SlackPlanTaskStatus::Error,
        _ => SlackPlanTaskStatus::InProgress,
    }
}

fn slug_task_id_part(value: &str) -> String {
    let mut out = String::new();
    let mut last_us = false;
    for c in value.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_us = false;
        } else if !last_us {
            out.push('_');
            last_us = true;
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() { "task".to_string() } else { trimmed }
}

fn stable_task_id_part(value: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(value.as_bytes());
    let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("{}_{hex}", slug_task_id_part(value))
}

fn compact_title(value: &str) -> String {
    truncate_slack_text(
        &value.split_whitespace().collect::<Vec<_>>().join(" "),
        SLACK_PROGRESS_TASK_TITLE_MAX,
    )
}

fn line_task_title(line: &SlackProgressLine, max_line_chars: usize) -> String {
    let label = {
        let compact = line.label.split_whitespace().collect::<Vec<_>>().join(" ");
        if !compact.is_empty() {
            compact
        } else {
            line.tool_name
                .clone()
                .or_else(|| line.kind.clone())
                .unwrap_or_else(|| "Update".to_string())
        }
    };
    let detail = {
        let joined = line_detail_parts(line).join(" · ");
        if joined.is_empty() {
            line.status.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
        } else {
            Some(joined)
        }
    };
    let fallback = line.text.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(detail) = detail {
        return compact_title(&format!("{label} — {}", compact_detail(&detail, max_line_chars)));
    }
    if !fallback.is_empty() && fallback != label {
        return compact_title(&fallback);
    }
    compact_title(&label)
}

/// Build the legacy section-field draft blocks (upstream
/// `buildSlackProgressDraftBlocks`).
pub fn build_slack_progress_draft_blocks(
    label: Option<&str>,
    title: Option<&str>,
    lines: &[SlackProgressLine],
    max_line_chars: Option<usize>,
) -> Option<Vec<Value>> {
    let label = label
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .or_else(|| title.map(str::trim).filter(|t| !t.is_empty()));
    let max_line_chars = max_line_chars.filter(|m| *m > 0).unwrap_or(SLACK_PROGRESS_DETAIL_MAX);
    let field = |text: String| {
        json!({ "type": "mrkdwn", "text": truncate_slack_text(&text, SLACK_PROGRESS_FIELD_MAX) })
    };
    let mut blocks: Vec<Value> = Vec::new();
    if let Some(label) = label {
        blocks.push(json!({
            "type": "section",
            "text": field(format!("*{}*", escape_slack_mrkdwn(label)))
        }));
    }
    for line in lines {
        let icon = line.icon.as_deref().unwrap_or("•");
        let title = format!("{icon} *{}*", escape_slack_mrkdwn(&line.label));
        let detail = {
            let joined = line_detail_parts(line).join(" · ");
            if joined.is_empty() {
                "—".to_string()
            } else {
                escape_slack_mrkdwn(&compact_detail(&joined, max_line_chars))
            }
        };
        blocks.push(json!({ "type": "section", "fields": [field(title), field(detail)] }));
    }
    let start = blocks.len().saturating_sub(SLACK_MAX_BLOCKS);
    let blocks: Vec<Value> = blocks[start..].to_vec();
    if blocks.is_empty() { None } else { Some(blocks) }
}

/// Build `streaming.progress.render:"rich"` plan/task chunks (upstream
/// `buildSlackProgressStreamChunks`): a `plan_update` chunk followed by one
/// `task_update` per line. `complete_in_progress` finalizes in-progress tasks
/// on completion chunks.
pub fn build_slack_progress_stream_chunks(
    label: Option<&str>,
    title: Option<&str>,
    lines: &[SlackProgressLine],
    max_line_chars: Option<usize>,
    complete_in_progress: bool,
) -> Option<Vec<Value>> {
    let max_line_chars =
        max_line_chars.filter(|m| *m > 0).unwrap_or(SLACK_PROGRESS_TASK_DETAIL_MAX);
    let start = lines.len().saturating_sub(SLACK_MAX_BLOCKS);
    let tasks: Vec<(String, String, SlackPlanTaskStatus)> = lines[start..]
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let id = match &line.id {
                Some(id) => stable_task_id_part(id),
                None => {
                    let seed = line
                        .tool_name
                        .clone()
                        .or_else(|| line.kind.clone())
                        .unwrap_or_else(|| line.label.clone());
                    format!("{}_{}", slug_task_id_part(&seed), index + 1)
                }
            };
            (id, line_task_title(line, max_line_chars), line_task_status(line))
        })
        .collect();
    if tasks.is_empty() {
        return None;
    }
    let plan_title = truncate_slack_text(
        &title
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .or_else(|| label.map(str::trim).filter(|l| !l.is_empty()))
            .map(str::to_string)
            .unwrap_or_else(|| {
                tasks
                    .last()
                    .map(|(_, title, _)| title.clone())
                    .unwrap_or_else(|| SLACK_PROGRESS_PLAN_FALLBACK_TITLE.to_string())
            }),
        SLACK_PROGRESS_CHUNK_TEXT_MAX,
    );
    let mut chunks = vec![json!({ "type": "plan_update", "title": plan_title })];
    for (id, title, status) in tasks {
        let status = if status == SlackPlanTaskStatus::InProgress && complete_in_progress {
            SlackPlanTaskStatus::Complete
        } else {
            status
        };
        chunks.push(json!({
            "type": "task_update",
            "id": id,
            "title": title,
            "status": status.as_str(),
        }));
    }
    Some(chunks)
}

// ============================================================================
// v2026.7.1: Interactive Payload Routing (`view_submission` / `view_closed`)
// (upstream: `interactive-dispatch.ts`)
// ============================================================================

/// Classification of a Slack interactivity payload for routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackInteractiveKind {
    BlockActions,
    ViewSubmission { callback_id: Option<String>, view_id: Option<String> },
    ViewClosed { callback_id: Option<String>, view_id: Option<String> },
    Shortcut,
    MessageAction,
    Unknown,
}

/// Route an interactivity payload by its `type` (upstream interactive
/// dispatch): `view_submission` and `view_closed` carry the view callback id
/// so modal flows can resolve their pending state.
pub fn classify_slack_interactive_payload(payload: &Value) -> SlackInteractiveKind {
    let view_meta = |payload: &Value| {
        let view = payload.get("view");
        (
            view.and_then(|v| v.get("callback_id"))
                .and_then(Value::as_str)
                .map(str::to_string),
            view.and_then(|v| v.get("id")).and_then(Value::as_str).map(str::to_string),
        )
    };
    match payload.get("type").and_then(Value::as_str) {
        Some("block_actions") => SlackInteractiveKind::BlockActions,
        Some("view_submission") => {
            let (callback_id, view_id) = view_meta(payload);
            SlackInteractiveKind::ViewSubmission { callback_id, view_id }
        }
        Some("view_closed") => {
            let (callback_id, view_id) = view_meta(payload);
            SlackInteractiveKind::ViewClosed { callback_id, view_id }
        }
        Some("shortcut") => SlackInteractiveKind::Shortcut,
        Some("message_action") => SlackInteractiveKind::MessageAction,
        _ => SlackInteractiveKind::Unknown,
    }
}

// ============================================================================
// v2026.7.1: Attachment Text in Thread Context
// ============================================================================

/// Recover attachment text for thread context: legacy attachments carry
/// forwarded content in `title`/`text`/`fallback` that `message.text` omits.
pub fn slack_attachment_context_text(message: &Value) -> Option<String> {
    let attachments = message.get("attachments")?.as_array()?;
    let mut parts: Vec<String> = Vec::new();
    for attachment in attachments {
        let mut piece: Vec<String> = Vec::new();
        for key in ["title", "text"] {
            if let Some(v) = attachment
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                piece.push(v.to_string());
            }
        }
        if piece.is_empty() {
            if let Some(fallback) = attachment
                .get("fallback")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                piece.push(fallback.to_string());
            }
        }
        if !piece.is_empty() {
            parts.push(piece.join("\n"));
        }
    }
    if parts.is_empty() { None } else { Some(parts.join("\n\n")) }
}

// ============================================================================
// Slack Channel Implementation
// ============================================================================

/// Slack channel implementation.
///
/// The live transport (Socket Mode / Events API) is not wired yet; outbound
/// sends go through the Web API directly. Inbound behavior (mention gating,
/// DM canonicalization, thread participation, App Home, assistant threads,
/// relay mode) is implemented in the pure helpers above, which the future
/// socket loop calls in the documented order.
pub struct SlackChannel {
    enabled: bool,
    bot_token: Option<String>,
    app_token: Option<String>,
    unfurl: SlackUnfurlOptions,
    reply_broadcast: bool,
}

impl SlackChannel {
    pub fn new(config: &Config) -> Self {
        let account = &config.channels.slack.default_account;
        let bot_token = resolve_slack_bot_token(account);
        let app_token = resolve_slack_app_token(account);
        let enabled = account.enabled.unwrap_or(bot_token.is_some());

        Self {
            enabled,
            bot_token,
            app_token,
            unfurl: SlackUnfurlOptions::from_account(account),
            reply_broadcast: account.reply_broadcast.unwrap_or(false),
        }
    }
}

#[async_trait]
impl ChannelPlugin for SlackChannel {
    fn id(&self) -> &str {
        "slack"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Slack".to_string(),
            description: "Slack Bot channel via Socket Mode or Events API".to_string(),
            enabled: self.enabled,
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
        if !self.enabled {
            return Ok(());
        }

        let _bot_token = match &self.bot_token {
            Some(t) => t,
            None => {
                warn!("Slack channel enabled but no bot token configured");
                return Ok(());
            }
        };

        info!("Slack channel starting");

        // Integration point for the live event loop (Socket Mode when
        // `app_token` is present, Events API webhook otherwise; relay mode
        // when `mode == "relay"` — see `slack_relay_mode_active`):
        //
        // 1. `auth.test` → capture bot_user_id / bot_id; emit
        //    `format_slack_bot_token_identity_warning` when user-token shaped.
        // 2. Reconnect forever with `compute_slack_reconnect_delay_ms`
        //    (no retry cap); abort only on
        //    `is_non_recoverable_slack_auth_error`.
        // 3. On `app_home_opened`: `should_publish_slack_home_view` →
        //    `views.publish(build_slack_home_view())`.
        // 4. On `assistant_thread_started` / `_context_changed`:
        //    `normalize_slack_assistant_thread` + suggested-prompt payloads.
        // 5. On `message`: `collect_slack_mention_metadata`,
        //    `resolve_slack_channel_config`, allow-bots and
        //    `ignore_other_mentions` gates, DM canonicalization via
        //    `resolve_slack_session_key`, thread participation via
        //    `SlackThreadParticipationStore` (persisted under state_dir).
        // 6. Interactivity: `classify_slack_interactive_payload` routes
        //    `view_submission` / `view_closed`.
        if self.app_token.is_none() {
            debug!("Slack: no app token; Events API webhook mode required");
        }

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.enabled {
            info!("Slack channel stopping");
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, message: &str) -> Result<()> {
        // v2026.2.26: Suppress NO_REPLY before making API call.
        if should_suppress_message(message) {
            debug!(channel = to, "Slack: suppressing NO_REPLY message");
            return Ok(());
        }

        // v2026.7.1: never leak reasoning payloads to the channel.
        let text = strip_slack_reasoning_from_outbound(message);
        if should_suppress_message(&text) {
            debug!(channel = to, "Slack: message was reasoning-only, suppressing");
            return Ok(());
        }

        let bot_token = self
            .bot_token
            .as_deref()
            .ok_or_else(|| anyhow!("Slack bot token not configured"))?;

        let channel_id = resolve_slack_channel_id(to)?;
        let payload = build_slack_post_message_payload(
            &channel_id,
            &text,
            None,
            self.reply_broadcast,
            None,
            self.unfurl,
        );

        info!(channel = %channel_id, "Slack: sending message");

        // v2026.7.1: process-wide write serialization.
        let response: Value = with_slack_write_lock(async {
            let resp = reqwest::Client::new()
                .post("https://slack.com/api/chat.postMessage")
                .bearer_auth(bot_token)
                .json(&payload)
                .send()
                .await?;
            resp.json::<Value>().await.map_err(anyhow::Error::from)
        })
        .await?;

        if response.get("ok").and_then(Value::as_bool) != Some(true) {
            let error = response.get("error").and_then(Value::as_str).unwrap_or("unknown_error");
            bail!("Slack chat.postMessage failed: {error}");
        }

        Ok(())
    }
}

/// Convenience function called by the top-level `send_message` dispatcher.
pub(crate) async fn send_message(config: &Config, to: &str, message: &str) -> Result<()> {
    let channel = SlackChannel::new(config);
    channel.send_message(to, message).await
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- NO_REPLY --------------------------------------------------------

    #[test]
    fn no_reply_sentinel_suppressed() {
        assert!(should_suppress_message("NO_REPLY"));
        assert!(should_suppress_message("  NO_REPLY  "));
        assert!(should_suppress_message("NO_REPLY\n"));
    }

    #[test]
    fn empty_message_suppressed() {
        assert!(should_suppress_message(""));
        assert!(should_suppress_message("   "));
        assert!(should_suppress_message("\n\t"));
    }

    #[test]
    fn normal_message_not_suppressed() {
        assert!(!should_suppress_message("Hello world"));
        assert!(!should_suppress_message("NO_REPLY extra text"));
        assert!(!should_suppress_message("no_reply")); // case-sensitive
    }

    // ---- Target parsing (v5.2 row 1) -------------------------------------

    #[test]
    fn parse_target_mention_form() {
        let t = parse_slack_target("<@U12345>", None).unwrap().unwrap();
        assert_eq!(t.kind, SlackTargetKind::User);
        assert_eq!(t.id, "U12345");
        assert_eq!(t.normalized, "user:U12345");
    }

    #[test]
    fn parse_target_prefix_forms() {
        let t = parse_slack_target("user:U777", None).unwrap().unwrap();
        assert_eq!(t.kind, SlackTargetKind::User);
        let t = parse_slack_target("channel:C0123", None).unwrap().unwrap();
        assert_eq!(t.kind, SlackTargetKind::Channel);
        assert_eq!(t.id, "C0123");
        let t = parse_slack_target("slack:U999", None).unwrap().unwrap();
        assert_eq!(t.kind, SlackTargetKind::User);
    }

    #[test]
    fn parse_target_hash_requires_id() {
        // Upstream `ensureTargetId` accepts any alphanumeric candidate
        // (`/^[A-Z0-9]+$/i`) but rejects names with separators.
        assert!(parse_slack_target("#dev-ops", None).is_err());
        assert!(parse_slack_target("#general chat", None).is_err());
        let t = parse_slack_target("#C0123456", None).unwrap().unwrap();
        assert_eq!(t.kind, SlackTargetKind::Channel);
        assert_eq!(t.id, "C0123456");
    }

    #[test]
    fn parse_target_at_requires_id() {
        assert!(parse_slack_target("@bob smith", None).is_err());
        let t = parse_slack_target("@U0AB12", None).unwrap().unwrap();
        assert_eq!(t.kind, SlackTargetKind::User);
    }

    #[test]
    fn parse_target_bare_defaults_to_channel() {
        let t = parse_slack_target("C0999", None).unwrap().unwrap();
        assert_eq!(t.kind, SlackTargetKind::Channel);
        let t = parse_slack_target("U0999", Some(SlackTargetKind::User)).unwrap().unwrap();
        assert_eq!(t.kind, SlackTargetKind::User);
        assert!(parse_slack_target("   ", None).unwrap().is_none());
    }

    #[test]
    fn targets_match_across_decorations() {
        assert!(slack_targets_match("C0123", "channel:C0123"));
        assert!(slack_targets_match("channel:c0123", "C0123"));
        assert!(slack_targets_match("<@U1>", "user:U1"));
        assert!(!slack_targets_match("C0123", "C0124"));
        assert!(!slack_targets_match("user:U1", "channel:U1"));
    }

    #[test]
    fn context_targets_match_resolved_user() {
        assert!(slack_context_targets_match("U123", None, Some("user:U123")));
        assert!(slack_context_targets_match("channel:C5", Some("C5"), None));
        assert!(!slack_context_targets_match("U123", Some("C5"), Some("channel:C5")));
    }

    #[test]
    fn looks_like_target_id_shapes() {
        assert!(looks_like_slack_target_id("<@U123>"));
        assert!(looks_like_slack_target_id("channel:C1"));
        assert!(looks_like_slack_target_id("#general"));
        assert!(looks_like_slack_target_id("C012345678"));
        assert!(!looks_like_slack_target_id("general"));
        assert!(!looks_like_slack_target_id(""));
    }

    #[test]
    fn resolve_channel_id_rejects_users() {
        assert_eq!(resolve_slack_channel_id("channel:C42").unwrap(), "C42");
        assert!(resolve_slack_channel_id("<@U42>").is_err());
    }

    // ---- Thread ts -------------------------------------------------------

    #[test]
    fn thread_ts_normalization() {
        assert_eq!(
            normalize_slack_thread_ts_candidate(Some("1726000000.000100")),
            Some("1726000000.000100".to_string())
        );
        assert_eq!(normalize_slack_thread_ts_candidate(Some("nope")), None);
        assert_eq!(normalize_slack_thread_ts_candidate(None), None);
        assert_eq!(
            resolve_slack_thread_ts_value(Some("bad"), Some("1.2")),
            Some("1.2".to_string())
        );
    }

    // ---- Allowlists (v5.2 rows 1 & 6) ------------------------------------

    #[test]
    fn allowlist_matches_bare_runtime_channel_ids() {
        // Entries with/without channel:/# decoration match bare runtime ids.
        assert!(slack_allowlist_entry_matches_channel("C0123", "C0123", None));
        assert!(slack_allowlist_entry_matches_channel("channel:C0123", "C0123", None));
        assert!(slack_allowlist_entry_matches_channel("#C0123", "C0123", None));
        assert!(slack_allowlist_entry_matches_channel("c0123", "C0123", None));
        assert!(!slack_allowlist_entry_matches_channel("C0124", "C0123", None));
    }

    #[test]
    fn allowlist_matches_names_and_wildcard() {
        assert!(slack_allowlist_entry_matches_channel("#general", "C1", Some("general")));
        assert!(slack_allowlist_entry_matches_channel("General Chat", "C1", Some("general-chat")));
        assert!(slack_allowlist_entry_matches_channel("*", "C1", None));
        assert!(!slack_allowlist_entry_matches_channel("#general", "C1", Some("random")));
        assert!(slack_allowlist_matches_channel(
            &["#ops".to_string(), "C9".to_string()],
            "C9",
            None
        ));
        assert!(!slack_allowlist_matches_channel(&[], "C9", None));
    }

    #[test]
    fn slug_normalization() {
        assert_eq!(normalize_slack_slug("  General Chat  "), "general-chat");
        assert_eq!(normalize_slack_slug("a//b"), "a-b");
        assert_eq!(normalize_slack_slug("-x-"), "x");
    }

    // ---- Channel config resolution ---------------------------------------

    fn channels_map(entries: &[(&str, SlackChannelConfig)]) -> HashMap<String, SlackChannelConfig> {
        entries.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn channel_config_empty_map_allows_all() {
        let resolved = resolve_slack_channel_config(None, "C1", None, true, false);
        assert!(resolved.allowed);
        assert!(resolved.require_mention);
    }

    #[test]
    fn channel_config_nonempty_map_fails_closed() {
        let map = channels_map(&[("C_OTHER", SlackChannelConfig::default())]);
        let resolved = resolve_slack_channel_config(Some(&map), "C1", None, true, false);
        assert!(!resolved.allowed);
    }

    #[test]
    fn channel_config_matches_bare_and_decorated_ids() {
        let mut entry = SlackChannelConfig::default();
        entry.require_mention = Some(false);
        let map = channels_map(&[("channel:c0123", entry)]);
        let resolved = resolve_slack_channel_config(Some(&map), "C0123", None, true, false);
        assert!(resolved.allowed);
        assert!(!resolved.require_mention);
        assert_eq!(resolved.match_key.as_deref(), Some("channel:c0123"));
    }

    #[test]
    fn channel_config_name_matching_gated() {
        let map = channels_map(&[("#general", SlackChannelConfig::default())]);
        let closed = resolve_slack_channel_config(Some(&map), "C1", Some("general"), true, false);
        assert!(!closed.allowed);
        let open = resolve_slack_channel_config(Some(&map), "C1", Some("general"), true, true);
        assert!(open.allowed);
        assert!(open.matched_by_name);
    }

    #[test]
    fn channel_config_wildcard_fallback() {
        let mut wildcard = SlackChannelConfig::default();
        wildcard.require_mention = Some(false);
        wildcard.ignore_other_mentions = Some(true);
        let map = channels_map(&[("*", wildcard)]);
        let resolved = resolve_slack_channel_config(Some(&map), "C77", None, true, false);
        assert!(resolved.allowed);
        assert!(!resolved.require_mention);
        assert_eq!(resolved.ignore_other_mentions, Some(true));
        assert_eq!(resolved.match_key.as_deref(), Some("*"));
    }

    #[test]
    fn channel_map_name_key_warnings() {
        let keys =
            vec!["C012345678".to_string(), "#general".to_string(), "*".to_string(), "ops".to_string()];
        let warnings = slack_channel_map_name_key_warnings(&keys, false);
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("#general") || warnings[1].contains("#general"));
        assert!(slack_channel_map_name_key_warnings(&keys, true).is_empty());
    }

    // ---- Mention detection (fail-closed) ---------------------------------

    #[test]
    fn mention_metadata_collection() {
        let meta =
            collect_slack_mention_metadata("hi <@U1> and <@U2> plus <!subteam^S1> <@U1> again");
        assert_eq!(meta.mentioned_user_ids, vec!["U1", "U2"]);
        assert_eq!(meta.mentioned_subteam_ids, vec!["S1"]);
        assert!(meta.has_any_mention);
        assert!(meta.has_subteam_mention);
        let none = collect_slack_mention_metadata("plain text");
        assert!(!none.has_any_mention);
        assert!(none.mentioned_user_ids.is_empty());
    }

    #[test]
    fn mention_detection_fails_closed_without_bot_id() {
        let meta = collect_slack_mention_metadata("<@U1> hello");
        // Unknown bot user id ⇒ NOT mentioned, even though a mention exists.
        assert!(!slack_explicitly_mentioned_bot(None, &meta));
        assert!(!slack_explicitly_mentioned_bot(Some("  "), &meta));
        assert!(slack_explicitly_mentioned_bot(Some("U1"), &meta));
        assert!(slack_explicitly_mentioned_bot(Some("u1"), &meta));
        assert!(!slack_explicitly_mentioned_bot(Some("U2"), &meta));
    }

    #[test]
    fn can_detect_mention_gate() {
        assert!(!slack_can_detect_mention(None, 0));
        assert!(slack_can_detect_mention(Some("U9"), 0));
        assert!(slack_can_detect_mention(None, 2));
    }

    #[test]
    fn allow_bots_mode_resolution() {
        use SlackAllowBotsResolved as R;
        assert_eq!(resolve_slack_allow_bots_mode(None, None), R::Off);
        assert_eq!(
            resolve_slack_allow_bots_mode(None, Some(SlackAllowBots::Flag(true))),
            R::All
        );
        assert_eq!(
            resolve_slack_allow_bots_mode(
                Some(SlackAllowBots::Mode(SlackAllowBotsMode::Mentions)),
                Some(SlackAllowBots::Flag(true))
            ),
            R::Mentions
        );
        assert_eq!(
            resolve_slack_allow_bots_mode(Some(SlackAllowBots::Flag(false)), None),
            R::Off
        );
    }

    #[test]
    fn allow_bots_mentions_gating() {
        use SlackAllowBotsResolved as R;
        assert!(!slack_bot_message_allowed(R::Off, false, true));
        assert!(slack_bot_message_allowed(R::All, false, false));
        assert!(slack_bot_message_allowed(R::Mentions, true, false)); // DM
        assert!(slack_bot_message_allowed(R::Mentions, false, true)); // mentioned
        assert!(!slack_bot_message_allowed(R::Mentions, false, false));
    }

    #[test]
    fn allow_bots_deserializes_bool_and_mentions() {
        let v: SlackAllowBots = serde_json::from_str("true").unwrap();
        assert_eq!(v, SlackAllowBots::Flag(true));
        let v: SlackAllowBots = serde_json::from_str("\"mentions\"").unwrap();
        assert_eq!(v, SlackAllowBots::Mode(SlackAllowBotsMode::Mentions));
        assert!(serde_json::from_str::<SlackAllowBots>("\"sometimes\"").is_err());
    }

    #[test]
    fn ignore_other_mentions_gate() {
        let meta = collect_slack_mention_metadata("<@U2> take a look");
        // mention of someone else, bot known, not mentioned → drop
        assert!(slack_should_drop_other_mention(true, true, Some("U1"), &meta, false));
        // disabled → keep
        assert!(!slack_should_drop_other_mention(false, true, Some("U1"), &meta, false));
        // bot unknown → keep (cannot distinguish; fail open here per upstream)
        assert!(!slack_should_drop_other_mention(true, true, None, &meta, false));
        // bot itself mentioned → keep
        assert!(!slack_should_drop_other_mention(true, true, Some("U1"), &meta, true));
        // DMs unaffected
        assert!(!slack_should_drop_other_mention(true, false, Some("U1"), &meta, false));
    }

    // ---- DM canonicalization + sessions (v5.2 row 1, v7.1) ---------------

    #[test]
    fn dm_canonicalizes_to_peer() {
        assert_eq!(resolve_slack_base_conversation_id(true, Some("U7"), "D123"), "user:U7");
        assert_eq!(resolve_slack_base_conversation_id(true, None, "D123"), "user:unknown");
        assert_eq!(resolve_slack_base_conversation_id(false, Some("U7"), "C123"), "C123");
    }

    #[test]
    fn session_keys_stable_and_thread_scoped() {
        // Top-level DMs: stable peer-scoped key, ignores thread ts.
        assert_eq!(
            resolve_slack_session_key("default", SlackChatKind::Direct, "U7", None),
            "slack:default:user:U7"
        );
        assert_eq!(
            resolve_slack_session_key("default", SlackChatKind::Direct, "U7", Some("1.2")),
            "slack:default:user:U7"
        );
        // Channels: thread replies get thread-scoped sessions.
        assert_eq!(
            resolve_slack_session_key("acct", SlackChatKind::Channel, "C1", Some("1.2")),
            "slack:acct:channel:C1:thread:1.2"
        );
        assert_eq!(
            resolve_slack_session_key("acct", SlackChatKind::Channel, "C1", None),
            "slack:acct:channel:C1"
        );
        assert_eq!(
            resolve_slack_session_key("", SlackChatKind::Group, "C2", None),
            "slack:default:group:C2"
        );
    }

    #[test]
    fn dm_history_limit_honored() {
        let mut account = SlackAccountConfig::default();
        assert_eq!(resolve_slack_dm_history_limit(&account), 0);
        account.history_limit = Some(30);
        assert_eq!(resolve_slack_dm_history_limit(&account), 30);
        account.dm_history_limit = Some(5);
        assert_eq!(resolve_slack_dm_history_limit(&account), 5);
        assert!(slack_should_seed_dm_history(true, false, 5, false));
        assert!(!slack_should_seed_dm_history(true, true, 5, false));
        assert!(!slack_should_seed_dm_history(true, false, 0, false));
        assert!(!slack_should_seed_dm_history(true, false, 5, true));
        assert!(!slack_should_seed_dm_history(false, false, 5, false));
    }

    // ---- App Home (v5.2 row 2) -------------------------------------------

    #[test]
    fn home_view_shape() {
        let view = build_slack_home_view();
        assert_eq!(view["type"], "home");
        let blocks = view["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "header");
        assert_eq!(blocks[1]["type"], "section");
        assert_eq!(blocks[2]["type"], "context");
    }

    #[test]
    fn home_view_publish_gating() {
        assert!(should_publish_slack_home_view(Some("U1"), Some("home")));
        assert!(should_publish_slack_home_view(Some("U1"), None));
        assert!(!should_publish_slack_home_view(Some("U1"), Some("messages")));
        assert!(!should_publish_slack_home_view(None, Some("home")));
        assert!(!should_publish_slack_home_view(Some(" "), Some("home")));
    }

    #[test]
    fn manifest_includes_home_and_assistant_events() {
        let manifest = build_slack_manifest("TestBot");
        assert_eq!(manifest["features"]["app_home"]["home_tab_enabled"], true);
        let events: Vec<&str> = manifest["settings"]["event_subscriptions"]["bot_events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(events.contains(&"app_home_opened"));
        assert!(events.contains(&"assistant_thread_started"));
        assert!(events.contains(&"assistant_thread_context_changed"));
        let prompts =
            manifest["features"]["assistant_view"]["suggested_prompts"].as_array().unwrap();
        assert_eq!(prompts.len(), 3);
    }

    // ---- Assistant threads (v7.1) ----------------------------------------

    #[test]
    fn assistant_thread_normalization_and_merge() {
        let event = json!({
            "type": "assistant_thread_started",
            "assistant_thread": {
                "user_id": "U5",
                "channel_id": "D9",
                "thread_ts": "1.5",
                "context": { "channel_id": "C3", "team_id": "T1" }
            }
        });
        let ctx = normalize_slack_assistant_thread(&event, None).unwrap();
        assert_eq!(ctx.assistant_channel_id, "D9");
        assert_eq!(ctx.thread_ts, "1.5");
        assert_eq!(ctx.user_id.as_deref(), Some("U5"));
        assert_eq!(ctx.channel_id.as_deref(), Some("C3"));

        // Context-changed without user falls back to previous context.
        let changed = json!({
            "type": "assistant_thread_context_changed",
            "assistant_thread": { "channel_id": "D9", "thread_ts": "1.5" },
            "context": { "team_id": "T2" }
        });
        let merged = normalize_slack_assistant_thread(&changed, Some(&ctx)).unwrap();
        assert_eq!(merged.user_id.as_deref(), Some("U5"));
        assert_eq!(merged.team_id.as_deref(), Some("T2"));

        // Missing channel/thread drops the event.
        let bad = json!({ "assistant_thread": { "channel_id": "D9" } });
        assert!(normalize_slack_assistant_thread(&bad, None).is_none());
    }

    #[test]
    fn loading_messages_rotate_and_cap() {
        assert_eq!(slack_thread_loading_message(0), "Reading the thread...");
        assert_eq!(slack_thread_loading_message(4), "Reading the thread...");
        assert_eq!(slack_thread_loading_message(5), "Checking context...");
        let msgs: Vec<&str> = (0..15).map(|_| "m").collect();
        let payload = build_slack_assistant_status_payload("C1", "1.2", "is thinking...", &msgs);
        assert_eq!(payload["loading_messages"].as_array().unwrap().len(), 10);
        let bare = build_slack_assistant_status_payload("C1", "1.2", "", &[]);
        assert!(bare.get("loading_messages").is_none());
    }

    #[test]
    fn suggested_prompts_payload_shape() {
        let payload = build_slack_suggested_prompts_payload(
            "D1",
            "1.2",
            "Try asking",
            &DEFAULT_SLACK_ASSISTANT_PROMPTS,
        );
        assert_eq!(payload["prompts"].as_array().unwrap().len(), 3);
        assert_eq!(payload["title"], "Try asking");
    }

    // ---- Thread participation store (v5.2 row 3) -------------------------

    #[test]
    fn participation_store_records_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("participation.sqlite3");
        {
            let store = SlackThreadParticipationStore::open(&path).unwrap();
            store.record("default", "C1", "1.23", Some("agent-a")).unwrap();
            assert!(store.has("default", "C1", "1.23"));
            assert!(!store.has("default", "C1", "9.99"));
            assert!(!store.has("other", "C1", "1.23"));
        }
        // Reopen (simulated restart): the participation record persists.
        let store = SlackThreadParticipationStore::open(&path).unwrap();
        assert!(store.has("default", "C1", "1.23"));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn participation_store_ignores_empty_components() {
        let store = SlackThreadParticipationStore::open_in_memory().unwrap();
        store.record("", "C1", "1.2", None).unwrap();
        assert!(store.is_empty());
        assert!(!store.has("", "C1", "1.2"));
    }

    #[test]
    fn implicit_mention_kinds_from_participation() {
        use SlackImplicitMentionKind as K;
        // Reply to a bot-authored parent wins.
        assert_eq!(
            resolve_slack_implicit_mention_kinds(
                false, Some("U1"), Some("1.2"), false, false, Some("U1"), false
            ),
            vec![K::ReplyToBot]
        );
        // Otherwise persisted participation admits the reply.
        assert_eq!(
            resolve_slack_implicit_mention_kinds(
                false, Some("U1"), Some("1.2"), false, false, Some("U2"), true
            ),
            vec![K::BotThreadParticipant]
        );
        // Explicit-mention-required threads and DMs never resolve kinds.
        assert!(resolve_slack_implicit_mention_kinds(
            false, Some("U1"), Some("1.2"), true, false, Some("U1"), true
        )
        .is_empty());
        assert!(resolve_slack_implicit_mention_kinds(
            true, Some("U1"), Some("1.2"), false, false, Some("U1"), true
        )
        .is_empty());
        // Unknown bot id fails closed.
        assert!(resolve_slack_implicit_mention_kinds(
            false, None, Some("1.2"), false, false, Some("U1"), true
        )
        .is_empty());
    }

    // ---- Status reactions (v5.2 row 4) -----------------------------------

    #[test]
    fn status_reaction_plan_transitions() {
        let mut plan = SlackStatusReactionPlan::new(true, None);
        let ops = plan.transition(SlackStatusState::Queued);
        assert_eq!(ops, vec![SlackReactionOp::Add("hourglass_flowing_sand".to_string())]);
        let ops = plan.transition(SlackStatusState::Thinking);
        assert_eq!(
            ops,
            vec![
                SlackReactionOp::Remove("hourglass_flowing_sand".to_string()),
                SlackReactionOp::Add("thought_balloon".to_string()),
            ]
        );
        // Same state is a no-op.
        assert!(plan.transition(SlackStatusState::Thinking).is_empty());
        let ops = plan.clear();
        assert_eq!(ops, vec![SlackReactionOp::Remove("thought_balloon".to_string())]);
        assert!(plan.clear().is_empty());
    }

    #[test]
    fn status_reaction_plan_disabled_is_silent() {
        let mut plan = SlackStatusReactionPlan::new(false, None);
        assert!(plan.transition(SlackStatusState::Queued).is_empty());
        assert!(plan.clear().is_empty());
    }

    #[test]
    fn status_reaction_emoji_overrides() {
        let emojis = StatusReactionsEmojiConfig {
            queued: Some("eyes".to_string()),
            ..Default::default()
        };
        assert_eq!(resolve_slack_status_emoji(SlackStatusState::Queued, Some(&emojis)), "eyes");
        assert_eq!(
            resolve_slack_status_emoji(SlackStatusState::Done, Some(&emojis)),
            "white_check_mark"
        );
    }

    #[test]
    fn message_tool_only_turns_keep_status() {
        assert!(slack_keep_status_for_message_tool_turn("message_tool_only", true));
        assert!(!slack_keep_status_for_message_tool_turn("message_tool_only", false));
        assert!(!slack_keep_status_for_message_tool_turn("normal", true));
    }

    // ---- Rich text walker (v5.2 row 5) -----------------------------------

    #[test]
    fn rich_text_blocks_recover_full_dm_text() {
        // Real-shaped payload: Slack truncates message.text but the
        // top-level rich_text block carries the full content.
        let blocks = vec![json!({
            "type": "rich_text",
            "block_id": "abc",
            "elements": [{
                "type": "rich_text_section",
                "elements": [
                    { "type": "text", "text": "Hello " },
                    { "type": "user", "user_id": "U42" },
                    { "type": "text", "text": " see " },
                    { "type": "link", "url": "https://example.com", "text": "the docs" },
                    { "type": "emoji", "name": "tada" }
                ]
            }]
        })];
        let recovered = resolve_slack_blocks_text(&blocks).unwrap();
        assert!(recovered.has_rich_text);
        assert_eq!(recovered.text, "Hello <@U42> see the docs:tada:");
        let chosen = choose_slack_primary_text(Some("Hello <@U42>"), Some(&recovered)).unwrap();
        assert_eq!(chosen, "Hello <@U42> see the docs:tada:");
    }

    #[test]
    fn rich_text_lists_join_with_newlines() {
        let blocks = vec![json!({
            "type": "rich_text",
            "elements": [{
                "type": "rich_text_list",
                "style": "bullet",
                "elements": [
                    { "type": "rich_text_section",
                      "elements": [{ "type": "text", "text": "one" }] },
                    { "type": "rich_text_section",
                      "elements": [{ "type": "text", "text": "two" }] }
                ]
            }]
        })];
        assert_eq!(resolve_slack_blocks_text(&blocks).unwrap().text, "one\ntwo");
    }

    #[test]
    fn section_and_context_blocks_read_text() {
        let blocks = vec![
            json!({ "type": "section", "text": { "type": "mrkdwn", "text": "sec" } }),
            json!({ "type": "section", "fields": [
                { "type": "mrkdwn", "text": "f1" }, { "type": "mrkdwn", "text": "f2" } ] }),
            json!({ "type": "header", "text": { "type": "plain_text", "text": "head" } }),
            json!({ "type": "context", "elements": [
                { "type": "mrkdwn", "text": "c1" }, { "type": "mrkdwn", "text": "c2" } ] }),
            json!({ "type": "image", "alt_text": "alt", "image_url": "https://x/y.png" }),
            json!({ "type": "divider" }),
        ];
        let text = resolve_slack_blocks_text(&blocks).unwrap();
        assert!(!text.has_rich_text);
        assert_eq!(text.text, "sec\nf1\nf2\nhead\nc1 c2\nalt");
    }

    #[test]
    fn primary_text_prefers_longer_prefix_extension() {
        let plain = SlackBlocksText { text: "abc def".to_string(), has_rich_text: false };
        // Non-rich block text only wins when it extends message.text.
        assert_eq!(choose_slack_primary_text(Some("abc"), Some(&plain)).unwrap(), "abc def");
        assert_eq!(choose_slack_primary_text(Some("xyz"), Some(&plain)).unwrap(), "xyz");
        assert_eq!(choose_slack_primary_text(None, Some(&plain)).unwrap(), "abc def");
        assert_eq!(choose_slack_primary_text(Some("t"), None).unwrap(), "t");
    }

    // ---- Reasoning suppression (v7.1) ------------------------------------

    #[test]
    fn reasoning_stripped_from_outbound() {
        let text = "before <think>secret reasoning</think> after";
        assert_eq!(strip_slack_reasoning_from_outbound(text), "before  after".trim());
        let only = "<thinking>all reasoning</thinking>";
        assert_eq!(strip_slack_reasoning_from_outbound(only), "");
        // Stray tags removed.
        assert_eq!(strip_slack_reasoning_from_outbound("a <mm:thought> b"), "a  b".trim());
        // Plain text untouched.
        assert_eq!(strip_slack_reasoning_from_outbound("plain"), "plain");
    }

    #[test]
    fn reasoning_progress_line_normalization() {
        assert_eq!(
            normalize_slack_reasoning_progress_line("Reasoning: first\nsecond  line"),
            "first second line"
        );
        assert_eq!(
            normalize_slack_reasoning_progress_line("<think>inner\ntext</think>"),
            "inner text"
        );
    }

    #[test]
    fn reasoning_progress_merge() {
        assert_eq!(merge_slack_reasoning_progress_text("", "abc", false), "abc");
        // Same normalized content keeps current.
        assert_eq!(merge_slack_reasoning_progress_text("abc", "abc", false), "abc");
        // Snapshot replaces.
        assert_eq!(merge_slack_reasoning_progress_text("abc", "xyz", true), "xyz");
        // Prefix extension replaces.
        assert_eq!(merge_slack_reasoning_progress_text("abc", "abc def", false), "abc def");
        // Plain delta appends.
        assert_eq!(merge_slack_reasoning_progress_text("abc", " tail", false), "abc tail");
    }

    // ---- Relay mode (v7.1) -----------------------------------------------

    #[test]
    fn relay_url_building() {
        let cfg = SlackRelayConfig {
            url: Some("https://router.example.com/relay/slack".to_string()),
            auth_token: Some("tok".to_string()),
            gateway_id: Some("gw-1".to_string()),
        };
        let url = build_slack_relay_websocket_url(&cfg).unwrap();
        assert!(url.starts_with("wss://router.example.com/relay/slack"));
        assert!(url.contains("gateway_id=gw-1"));

        // http → ws only allowed for local hosts.
        let local = SlackRelayConfig {
            url: Some("http://127.0.0.1:8000/relay".to_string()),
            gateway_id: Some("gw".to_string()),
            ..Default::default()
        };
        assert!(build_slack_relay_websocket_url(&local).is_ok());
        let remote_plain = SlackRelayConfig {
            url: Some("http://router.example.com/relay".to_string()),
            gateway_id: Some("gw".to_string()),
            ..Default::default()
        };
        assert!(build_slack_relay_websocket_url(&remote_plain).is_err());

        // Path required.
        let no_path = SlackRelayConfig {
            url: Some("wss://router.example.com".to_string()),
            gateway_id: Some("gw".to_string()),
            ..Default::default()
        };
        assert!(build_slack_relay_websocket_url(&no_path).is_err());
    }

    #[test]
    fn relay_mode_activation() {
        let mut account = SlackAccountConfig::default();
        assert!(!slack_relay_mode_active(&account));
        account.mode = Some("relay".to_string());
        assert!(slack_relay_mode_active(&account));
    }

    #[test]
    fn relay_frame_parsing() {
        // Hello with identity.
        let frame = parse_slack_relay_frame(
            r#"{"type":"hello","slack_identity":{"username":"Fany","icon_emoji":":lobster:"}}"#,
        )
        .unwrap();
        match frame {
            SlackRelayFrame::Hello { identity: Some(id) } => {
                assert_eq!(id.username.as_deref(), Some("Fany"));
                assert_eq!(id.icon_emoji.as_deref(), Some(":lobster:"));
            }
            other => panic!("expected hello, got {other:?}"),
        }

        // Valid slack_event.
        let frame = parse_slack_relay_frame(
            r#"{"type":"slack_event","delivery_id":"d1",
                "route":{"kind":"thread_affinity","key":"C1:1.2"},
                "payload":{"event":{"type":"message","channel":"C1","text":"hi"}}}"#,
        )
        .unwrap();
        match frame {
            SlackRelayFrame::Event { delivery_id, route, message } => {
                assert_eq!(delivery_id, "d1");
                assert_eq!(route.kind, "thread_affinity");
                assert_eq!(message["channel"], "C1");
            }
            other => panic!("expected event, got {other:?}"),
        }

        // Unknown route kind → ignored.
        let ignored = parse_slack_relay_frame(
            r#"{"type":"slack_event","delivery_id":"d1",
                "route":{"kind":"mystery","key":"k"},
                "payload":{"event":{"type":"message","channel":"C1"}}}"#,
        )
        .unwrap();
        assert_eq!(ignored, SlackRelayFrame::Ignored);

        // Non-message event → ignored.
        let ignored = parse_slack_relay_frame(
            r#"{"type":"slack_event","delivery_id":"d1",
                "route":{"kind":"user_group","key":"k"},
                "payload":{"event":{"type":"reaction_added","channel":"C1"}}}"#,
        )
        .unwrap();
        assert_eq!(ignored, SlackRelayFrame::Ignored);

        // Malformed JSON errors.
        assert!(parse_slack_relay_frame("{not json").is_err());

        assert_eq!(build_slack_relay_ack("d1"), json!({"type":"ack","delivery_id":"d1"}));
    }

    // ---- Reconnect policy (v7.1) -----------------------------------------

    #[test]
    fn reconnect_backoff_schedule() {
        let p = &SLACK_SOCKET_RECONNECT_POLICY;
        // Deterministic mid-jitter (unit = 0.5 → no jitter applied).
        assert_eq!(compute_slack_reconnect_delay_ms(p, 1, 0.5), 2000);
        assert_eq!(compute_slack_reconnect_delay_ms(p, 2, 0.5), 3600);
        // Capped at max — and stays capped for arbitrarily large attempts
        // (no retry cap: attempt 1000 still yields a finite delay).
        assert_eq!(compute_slack_reconnect_delay_ms(p, 10, 0.5), 30000);
        assert_eq!(compute_slack_reconnect_delay_ms(p, 1000, 0.5), 30000);
        // Jitter bounds: ±25%.
        assert_eq!(compute_slack_reconnect_delay_ms(p, 1, 0.0), 1500);
        assert_eq!(compute_slack_reconnect_delay_ms(p, 1, 1.0), 2500);
    }

    #[test]
    fn non_recoverable_auth_errors() {
        assert!(is_non_recoverable_slack_auth_error("An API error occurred: invalid_auth"));
        assert!(is_non_recoverable_slack_auth_error("token_revoked"));
        assert!(is_non_recoverable_slack_auth_error("missing_scope: chat:write"));
        assert!(!is_non_recoverable_slack_auth_error("rate_limited"));
        assert!(!is_non_recoverable_slack_auth_error("socket hang up"));
    }

    #[test]
    fn shared_connection_count_detection() {
        assert_eq!(
            resolve_slack_socket_mode_connection_count(
                r#"{"type":"hello","num_connections":3}"#
            ),
            Some(3)
        );
        assert_eq!(
            resolve_slack_socket_mode_connection_count(r#"{"type":"other"}"#),
            None
        );
        assert!(format_slack_shared_connection_warning(3).contains("3 active connections"));
    }

    // ---- Write serialization + pagination (v7.1) -------------------------

    #[tokio::test]
    async fn write_lock_serializes() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CONCURRENT: AtomicUsize = AtomicUsize::new(0);
        let mut handles = Vec::new();
        for _ in 0..8 {
            handles.push(tokio::spawn(async {
                with_slack_write_lock(async {
                    let now = CONCURRENT.fetch_add(1, Ordering::SeqCst) + 1;
                    assert_eq!(now, 1, "writes must not interleave");
                    tokio::task::yield_now().await;
                    CONCURRENT.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }
    }

    #[test]
    fn inbound_lookup_lock_is_per_account() {
        let a1 = slack_inbound_lookup_lock("acct-a");
        let a2 = slack_inbound_lookup_lock("acct-a");
        let b = slack_inbound_lookup_lock("acct-b");
        assert!(Arc::ptr_eq(&a1, &a2));
        assert!(!Arc::ptr_eq(&a1, &b));
        let d1 = slack_inbound_lookup_lock("");
        let d2 = slack_inbound_lookup_lock("default");
        assert!(Arc::ptr_eq(&d1, &d2));
    }

    #[tokio::test]
    async fn cursor_pagination_is_unbounded() {
        // 120 pages — far beyond any fixed page cap.
        let pages: u32 = 120;
        let items = collect_slack_cursor_items(|cursor| async move {
            let page = cursor.as_deref().map(|c| c.parse::<u32>().unwrap()).unwrap_or(0);
            let next = if page + 1 < pages { Some((page + 1).to_string()) } else { None };
            Ok((vec![page], next))
        })
        .await
        .unwrap();
        assert_eq!(items.len(), pages as usize);
        assert_eq!(items[0], 0);
        assert_eq!(*items.last().unwrap(), pages - 1);
    }

    #[test]
    fn next_cursor_reading() {
        assert_eq!(
            read_slack_next_cursor(&json!({"response_metadata":{"next_cursor":"abc"}})),
            Some("abc".to_string())
        );
        assert_eq!(read_slack_next_cursor(&json!({"response_metadata":{"next_cursor":"  "}})), None);
        assert_eq!(read_slack_next_cursor(&json!({})), None);
    }

    // ---- Outbound payloads (v7.1) ----------------------------------------

    #[test]
    fn unfurl_defaults_off() {
        let payload = build_slack_post_message_payload(
            "C1", "hi", None, false, None, SlackUnfurlOptions::default(),
        );
        assert_eq!(payload["unfurl_links"], false);
        assert!(payload.get("unfurl_media").is_none());
        assert!(payload.get("thread_ts").is_none());
        assert!(payload.get("reply_broadcast").is_none());
    }

    #[test]
    fn unfurl_and_broadcast_opt_in() {
        let payload = build_slack_post_message_payload(
            "C1",
            "hi",
            Some("1.2"),
            true,
            None,
            SlackUnfurlOptions { unfurl_links: Some(true), unfurl_media: Some(false) },
        );
        assert_eq!(payload["unfurl_links"], true);
        assert_eq!(payload["unfurl_media"], false);
        assert_eq!(payload["thread_ts"], "1.2");
        assert_eq!(payload["reply_broadcast"], true);
        // reply_broadcast requires thread_ts.
        let no_thread = build_slack_post_message_payload(
            "C1", "hi", None, true, None, SlackUnfurlOptions::default(),
        );
        assert!(no_thread.get("reply_broadcast").is_none());
    }

    #[test]
    fn blocks_validation() {
        assert!(validate_slack_blocks_array(&json!("nope")).is_err());
        assert!(validate_slack_blocks_array(&json!([])).is_err());
        assert!(validate_slack_blocks_array(&json!([{"type":"section"}])).is_ok());
        assert!(validate_slack_blocks_array(&json!([{"no_type":true}])).is_err());
        let too_many: Vec<Value> = (0..51).map(|_| json!({"type":"section"})).collect();
        assert!(validate_slack_blocks_array(&Value::Array(too_many)).is_err());
    }

    // ---- Progress rendering (v7.1) ---------------------------------------

    #[test]
    fn progress_render_mode_gating() {
        assert_eq!(resolve_slack_progress_render(Some("rich")), SlackProgressRender::Rich);
        assert_eq!(resolve_slack_progress_render(Some("RICH")), SlackProgressRender::Rich);
        assert_eq!(resolve_slack_progress_render(Some("legacy")), SlackProgressRender::Legacy);
        assert_eq!(resolve_slack_progress_render(None), SlackProgressRender::Legacy);
    }

    fn progress_line(label: &str, status: Option<&str>) -> SlackProgressLine {
        SlackProgressLine {
            label: label.to_string(),
            status: status.map(str::to_string),
            text: label.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn rich_progress_chunks_shape() {
        let lines = vec![
            progress_line("Read file", Some("done")),
            progress_line("Run tests", None),
        ];
        let chunks =
            build_slack_progress_stream_chunks(Some("Working"), None, &lines, None, false).unwrap();
        assert_eq!(chunks[0]["type"], "plan_update");
        assert_eq!(chunks[0]["title"], "Working");
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[1]["type"], "task_update");
        assert_eq!(chunks[1]["status"], "complete");
        assert_eq!(chunks[2]["status"], "in_progress");
        // Completion pass finalizes in-progress tasks.
        let done =
            build_slack_progress_stream_chunks(Some("Working"), None, &lines, None, true).unwrap();
        assert_eq!(done[2]["status"], "complete");
        // Empty lines produce no chunks.
        assert!(build_slack_progress_stream_chunks(None, None, &[], None, false).is_none());
    }

    #[test]
    fn rich_progress_status_classification() {
        assert_eq!(line_task_status(&progress_line("x", Some("exit 0"))), SlackPlanTaskStatus::Complete);
        assert_eq!(line_task_status(&progress_line("x", Some("exit 1"))), SlackPlanTaskStatus::Error);
        assert_eq!(line_task_status(&progress_line("x", Some("failed"))), SlackPlanTaskStatus::Error);
        assert_eq!(line_task_status(&progress_line("x", None)), SlackPlanTaskStatus::InProgress);
    }

    #[test]
    fn legacy_progress_blocks_shape() {
        let lines = vec![progress_line("Step", Some("running"))];
        let blocks =
            build_slack_progress_draft_blocks(Some("Job"), None, &lines, None).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "section");
        assert!(blocks[0]["text"]["text"].as_str().unwrap().contains("*Job*"));
        assert!(blocks[1]["fields"].as_array().unwrap().len() == 2);
        assert!(build_slack_progress_draft_blocks(None, None, &[], None).is_none());
    }

    // ---- Interactive routing (v7.1) --------------------------------------

    #[test]
    fn interactive_payload_classification() {
        let submission = json!({
            "type": "view_submission",
            "view": { "id": "V1", "callback_id": "mylobster:modal" }
        });
        assert_eq!(
            classify_slack_interactive_payload(&submission),
            SlackInteractiveKind::ViewSubmission {
                callback_id: Some("mylobster:modal".to_string()),
                view_id: Some("V1".to_string())
            }
        );
        let closed = json!({ "type": "view_closed", "view": { "id": "V2" } });
        assert_eq!(
            classify_slack_interactive_payload(&closed),
            SlackInteractiveKind::ViewClosed { callback_id: None, view_id: Some("V2".to_string()) }
        );
        assert_eq!(
            classify_slack_interactive_payload(&json!({ "type": "block_actions" })),
            SlackInteractiveKind::BlockActions
        );
        assert_eq!(
            classify_slack_interactive_payload(&json!({})),
            SlackInteractiveKind::Unknown
        );
    }

    // ---- Attachment context (v7.1) ---------------------------------------

    #[test]
    fn attachment_text_recovered() {
        let message = json!({
            "text": "forwarded",
            "attachments": [
                { "title": "Original", "text": "Full forwarded body" },
                { "fallback": "fallback only" },
                { "color": "#fff" }
            ]
        });
        assert_eq!(
            slack_attachment_context_text(&message).unwrap(),
            "Original\nFull forwarded body\n\nfallback only"
        );
        assert!(slack_attachment_context_text(&json!({"text":"x"})).is_none());
        assert!(slack_attachment_context_text(&json!({"attachments":[]})).is_none());
    }

    // ---- Secrets + token warning (v7.1) ----------------------------------

    #[test]
    fn secret_input_shapes() {
        assert_eq!(resolve_slack_secret_input("xoxb-123").as_deref(), Some("xoxb-123"));
        assert_eq!(resolve_slack_secret_input("  "), None);
        std::env::set_var("SLACK_TEST_SECRET_RS", "resolved-token");
        assert_eq!(
            resolve_slack_secret_input("$SLACK_TEST_SECRET_RS").as_deref(),
            Some("resolved-token")
        );
        assert_eq!(
            resolve_slack_secret_input("${SLACK_TEST_SECRET_RS}").as_deref(),
            Some("resolved-token")
        );
        assert_eq!(
            resolve_slack_secret_input("secretref-env:SLACK_TEST_SECRET_RS").as_deref(),
            Some("resolved-token")
        );
        assert_eq!(
            resolve_slack_secret_input("__env__:SLACK_TEST_SECRET_RS").as_deref(),
            Some("resolved-token")
        );
        // Unresolvable refs fail soft (never leak the marker as a token).
        assert_eq!(resolve_slack_secret_input("$SLACK_TEST_SECRET_MISSING_RS"), None);
        // `$lowercase` is not env-shaped → literal.
        assert_eq!(resolve_slack_secret_input("$notenv").as_deref(), Some("$notenv"));
        // Structured SecretRef values.
        assert_eq!(
            resolve_slack_secret_value(&json!({"source":"env","id":"SLACK_TEST_SECRET_RS"}))
                .as_deref(),
            Some("resolved-token")
        );
        assert_eq!(resolve_slack_secret_value(&json!({"source":"exec","id":"x"})), None);
        assert_eq!(resolve_slack_secret_value(&json!(42)), None);
        std::env::remove_var("SLACK_TEST_SECRET_RS");
    }

    #[test]
    fn bot_token_identity_warning() {
        // user token shape: user_id without bot_id → warn.
        let warning =
            format_slack_bot_token_identity_warning(Some("U77"), None, Some("acct")).unwrap();
        assert!(warning.contains("U77"));
        assert!(warning.contains("channels.slack.accounts.acct.botToken"));
        assert!(warning.contains("fail closed"));
        // default account path names all three sources.
        let warning = format_slack_bot_token_identity_warning(Some("U77"), None, None).unwrap();
        assert!(warning.contains("SLACK_BOT_TOKEN"));
        // bot tokens (bot_id present) do not warn.
        assert!(format_slack_bot_token_identity_warning(Some("U77"), Some("B1"), None).is_none());
        assert!(format_slack_bot_token_identity_warning(None, None, None).is_none());
    }

    // ---- Misc helpers ----------------------------------------------------

    #[test]
    fn truncate_and_escape() {
        assert_eq!(truncate_slack_text("hello", 10), "hello");
        assert_eq!(truncate_slack_text("hello", 3), "he…");
        assert_eq!(truncate_slack_text("héllo", 3), "hé…"); // char-safe
        assert_eq!(escape_slack_mrkdwn("a<b>&c"), "a&lt;b&gt;&amp;c");
    }
}
