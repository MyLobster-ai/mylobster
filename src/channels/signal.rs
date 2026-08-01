//! Signal channel (port of OpenClaw `extensions/signal/src/*` at v2026.7.1).
//!
//! Live transport is a signal-cli JSON-RPC daemon (HTTP + SSE receive stream).
//! The pure behavior — target aliases, identity/allowlist matching, media caps
//! with base64 headroom, bounded SSE parsing, reconnect backoff, native reply
//! quotes and reaction gating — is implemented and unit-tested here; the
//! signal-cli process wiring plugs into `SignalChannel::start_account`.

use crate::config::Config;
use crate::gateway::GatewayState;

use super::plugin::{ChannelCapability, ChannelMeta, ChannelPlugin};

use anyhow::Result;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use tracing::info;

// ============================================================================
// Identity (upstream: extensions/signal/src/uuid.ts, identity.ts)
// ============================================================================

static UUID_HYPHENATED_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").unwrap()
});
static UUID_COMPACT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^[0-9a-f]{32}$").unwrap());

/// Port of `looksLikeUuid` (uuid.ts): hyphenated or compact UUID, or an
/// all-hex string that contains at least one `a-f` letter (a digit-only
/// string is a phone number, not a UUID).
pub fn looks_like_uuid(value: &str) -> bool {
    if UUID_HYPHENATED_RE.is_match(value) || UUID_COMPACT_RE.is_match(value) {
        return true;
    }
    let compact: String = value.chars().filter(|c| *c != '-').collect();
    if compact.is_empty() || !compact.chars().all(|c| c.is_ascii_hexdigit()) {
        return false;
    }
    compact.chars().any(|c| c.is_ascii_alphabetic())
}

/// Port of `normalizeE164` (src/utils.ts): strip a leading `scheme:` prefix,
/// keep digits only, return `+<digits>`. A digit-free identity yields `None`
/// — this is the upstream "digit-free identity rejection" rule.
pub fn normalize_e164(number: &str) -> Option<String> {
    static SCHEME_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^[a-z][a-z0-9-]*:").unwrap());
    let without_prefix = SCHEME_RE.replace(number, "");
    let digits: String = without_prefix.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        Some(format!("+{digits}"))
    }
}

fn strip_signal_prefix(value: &str) -> &str {
    let trimmed = value.trim();
    if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("signal:") {
        trimmed[7..].trim()
    } else {
        trimmed
    }
}

/// Inbound sender identity (upstream `SignalSender`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalSender {
    Phone { raw: String, e164: String },
    Uuid { raw: String },
}

/// Port of `resolveSignalSender`: prefer a normalizable source number,
/// fall back to the source UUID.
pub fn resolve_signal_sender(
    source_number: Option<&str>,
    source_uuid: Option<&str>,
) -> Option<SignalSender> {
    if let Some(number) = source_number.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(e164) = normalize_e164(number) {
            return Some(SignalSender::Phone { raw: number.to_string(), e164 });
        }
    }
    source_uuid
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|uuid| SignalSender::Uuid { raw: uuid.to_string() })
}

/// Port of `formatSignalSenderId`.
pub fn format_signal_sender_id(sender: &SignalSender) -> String {
    match sender {
        SignalSender::Phone { e164, .. } => e164.clone(),
        SignalSender::Uuid { raw } => format!("uuid:{raw}"),
    }
}

/// Parsed allowlist entry (upstream `SignalAllowEntry`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalAllowEntry {
    Any,
    Phone(String),
    Uuid(String),
}

/// Port of `parseSignalAllowEntry`. Digit-free entries that are not UUIDs are
/// rejected (`normalize_e164` returns `None`).
pub fn parse_signal_allow_entry(entry: &str) -> Option<SignalAllowEntry> {
    let trimmed = entry.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "*" {
        return Some(SignalAllowEntry::Any);
    }
    let stripped = strip_signal_prefix(trimmed);
    let lower = stripped.to_lowercase();
    if let Some(raw) = lower.strip_prefix("uuid:") {
        let raw = stripped[stripped.len() - raw.len()..].trim();
        if raw.is_empty() {
            return None;
        }
        return Some(SignalAllowEntry::Uuid(raw.to_string()));
    }
    if looks_like_uuid(stripped) {
        return Some(SignalAllowEntry::Uuid(stripped.to_string()));
    }
    normalize_e164(stripped).map(SignalAllowEntry::Phone)
}

/// Port of `normalizeSignalAllowRecipient`.
pub fn normalize_signal_allow_recipient(entry: &str) -> Option<String> {
    match parse_signal_allow_entry(entry)? {
        SignalAllowEntry::Any => None,
        SignalAllowEntry::Phone(e164) => Some(e164),
        SignalAllowEntry::Uuid(raw) => Some(raw),
    }
}

/// Port of `isSignalSenderAllowed`. UUID comparison is case-insensitive so
/// mixed-case ids in config still match (v2026.7.1 parity row).
pub fn is_signal_sender_allowed(sender: &SignalSender, allow_from: &[String]) -> bool {
    if allow_from.is_empty() {
        return false;
    }
    let parsed: Vec<SignalAllowEntry> =
        allow_from.iter().filter_map(|e| parse_signal_allow_entry(e)).collect();
    if parsed.iter().any(|e| matches!(e, SignalAllowEntry::Any)) {
        return true;
    }
    parsed.iter().any(|entry| match (entry, sender) {
        (SignalAllowEntry::Phone(e164), SignalSender::Phone { e164: s, .. }) => e164 == s,
        (SignalAllowEntry::Uuid(raw), SignalSender::Uuid { raw: s }) => {
            raw.eq_ignore_ascii_case(s)
        }
        _ => false,
    })
}

// ============================================================================
// Group allowlists (upstream: extensions/signal/src/monitor/access-policy.ts)
// ============================================================================

/// Port of `normalizeSignalGroupEntry`: strips `signal:`, then either the
/// `group:` prefix (yielding the group id) or the raw trimmed entry (a bare
/// entry is also a group-id candidate).
pub fn normalize_signal_group_entry(entry: &str) -> Option<String> {
    let trimmed = entry.trim();
    if trimmed.is_empty() {
        return None;
    }
    let stripped = strip_signal_prefix(trimmed);
    let lower = stripped.to_lowercase();
    if let Some(rest) = lower.strip_prefix("group:") {
        let group_id = stripped[stripped.len() - rest.len()..].trim();
        return if group_id.is_empty() { None } else { Some(group_id.to_string()) };
    }
    Some(trimmed.to_string())
}

/// Group allowlist matcher: entries match the inbound group id AND sender ids
/// (mixed-case ids compare case-insensitively). Empty `group_allow_from`
/// falls back to `allow_from` (upstream
/// `policy.groupAllowFromFallbackToAllowFrom: true`).
pub fn is_signal_group_allowed(
    group_id: &str,
    sender: &SignalSender,
    group_allow_from: &[String],
    allow_from: &[String],
) -> bool {
    let effective: &[String] =
        if group_allow_from.is_empty() { allow_from } else { group_allow_from };
    if effective.is_empty() {
        return false;
    }
    if effective.iter().any(|e| e.trim() == "*") {
        return true;
    }
    let group_matches = effective
        .iter()
        .filter_map(|e| normalize_signal_group_entry(e))
        .any(|candidate| candidate.eq_ignore_ascii_case(group_id.trim()));
    group_matches || is_signal_sender_allowed(sender, effective)
}

// ============================================================================
// Target normalization + aliases
// (upstream: extensions/signal/src/normalize.ts, aliases.ts)
// ============================================================================

/// Port of `normalizeSignalMessagingTarget`.
pub fn normalize_signal_messaging_target(raw: &str) -> Option<String> {
    let mut normalized = raw.trim();
    if normalized.is_empty() {
        return None;
    }
    normalized = strip_signal_prefix(normalized);
    if normalized.is_empty() {
        return None;
    }
    let lower = normalized.to_lowercase();
    let tail = |prefix: &str| normalized[prefix.len()..].trim().to_string();
    if lower.starts_with("group:") {
        let id = tail("group:");
        return if id.is_empty() { None } else { Some(format!("group:{id}")) };
    }
    if lower.starts_with("username:") {
        let id = tail("username:");
        return if id.is_empty() { None } else { Some(format!("username:{id}").to_lowercase()) };
    }
    if lower.starts_with("u:") {
        let id = tail("u:");
        return if id.is_empty() { None } else { Some(format!("username:{id}").to_lowercase()) };
    }
    if lower.starts_with("uuid:") {
        let id = tail("uuid:");
        return if id.is_empty() { None } else { Some(id.to_lowercase()) };
    }
    Some(normalized.to_lowercase())
}

/// Port of `looksLikeSignalTargetId`.
pub fn looks_like_signal_target_id(raw: &str, normalized: Option<&str>) -> bool {
    static PREFIXED_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?i)^(signal:)?(group:|username:|u:)").unwrap());
    static UUID_PREFIXED_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^(signal:)?uuid:").unwrap());
    static PHONE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\+?\d{3,}$").unwrap());
    static SIGNAL_PREFIX_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^signal:").unwrap());

    let candidates: Vec<&str> = [Some(raw), normalized]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect();

    for candidate in candidates {
        if PREFIXED_RE.is_match(candidate) {
            return true;
        }
        if UUID_PREFIXED_RE.is_match(candidate) {
            let stripped = SIGNAL_PREFIX_RE.replace(candidate, "");
            let stripped = Regex::new(r"(?i)^uuid:").unwrap().replace(&stripped, "");
            let stripped = stripped.trim();
            if stripped.is_empty() {
                continue;
            }
            if UUID_HYPHENATED_RE.is_match(stripped) || UUID_COMPACT_RE.is_match(stripped) {
                return true;
            }
            continue;
        }
        let without_prefix = SIGNAL_PREFIX_RE.replace(candidate, "");
        let without_prefix = without_prefix.trim();
        if UUID_HYPHENATED_RE.is_match(without_prefix)
            || UUID_COMPACT_RE.is_match(without_prefix)
            || PHONE_RE.is_match(without_prefix)
        {
            return true;
        }
    }
    false
}

/// Whether a resolved target addresses a user or a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalTargetKind {
    User,
    Group,
}

/// A resolved Signal delivery target (upstream `ResolvedSignalTarget`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSignalTarget {
    pub to: String,
    pub kind: SignalTargetKind,
    /// Set when the target was reached through a configured alias.
    pub alias: Option<String>,
}

fn normalize_alias_key(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let stripped = strip_signal_prefix(trimmed);
    let normalized = stripped.to_lowercase();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn resolve_raw_signal_target(input: &str) -> Option<(String, SignalTargetKind)> {
    let normalized = normalize_signal_messaging_target(input)?;
    if !looks_like_signal_target_id(input, Some(&normalized)) {
        return None;
    }
    let kind = if normalized.to_lowercase().starts_with("group:") {
        SignalTargetKind::Group
    } else {
        SignalTargetKind::User
    };
    Some((normalized, kind))
}

/// Port of `resolveSignalTarget` + `resolveSignalAliasTargetFromMap`:
/// raw targets win; otherwise alias chains are followed with recursion
/// detection. Errors mirror upstream (recursive alias, dangling alias).
pub fn resolve_signal_target(
    aliases: &HashMap<String, String>,
    input: &str,
) -> Result<Option<ResolvedSignalTarget>> {
    if let Some((to, kind)) = resolve_raw_signal_target(input) {
        return Ok(Some(ResolvedSignalTarget { to, kind, alias: None }));
    }

    // Normalize alias keys once (case-insensitive, `signal:` stripped).
    let mut alias_map: HashMap<String, &String> = HashMap::new();
    for (raw_key, raw_value) in aliases {
        if let Some(key) = normalize_alias_key(raw_key) {
            alias_map.insert(key, raw_value);
        }
    }

    let Some(initial_alias) = normalize_alias_key(input) else {
        return Ok(None);
    };
    if !alias_map.contains_key(&initial_alias) {
        return Ok(None);
    }

    let mut visited: HashSet<String> = HashSet::new();
    let mut alias = initial_alias.clone();
    loop {
        if !visited.insert(alias.clone()) {
            anyhow::bail!(
                "Signal alias \"{initial_alias}\" resolves recursively through \"{alias}\"."
            );
        }
        let raw_value = alias_map
            .get(&alias)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("Signal alias \"{alias}\" must point to a non-empty Signal target.")
            })?;
        if let Some((to, kind)) = resolve_raw_signal_target(raw_value) {
            return Ok(Some(ResolvedSignalTarget {
                to,
                kind,
                alias: Some(initial_alias),
            }));
        }
        if let Some(next_alias) = normalize_alias_key(raw_value) {
            if alias_map.contains_key(&next_alias) {
                alias = next_alias;
                continue;
            }
        }
        anyhow::bail!(
            "Signal alias \"{initial_alias}\" must point to an E.164 number, uuid:<id>, username:<name>, or group:<id>."
        );
    }
}

// ============================================================================
// Media size caps (upstream: extensions/signal/src/monitor.ts)
// ============================================================================

/// Headroom on the RPC response cap over the raw base64 payload size.
pub const SIGNAL_ATTACHMENT_RPC_RESPONSE_HEADROOM_BYTES: u64 = 64 * 1024;
const SIGNAL_BASE64_OVERHEAD_NUMERATOR: u64 = 4;
const SIGNAL_BASE64_OVERHEAD_DENOMINATOR: u64 = 3;
/// Default `mediaMaxMb` when unset (upstream `?? 8`).
pub const SIGNAL_DEFAULT_MEDIA_MAX_MB: f64 = 8.0;

/// `channels.signal.mediaMaxMb` -> byte cap (default 8 MB).
pub fn resolve_signal_media_max_bytes(media_max_mb: Option<f64>) -> u64 {
    let mb = match media_max_mb {
        Some(v) if v.is_finite() && v > 0.0 => v,
        _ => SIGNAL_DEFAULT_MEDIA_MAX_MB,
    };
    (mb * 1024.0 * 1024.0) as u64
}

/// Port of `deriveSignalAttachmentRpcMaxResponseBytes`: a `getAttachment` RPC
/// returns the file base64-encoded, so the HTTP response cap must account for
/// the ~4/3 expansion plus envelope headroom.
pub fn derive_signal_attachment_rpc_max_response_bytes(max_bytes: u64) -> Option<u64> {
    if max_bytes == 0 {
        return None;
    }
    let base64_bytes = (max_bytes * SIGNAL_BASE64_OVERHEAD_NUMERATOR)
        .div_ceil(SIGNAL_BASE64_OVERHEAD_DENOMINATOR);
    Some(base64_bytes + SIGNAL_ATTACHMENT_RPC_RESPONSE_HEADROOM_BYTES)
}

/// Pre-fetch size gate (upstream `fetchAttachment`): reject a declared size
/// over the cap before issuing the RPC.
pub fn check_signal_attachment_size(
    attachment_id: &str,
    declared_size: Option<u64>,
    max_bytes: u64,
) -> Result<()> {
    if let Some(size) = declared_size {
        if size > max_bytes {
            anyhow::bail!(
                "Signal attachment {attachment_id} exceeds {}MB limit",
                max_bytes / (1024 * 1024)
            );
        }
    }
    Ok(())
}

// ============================================================================
// Bounded SSE receive parsing (upstream: extensions/signal/src/client.ts)
// ============================================================================

/// Default per-request HTTP response cap.
pub const DEFAULT_SIGNAL_HTTP_RESPONSE_MAX_BYTES: u64 = 1_048_576;
/// Cap on un-drained SSE line buffer bytes.
pub const MAX_SIGNAL_SSE_BUFFER_BYTES: usize = 1_048_576;
/// Cap on accumulated `data:` bytes for a single SSE event.
pub const MAX_SIGNAL_SSE_EVENT_DATA_BYTES: usize = 1_048_576;

/// One parsed SSE event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignalSseEvent {
    pub event: Option<String>,
    pub data: Option<String>,
    pub id: Option<String>,
}

/// Incremental SSE parser with the upstream payload bounds. Feed raw chunks;
/// complete events are returned as they flush (blank line). The receive
/// stream itself is long-lived: the HTTP client driving this parser must be
/// built WITHOUT a read-idle/total-request deadline (reqwest: no
/// `.timeout(..)` on the streaming GET; connect timeout only) so an idle
/// monitor connection stays open indefinitely — the v5.2 "no 10s deadline"
/// fix.
#[derive(Debug, Default)]
pub struct SignalSseParser {
    buffer: String,
    current: SignalSseEvent,
    current_data_bytes: usize,
}

impl SignalSseParser {
    pub fn new() -> Self {
        Self::default()
    }

    fn flush_event(&mut self, out: &mut Vec<SignalSseEvent>) {
        if self.current.event.is_none() && self.current.data.is_none() && self.current.id.is_none()
        {
            return;
        }
        out.push(std::mem::take(&mut self.current));
        self.current_data_bytes = 0;
    }

    fn process_line(&mut self, line: &str, out: &mut Vec<SignalSseEvent>) -> Result<()> {
        if line.is_empty() {
            self.flush_event(out);
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, raw_value) = match line.find(':') {
            Some(idx) => (line[..idx].trim(), &line[idx + 1..]),
            None => (line.trim(), ""),
        };
        let value = raw_value.strip_prefix(' ').unwrap_or(raw_value);
        match field {
            "event" => self.current.event = Some(value.to_string()),
            "data" => {
                let segment = if self.current.data.is_some() {
                    format!("\n{value}")
                } else {
                    value.to_string()
                };
                self.current_data_bytes += segment.len();
                if self.current_data_bytes > MAX_SIGNAL_SSE_EVENT_DATA_BYTES {
                    anyhow::bail!("Signal SSE event data exceeded size limit");
                }
                match &mut self.current.data {
                    Some(data) => data.push_str(&segment),
                    None => self.current.data = Some(segment),
                }
            }
            "id" => self.current.id = Some(value.to_string()),
            _ => {}
        }
        Ok(())
    }

    /// Feed a raw chunk; returns any events completed by this chunk.
    pub fn push_chunk(&mut self, chunk: &[u8]) -> Result<Vec<SignalSseEvent>> {
        if self.buffer.len() + chunk.len() > MAX_SIGNAL_SSE_BUFFER_BYTES {
            anyhow::bail!("Signal SSE buffer exceeded size limit");
        }
        self.buffer.push_str(&String::from_utf8_lossy(chunk));
        let mut out = Vec::new();
        while let Some(line_end) = self.buffer.find('\n') {
            let mut line: String = self.buffer[..line_end].to_string();
            self.buffer.drain(..=line_end);
            if line.ends_with('\r') {
                line.pop();
            }
            self.process_line(&line, &mut out)?;
        }
        if self.buffer.len() > MAX_SIGNAL_SSE_BUFFER_BYTES {
            anyhow::bail!("Signal SSE buffer exceeded size limit");
        }
        Ok(out)
    }

    /// Flush the trailing event at stream end (upstream `flushEvent`).
    pub fn finish(&mut self) -> Vec<SignalSseEvent> {
        let mut out = Vec::new();
        self.flush_event(&mut out);
        out
    }
}

// ============================================================================
// SSE reconnect backoff (upstream: extensions/signal/src/sse-reconnect.ts,
// src/infra/backoff.ts)
// ============================================================================

/// Reconnect backoff policy (upstream `DEFAULT_RECONNECT_POLICY`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SignalBackoffPolicy {
    pub initial_ms: u64,
    pub max_ms: u64,
    pub factor: f64,
    pub jitter: f64,
}

impl Default for SignalBackoffPolicy {
    fn default() -> Self {
        Self { initial_ms: 1_000, max_ms: 10_000, factor: 2.0, jitter: 0.2 }
    }
}

/// Port of `computeBackoff`: `min(max, round(base + base*jitter*rand))` with
/// `base = initial * factor^(attempt-1)`. `jitter_unit` is a caller-supplied
/// random in `[0, 1)` so tests are deterministic.
pub fn compute_signal_backoff(policy: &SignalBackoffPolicy, attempt: u32, jitter_unit: f64) -> u64 {
    let base = policy.initial_ms as f64 * policy.factor.powi(attempt.saturating_sub(1).max(0) as i32);
    let jitter = base * policy.jitter * jitter_unit;
    ((base + jitter).round() as u64).min(policy.max_ms)
}

/// Reconnect state machine (upstream `runSignalSseLoop`): attempts reset on
/// every received event; each disconnect/error increments and yields the next
/// backoff delay. The loop reconnects forever until aborted — the receive
/// monitor is intentionally long-lived with no idle deadline.
#[derive(Debug, Default)]
pub struct SseReconnectState {
    attempts: u32,
    policy: SignalBackoffPolicy,
}

impl SseReconnectState {
    pub fn new(policy: SignalBackoffPolicy) -> Self {
        Self { attempts: 0, policy }
    }

    /// A received event proves the stream is healthy — reset the counter.
    pub fn record_event(&mut self) {
        self.attempts = 0;
    }

    /// Stream ended or errored: returns the delay before the next connect.
    pub fn next_delay_ms(&mut self, jitter_unit: f64) -> u64 {
        self.attempts += 1;
        compute_signal_backoff(&self.policy, self.attempts, jitter_unit)
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }
}

// ============================================================================
// Native reply quotes (upstream: extensions/signal/src/send.ts)
// ============================================================================

/// Quote payload for a native Signal reply (send RPC `quoteTimestamp` /
/// `quoteAuthor` / `quoteMessage` params).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalQuote {
    pub reply_to_id: String,
    pub quote_timestamp: u64,
    pub quote_author: String,
    pub quote_message: String,
}

impl SignalQuote {
    /// The extra JSON-RPC params merged into the send request.
    pub fn to_rpc_params(&self) -> serde_json::Value {
        serde_json::json!({
            "quoteTimestamp": self.quote_timestamp,
            "quoteAuthor": self.quote_author,
            "quoteMessage": self.quote_message,
        })
    }
}

fn parse_signal_reply_timestamp(raw: Option<&str>) -> Option<u64> {
    let value = raw.map(str::trim).filter(|v| !v.is_empty())?;
    if !value.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let timestamp: u64 = value.parse().ok()?;
    // Upstream requires a safe positive integer.
    if timestamp == 0 || timestamp > (1u64 << 53) {
        return None;
    }
    Some(timestamp)
}

/// Port of `resolveSignalQuoteParams`: a reply id must be a Signal message
/// timestamp (digits) and the author is required; the quoted body defaults
/// to empty.
pub fn resolve_signal_quote_params(
    reply_to_id: Option<&str>,
    reply_to_author: Option<&str>,
    reply_to_body: Option<&str>,
) -> Option<SignalQuote> {
    let timestamp = parse_signal_reply_timestamp(reply_to_id)?;
    let author = reply_to_author.map(str::trim).filter(|a| !a.is_empty())?;
    Some(SignalQuote {
        reply_to_id: timestamp.to_string(),
        quote_timestamp: timestamp,
        quote_author: author.to_string(),
        quote_message: reply_to_body.unwrap_or("").to_string(),
    })
}

/// Port of `isSignalQuoteMetadataRejection`: when the daemon rejects the quote
/// metadata the message is resent unquoted instead of dropped.
pub fn is_signal_quote_metadata_rejection(message: &str) -> bool {
    let normalized = message.to_lowercase();
    if !normalized.contains("quote") {
        return false;
    }
    ["reject", "invalid", "unrecognized", "unsupported", "not found", "no such", "unknown"]
        .iter()
        .any(|needle| normalized.contains(needle))
}

// ============================================================================
// Inbound status reactions (upstream: extensions/signal/src/reaction-level.ts,
// monitor.ts reactionNotifications gating)
// ============================================================================

/// Agent reaction level (upstream `ReactionLevel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalReactionLevel {
    Off,
    Ack,
    Minimal,
    Extensive,
}

/// Port of `resolveSignalReactionLevel`: default `minimal`, invalid values
/// fall back to `minimal`.
pub fn resolve_signal_reaction_level(value: Option<&str>) -> SignalReactionLevel {
    match value.map(str::trim).map(str::to_lowercase).as_deref() {
        Some("off") => SignalReactionLevel::Off,
        Some("ack") => SignalReactionLevel::Ack,
        Some("extensive") => SignalReactionLevel::Extensive,
        _ => SignalReactionLevel::Minimal,
    }
}

/// Inbound reaction notification mode (upstream `reactionNotifications`,
/// default `own`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalReactionNotificationMode {
    Off,
    Own,
    All,
}

pub fn resolve_signal_reaction_notification_mode(
    value: Option<&str>,
) -> SignalReactionNotificationMode {
    match value.map(str::trim).map(str::to_lowercase).as_deref() {
        Some("off") => SignalReactionNotificationMode::Off,
        Some("all") => SignalReactionNotificationMode::All,
        _ => SignalReactionNotificationMode::Own,
    }
}

/// Gate for surfacing an inbound status reaction to the agent: `off` drops
/// everything, `own` only reactions targeting the agent's own messages,
/// `all` surfaces every reaction. A non-empty `reaction_allowlist` further
/// restricts by reacting sender.
pub fn should_emit_signal_reaction_notification(
    mode: SignalReactionNotificationMode,
    target_is_own: bool,
    sender: &SignalSender,
    reaction_allowlist: &[String],
) -> bool {
    let mode_ok = match mode {
        SignalReactionNotificationMode::Off => false,
        SignalReactionNotificationMode::Own => target_is_own,
        SignalReactionNotificationMode::All => true,
    };
    if !mode_ok {
        return false;
    }
    if reaction_allowlist.is_empty() {
        return true;
    }
    is_signal_sender_allowed(sender, reaction_allowlist)
}

// ============================================================================
// Channel plugin
// ============================================================================

/// Signal channel.
///
/// Live integration point: a signal-cli JSON-RPC daemon. `start_account`
/// should spawn/attach the daemon and drive [`SignalSseParser`] +
/// [`SseReconnectState`] over a streaming HTTP GET **without a read
/// deadline** (long-lived receive monitor).
pub struct SignalChannel {
    enabled: bool,
}

impl SignalChannel {
    pub fn new(config: &Config) -> Self {
        let enabled = config.channels.signal.enabled.unwrap_or(false);

        Self { enabled }
    }
}

#[async_trait]
impl ChannelPlugin for SignalChannel {
    fn id(&self) -> &str {
        "signal"
    }

    fn meta(&self) -> ChannelMeta {
        ChannelMeta {
            name: "Signal".to_string(),
            description: "Signal Messenger channel (signal-cli JSON-RPC)".to_string(),
            enabled: self.enabled,
            multi_account: false,
        }
    }

    fn capabilities(&self) -> Vec<ChannelCapability> {
        vec![
            ChannelCapability::SendText,
            ChannelCapability::ReceiveText,
            ChannelCapability::SendMedia,
            ChannelCapability::ReceiveMedia,
            ChannelCapability::Groups,
            ChannelCapability::Reactions,
            ChannelCapability::Threads,
            ChannelCapability::ReadReceipts,
            ChannelCapability::TypingIndicators,
        ]
    }

    async fn start_account(&self, _state: &GatewayState) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        info!("Signal channel starting");
        // Integration point: spawn/attach signal-cli, then run the receive
        // loop: connect SSE (no read-idle timeout), feed SignalSseParser,
        // reset SseReconnectState on events, back off on disconnects.

        Ok(())
    }

    async fn stop_account(&self) -> Result<()> {
        if self.enabled {
            info!("Signal channel stopping");
        }
        Ok(())
    }

    async fn send_message(&self, to: &str, _message: &str) -> Result<()> {
        info!(to = to, "Signal: sending message (send RPC wiring pending)");
        // Integration point: resolve target via resolve_signal_target with
        // configured aliases, then POST the JSON-RPC send (merging
        // SignalQuote::to_rpc_params for native replies).
        Ok(())
    }
}

/// Convenience function called by the top-level `send_message` dispatcher.
pub(crate) async fn send_message(config: &Config, to: &str, message: &str) -> Result<()> {
    let channel = SignalChannel::new(config);
    channel.send_message(to, message).await
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn phone(e164: &str) -> SignalSender {
        SignalSender::Phone { raw: e164.to_string(), e164: e164.to_string() }
    }

    fn uuid(raw: &str) -> SignalSender {
        SignalSender::Uuid { raw: raw.to_string() }
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ---- identity ----

    #[test]
    fn looks_like_uuid_accepts_hyphenated_and_compact() {
        assert!(looks_like_uuid("6e8ccc9c-11a2-4b6b-a4b7-9f3e6f2f0a11"));
        assert!(looks_like_uuid("6E8CCC9C11A24B6BA4B79F3E6F2F0A11"));
        assert!(looks_like_uuid("abc-def"));
    }

    #[test]
    fn looks_like_uuid_rejects_digit_only_and_non_hex() {
        assert!(!looks_like_uuid("1234567890"));
        assert!(!looks_like_uuid("hello"));
        assert!(!looks_like_uuid(""));
    }

    #[test]
    fn normalize_e164_rejects_digit_free_identity() {
        assert_eq!(normalize_e164("+1 (555) 000-1111"), Some("+15550001111".to_string()));
        assert_eq!(normalize_e164("signal:+49123"), Some("+49123".to_string()));
        // Digit-free identity rejection (upstream rule).
        assert_eq!(normalize_e164("nobody"), None);
        assert_eq!(normalize_e164(""), None);
    }

    #[test]
    fn resolve_sender_prefers_phone_then_uuid() {
        assert_eq!(
            resolve_signal_sender(Some("+15550001111"), Some("abc")),
            Some(phone("+15550001111"))
        );
        assert_eq!(
            resolve_signal_sender(Some("not-a-number"), Some("6e8ccc9c-11a2-4b6b-a4b7-9f3e6f2f0a11")),
            Some(uuid("6e8ccc9c-11a2-4b6b-a4b7-9f3e6f2f0a11"))
        );
        assert_eq!(resolve_signal_sender(None, None), None);
    }

    #[test]
    fn parse_allow_entry_variants() {
        assert_eq!(parse_signal_allow_entry("*"), Some(SignalAllowEntry::Any));
        assert_eq!(
            parse_signal_allow_entry("signal:uuid:ABCDEF"),
            Some(SignalAllowEntry::Uuid("ABCDEF".to_string()))
        );
        assert_eq!(
            parse_signal_allow_entry("6e8ccc9c-11a2-4b6b-a4b7-9f3e6f2f0a11"),
            Some(SignalAllowEntry::Uuid("6e8ccc9c-11a2-4b6b-a4b7-9f3e6f2f0a11".to_string()))
        );
        assert_eq!(
            parse_signal_allow_entry("+1 555 000 1111"),
            Some(SignalAllowEntry::Phone("+15550001111".to_string()))
        );
        // Digit-free, non-uuid entries are rejected.
        assert_eq!(parse_signal_allow_entry("no-digits-here!"), None);
    }

    #[test]
    fn sender_allowlist_matches_phone_and_uuid_case_insensitive() {
        let allow = strings(&["+15550001111", "uuid:AABBCCDD-1122-3344-5566-778899aabbcc"]);
        assert!(is_signal_sender_allowed(&phone("+15550001111"), &allow));
        assert!(is_signal_sender_allowed(
            &uuid("aabbccdd-1122-3344-5566-778899AABBCC"),
            &allow
        ));
        assert!(!is_signal_sender_allowed(&phone("+2000"), &allow));
        assert!(!is_signal_sender_allowed(&phone("+15550001111"), &[]));
        assert!(is_signal_sender_allowed(&phone("+9"), &strings(&["*"])));
    }

    // ---- group allowlists ----

    #[test]
    fn group_allowlist_matches_group_ids_and_sender_ids() {
        let group_allow = strings(&["group:GroupIdAbc==", "+15550001111"]);
        // Group id match (mixed case, prefix stripped).
        assert!(is_signal_group_allowed("groupidabc==", &phone("+2000"), &group_allow, &[]));
        // Sender id match even when group id is unknown.
        assert!(is_signal_group_allowed("other", &phone("+15550001111"), &group_allow, &[]));
        // Neither matches.
        assert!(!is_signal_group_allowed("other", &phone("+2000"), &group_allow, &[]));
    }

    #[test]
    fn group_allowlist_bare_entry_matches_group_id() {
        // normalizeSignalGroupEntry keeps bare entries as group-id candidates.
        let group_allow = strings(&["SoMeGroupId"]);
        assert!(is_signal_group_allowed("somegroupid", &phone("+2000"), &group_allow, &[]));
    }

    #[test]
    fn group_allowlist_falls_back_to_allow_from_and_wildcard() {
        let allow = strings(&["+15550001111"]);
        assert!(is_signal_group_allowed("g1", &phone("+15550001111"), &[], &allow));
        assert!(!is_signal_group_allowed("g1", &phone("+2000"), &[], &allow));
        assert!(is_signal_group_allowed("g1", &phone("+2000"), &strings(&["*"]), &[]));
        assert!(!is_signal_group_allowed("g1", &phone("+2000"), &[], &[]));
    }

    // ---- targets + aliases ----

    #[test]
    fn normalize_messaging_target_variants() {
        assert_eq!(
            normalize_signal_messaging_target("signal:group: ABC "),
            Some("group:ABC".to_string())
        );
        assert_eq!(
            normalize_signal_messaging_target("u:Molty"),
            Some("username:molty".to_string())
        );
        assert_eq!(
            normalize_signal_messaging_target("uuid:ABC-DEF"),
            Some("abc-def".to_string())
        );
        assert_eq!(normalize_signal_messaging_target("  "), None);
    }

    #[test]
    fn looks_like_target_id_variants() {
        assert!(looks_like_signal_target_id("group:abc", None));
        assert!(looks_like_signal_target_id("signal:u:name", None));
        assert!(looks_like_signal_target_id("+15550001111", None));
        assert!(looks_like_signal_target_id("6e8ccc9c-11a2-4b6b-a4b7-9f3e6f2f0a11", None));
        assert!(!looks_like_signal_target_id("uuid:not-a-uuid", None));
        assert!(!looks_like_signal_target_id("frank", None));
    }

    #[test]
    fn alias_resolution_direct_chain_and_recursion() {
        let mut aliases = HashMap::new();
        aliases.insert("Boss".to_string(), "+15550001111".to_string());
        aliases.insert("chief".to_string(), "boss".to_string());
        aliases.insert("team".to_string(), "group:XYZ".to_string());
        aliases.insert("loop-a".to_string(), "loop-b".to_string());
        aliases.insert("loop-b".to_string(), "loop-a".to_string());
        aliases.insert("dangling".to_string(), "not a target".to_string());

        // Raw target wins without alias involvement.
        let raw = resolve_signal_target(&aliases, "+15550009999").unwrap().unwrap();
        assert_eq!(raw.to, "+15550009999");
        assert_eq!(raw.alias, None);

        // Direct alias (case-insensitive key).
        let direct = resolve_signal_target(&aliases, "signal:BOSS").unwrap().unwrap();
        assert_eq!(direct.to, "+15550001111");
        assert_eq!(direct.kind, SignalTargetKind::User);
        assert_eq!(direct.alias, Some("boss".to_string()));

        // Chained alias.
        let chained = resolve_signal_target(&aliases, "chief").unwrap().unwrap();
        assert_eq!(chained.to, "+15550001111");
        assert_eq!(chained.alias, Some("chief".to_string()));

        // Group alias kind.
        let group = resolve_signal_target(&aliases, "team").unwrap().unwrap();
        assert_eq!(group.kind, SignalTargetKind::Group);

        // Recursion + dangling errors; unknown -> None.
        assert!(resolve_signal_target(&aliases, "loop-a").is_err());
        assert!(resolve_signal_target(&aliases, "dangling").is_err());
        assert!(resolve_signal_target(&aliases, "unknown").unwrap().is_none());
    }

    // ---- media caps ----

    #[test]
    fn media_max_bytes_defaults_and_overrides() {
        assert_eq!(resolve_signal_media_max_bytes(None), 8 * 1024 * 1024);
        assert_eq!(resolve_signal_media_max_bytes(Some(50.0)), 50 * 1024 * 1024);
        assert_eq!(resolve_signal_media_max_bytes(Some(0.0)), 8 * 1024 * 1024);
        assert_eq!(resolve_signal_media_max_bytes(Some(-3.0)), 8 * 1024 * 1024);
    }

    #[test]
    fn attachment_rpc_cap_accounts_for_base64_headroom() {
        // 3 bytes -> 4 base64 bytes + headroom.
        assert_eq!(
            derive_signal_attachment_rpc_max_response_bytes(3),
            Some(4 + SIGNAL_ATTACHMENT_RPC_RESPONSE_HEADROOM_BYTES)
        );
        // Non-multiple of 3 rounds up (ceil).
        assert_eq!(
            derive_signal_attachment_rpc_max_response_bytes(4),
            Some(6 + SIGNAL_ATTACHMENT_RPC_RESPONSE_HEADROOM_BYTES)
        );
        // 8 MiB is not divisible by 3, so the expansion must round UP here too
        // — `eight_mb * 4 / 3` truncates in Rust and would understate the cap
        // by a byte, which is the wrong direction for a limit.
        let eight_mb = 8 * 1024 * 1024u64;
        assert_eq!(
            derive_signal_attachment_rpc_max_response_bytes(eight_mb),
            Some((eight_mb * 4).div_ceil(3) + SIGNAL_ATTACHMENT_RPC_RESPONSE_HEADROOM_BYTES)
        );
        assert_eq!(derive_signal_attachment_rpc_max_response_bytes(0), None);
    }

    #[test]
    fn attachment_size_gate() {
        let cap = 8 * 1024 * 1024;
        assert!(check_signal_attachment_size("a1", Some(cap), cap).is_ok());
        assert!(check_signal_attachment_size("a1", None, cap).is_ok());
        let err = check_signal_attachment_size("a1", Some(cap + 1), cap).unwrap_err();
        assert!(err.to_string().contains("exceeds 8MB limit"), "{err}");
    }

    // ---- SSE parsing ----

    #[test]
    fn sse_parser_parses_events_and_multiline_data() {
        let mut parser = SignalSseParser::new();
        let events = parser
            .push_chunk(b"event: receive\r\ndata: {\"a\":1}\ndata: more\nid: 7\n\n")
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("receive"));
        assert_eq!(events[0].data.as_deref(), Some("{\"a\":1}\nmore"));
        assert_eq!(events[0].id.as_deref(), Some("7"));
    }

    #[test]
    fn sse_parser_handles_split_chunks_comments_and_finish() {
        let mut parser = SignalSseParser::new();
        assert!(parser.push_chunk(b": keepalive\ndata: par").unwrap().is_empty());
        assert!(parser.push_chunk(b"tial\n").unwrap().is_empty());
        let tail = parser.finish();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].data.as_deref(), Some("partial"));
    }

    #[test]
    fn sse_parser_enforces_bounds() {
        let mut parser = SignalSseParser::new();
        let oversized = vec![b'x'; MAX_SIGNAL_SSE_BUFFER_BYTES + 1];
        assert!(parser.push_chunk(&oversized).is_err());

        let mut parser = SignalSseParser::new();
        // Two data lines that together exceed the per-event cap.
        let half = MAX_SIGNAL_SSE_EVENT_DATA_BYTES / 2 + 10;
        let line = format!("data: {}\n", "y".repeat(half));
        assert!(parser.push_chunk(line.as_bytes()).is_ok());
        assert!(parser.push_chunk(line.as_bytes()).is_err());
    }

    // ---- reconnect backoff ----

    #[test]
    fn backoff_growth_cap_and_jitter() {
        let policy = SignalBackoffPolicy::default();
        assert_eq!(compute_signal_backoff(&policy, 1, 0.0), 1_000);
        assert_eq!(compute_signal_backoff(&policy, 2, 0.0), 2_000);
        assert_eq!(compute_signal_backoff(&policy, 3, 0.0), 4_000);
        // Capped at max_ms.
        assert_eq!(compute_signal_backoff(&policy, 10, 0.0), 10_000);
        // Jitter adds up to base * 0.2.
        assert_eq!(compute_signal_backoff(&policy, 1, 1.0), 1_200);
    }

    #[test]
    fn reconnect_state_resets_on_event() {
        let mut state = SseReconnectState::new(SignalBackoffPolicy::default());
        assert_eq!(state.next_delay_ms(0.0), 1_000);
        assert_eq!(state.next_delay_ms(0.0), 2_000);
        state.record_event();
        assert_eq!(state.attempts(), 0);
        assert_eq!(state.next_delay_ms(0.0), 1_000);
    }

    // ---- reply quotes ----

    #[test]
    fn quote_params_require_timestamp_and_author() {
        let quote =
            resolve_signal_quote_params(Some("1712345678901"), Some("+15550001111"), Some("hi"))
                .unwrap();
        assert_eq!(quote.quote_timestamp, 1_712_345_678_901);
        assert_eq!(quote.reply_to_id, "1712345678901");
        let params = quote.to_rpc_params();
        assert_eq!(params["quoteTimestamp"], 1_712_345_678_901u64);
        assert_eq!(params["quoteAuthor"], "+15550001111");
        assert_eq!(params["quoteMessage"], "hi");

        // Body defaults to empty.
        let quote = resolve_signal_quote_params(Some("5"), Some("a"), None).unwrap();
        assert_eq!(quote.quote_message, "");

        assert!(resolve_signal_quote_params(Some("abc"), Some("a"), None).is_none());
        assert!(resolve_signal_quote_params(Some("0"), Some("a"), None).is_none());
        assert!(resolve_signal_quote_params(Some("5"), None, None).is_none());
        assert!(resolve_signal_quote_params(None, Some("a"), None).is_none());
    }

    #[test]
    fn quote_metadata_rejection_detection() {
        assert!(is_signal_quote_metadata_rejection("Invalid quote timestamp"));
        assert!(is_signal_quote_metadata_rejection("quote target not found"));
        assert!(!is_signal_quote_metadata_rejection("network unreachable"));
        assert!(!is_signal_quote_metadata_rejection("quote accepted"));
    }

    // ---- reactions ----

    #[test]
    fn reaction_level_resolution() {
        assert_eq!(resolve_signal_reaction_level(None), SignalReactionLevel::Minimal);
        assert_eq!(resolve_signal_reaction_level(Some("off")), SignalReactionLevel::Off);
        assert_eq!(resolve_signal_reaction_level(Some("ACK")), SignalReactionLevel::Ack);
        assert_eq!(
            resolve_signal_reaction_level(Some("extensive")),
            SignalReactionLevel::Extensive
        );
        assert_eq!(resolve_signal_reaction_level(Some("bogus")), SignalReactionLevel::Minimal);
    }

    #[test]
    fn reaction_notification_gating() {
        use SignalReactionNotificationMode as Mode;
        let sender = phone("+15550001111");
        assert!(!should_emit_signal_reaction_notification(Mode::Off, true, &sender, &[]));
        assert!(should_emit_signal_reaction_notification(Mode::Own, true, &sender, &[]));
        assert!(!should_emit_signal_reaction_notification(Mode::Own, false, &sender, &[]));
        assert!(should_emit_signal_reaction_notification(Mode::All, false, &sender, &[]));
        // Allowlist restricts by reacting sender.
        let allow = strings(&["+15550001111"]);
        assert!(should_emit_signal_reaction_notification(Mode::All, false, &sender, &allow));
        assert!(!should_emit_signal_reaction_notification(Mode::All, false, &phone("+2"), &allow));
        assert_eq!(resolve_signal_reaction_notification_mode(None), Mode::Own);
        assert_eq!(resolve_signal_reaction_notification_mode(Some("off")), Mode::Off);
        assert_eq!(resolve_signal_reaction_notification_mode(Some("ALL")), Mode::All);
    }
}
