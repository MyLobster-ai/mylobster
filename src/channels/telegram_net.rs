//! Telegram networking behavior: per-method Bot API request timeout guards,
//! long-poll timeout clamping, DNS result-order decisions, transport fallback
//! promotion logging, error classification, and the global sendChatAction
//! 401/transient backoff guard.
//!
//! Ports of OpenClaw `extensions/telegram/src/request-timeouts.ts`,
//! `network-config.ts`, `network-errors.ts`, parts of `fetch.ts`, and
//! `sendchataction-401-backoff.ts` at v2026.7.1.
//!
//! teloxide mapping note: teloxide 0.13 exposes a single reqwest client
//! timeout for all Bot API calls; the upstream-visible per-method guards
//! (60 s for outbound text/typing, 45 s getUpdates, etc.) are therefore
//! applied per-request via `reqwest::RequestBuilder::timeout` by the
//! `TelegramApi` layer in `telegram.rs` instead of teloxide's global client
//! timeout.

use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;

// ============================================================================
// Request timeouts (request-timeouts.ts)
// ============================================================================

pub const TELEGRAM_GET_UPDATES_REQUEST_TIMEOUT_MS: u64 = 45_000;
const TELEGRAM_OUTBOUND_TEXT_REQUEST_TIMEOUT_MS: u64 = 60_000;
const TELEGRAM_DEFAULT_LONG_POLL_TIMEOUT_SECONDS: u64 = 30;
const TELEGRAM_LONG_POLL_ABORT_MARGIN_SECONDS: u64 = 5;

/// Max timer timeout (JS `MAX_TIMER_TIMEOUT_MS` = 2^31 - 1).
pub const MAX_TIMER_TIMEOUT_MS: u64 = 2_147_483_647;

/// Base per-method Bot API request timeouts in ms. Bounds startup /
/// control-plane calls so the gateway cannot report Telegram as healthy while
/// provider startup is still hung on Bot API setup; outbound text and typing
/// carry the 60 s guard.
fn base_request_timeout_ms(method: &str) -> Option<u64> {
    Some(match method {
        "deletemycommands" => 15_000,
        "deletewebhook" => 15_000,
        "deletemessage" => 15_000,
        "editforumtopic" => 15_000,
        "editmessagetext" => 15_000,
        "getchat" => 15_000,
        "getfile" => 30_000,
        "getme" => 15_000,
        "getupdates" => TELEGRAM_GET_UPDATES_REQUEST_TIMEOUT_MS,
        "pinchatmessage" => 15_000,
        "sendanimation" => 30_000,
        "sendaudio" => 30_000,
        "sendchataction" => TELEGRAM_OUTBOUND_TEXT_REQUEST_TIMEOUT_MS,
        "senddocument" => 30_000,
        "sendmessage" => TELEGRAM_OUTBOUND_TEXT_REQUEST_TIMEOUT_MS,
        "sendmessagedraft" => TELEGRAM_OUTBOUND_TEXT_REQUEST_TIMEOUT_MS,
        "sendphoto" => 30_000,
        "sendvideo" => 30_000,
        "sendvoice" => 30_000,
        "setmessagereaction" => 10_000,
        "setmycommands" => 15_000,
        "setwebhook" => 15_000,
        _ => return None,
    })
}

/// Converts a configured `timeoutSeconds` into a timer-safe ms value
/// (upstream `resolveConfiguredTelegramRequestTimeoutMs`: floor seconds,
/// clamp to ≥ 1 s, cap at `MAX_TIMER_TIMEOUT_MS`).
fn resolve_configured_request_timeout_ms(timeout_seconds: Option<u64>) -> Option<u64> {
    let secs = timeout_seconds?.max(1);
    Some(secs.saturating_mul(1000).min(MAX_TIMER_TIMEOUT_MS))
}

/// Resolves the effective request timeout for a Bot API `method` (lowercase).
/// Configured `timeoutSeconds` can only *extend* the safe per-method guard —
/// a low configured client timeout never preempts the 60 s outbound guards.
/// `getupdates` always keeps its fixed guard.
pub fn resolve_telegram_request_timeout_ms(
    method: &str,
    timeout_seconds: Option<u64>,
) -> Option<u64> {
    if method.is_empty() {
        return None;
    }
    let base = base_request_timeout_ms(method)?;
    if method == "getupdates" {
        return Some(base);
    }
    Some(base.max(resolve_configured_request_timeout_ms(timeout_seconds).unwrap_or(0)))
}

/// Clamps the long-poll `timeout` parameter so a configured `timeoutSeconds`
/// lower than the poll window does not churn HTTPS connections: the wire
/// timeout is capped at the getUpdates HTTP guard minus a 5 s abort margin
/// (i.e. 40 s), with a 30 s default and a 1 s floor.
pub fn resolve_telegram_long_poll_timeout_seconds(timeout_seconds: Option<u64>) -> u64 {
    let max_long_poll = (TELEGRAM_GET_UPDATES_REQUEST_TIMEOUT_MS / 1000)
        .saturating_sub(TELEGRAM_LONG_POLL_ABORT_MARGIN_SECONDS)
        .max(1);
    let configured = timeout_seconds
        .map(|s| s.max(1))
        .unwrap_or(TELEGRAM_DEFAULT_LONG_POLL_TIMEOUT_SECONDS);
    configured.min(max_long_poll)
}

/// Startup probe (getMe) timeout: at least the getMe guard, extended by a
/// higher configured `timeoutSeconds` (upstream
/// `resolveTelegramStartupProbeTimeoutMs`).
pub fn resolve_telegram_startup_probe_timeout_ms(timeout_seconds: Option<u64>) -> u64 {
    let get_me = resolve_telegram_request_timeout_ms("getme", None).unwrap_or(15_000);
    match timeout_seconds {
        None => get_me,
        Some(secs) => get_me.max(resolve_configured_request_timeout_ms(Some(secs)).unwrap_or(1_000)),
    }
}

// ============================================================================
// DNS result order / autoSelectFamily decisions (network-config.ts)
// ============================================================================

pub const TELEGRAM_DNS_RESULT_ORDER_ENV: &str = "MYLOBSTER_TELEGRAM_DNS_RESULT_ORDER";
/// Upstream-compatible alias.
pub const TELEGRAM_DNS_RESULT_ORDER_ENV_COMPAT: &str = "OPENCLAW_TELEGRAM_DNS_RESULT_ORDER";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsResultOrder {
    Ipv4First,
    Verbatim,
}

impl DnsResultOrder {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "ipv4first" => Some(Self::Ipv4First),
            "verbatim" => Some(Self::Verbatim),
            _ => None,
        }
    }
}

/// DNS result-order decision with its source, for startup diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsResultOrderDecision {
    /// `None` = inherit the process default resolver order. With reqwest /
    /// getaddrinfo the process (system) DNS result order is inherited by
    /// construction, matching the upstream "inherit process DNS result order"
    /// behavior; an explicit value only records the operator's override.
    pub value: Option<DnsResultOrder>,
    pub source: Option<String>,
}

/// Resolves the DNS result order for Telegram Bot API transport.
/// Priority: env override → config `channels.telegram.network.dnsResultOrder`
/// → inherit process default (upstream `resolveTelegramDnsResultOrderDecision`;
/// the Node-22 `ipv4first` default does not apply to the Rust transport).
pub fn resolve_telegram_dns_result_order_decision(
    config_value: Option<&str>,
    env_lookup: impl Fn(&str) -> Option<String>,
) -> DnsResultOrderDecision {
    for env_name in [
        TELEGRAM_DNS_RESULT_ORDER_ENV,
        TELEGRAM_DNS_RESULT_ORDER_ENV_COMPAT,
    ] {
        if let Some(raw) = env_lookup(env_name) {
            if let Some(value) = DnsResultOrder::parse(&raw) {
                return DnsResultOrderDecision {
                    value: Some(value),
                    source: Some(format!("env:{env_name}")),
                };
            }
        }
    }
    if let Some(raw) = config_value {
        if let Some(value) = DnsResultOrder::parse(raw) {
            return DnsResultOrderDecision {
                value: Some(value),
                source: Some("config".to_string()),
            };
        }
    }
    DnsResultOrderDecision {
        value: None,
        source: None,
    }
}

// ============================================================================
// Transport fallback promotion logging (fetch.ts sticky fallback chain)
// ============================================================================

/// Log level for a transport fallback event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackLogLevel {
    Debug,
    Warn,
}

/// A loggable transport fallback event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackLogEvent {
    pub level: FallbackLogLevel,
    pub message: String,
}

/// Transport attempt tiers, mirroring the upstream fallback chain:
/// primary dispatcher → sticky IPv4-only dispatcher → pinned-IP dispatcher.
pub const TELEGRAM_TRANSPORT_PRIMARY: usize = 0;
pub const TELEGRAM_TRANSPORT_STICKY_IPV4: usize = 1;
pub const TELEGRAM_TRANSPORT_PINNED_IP: usize = 2;

/// Sticky transport fallback state machine (subset of upstream `fetch.ts`):
/// tracks the sticky attempt tier; IPv4 fallback promotions and recoveries log
/// at **debug** while pinned-IP escalations stay **warn** (v2026.7.1: "inherit
/// the process DNS result order ... and downgrade recovered sticky IPv4
/// fallback promotions to debug logs, while keeping pinned-IP escalation
/// warnings visible").
#[derive(Debug, Default)]
pub struct TelegramTransportFallback {
    sticky_attempt_index: usize,
}

impl TelegramTransportFallback {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sticky_attempt_index(&self) -> usize {
        self.sticky_attempt_index
    }

    /// Promotes the sticky tier to `next_index` (after a failure on a lower
    /// tier). Returns the log event to emit, or `None` when no promotion
    /// happens (already at or past that tier).
    pub fn promote(&mut self, next_index: usize) -> Option<FallbackLogEvent> {
        if next_index <= self.sticky_attempt_index || next_index > TELEGRAM_TRANSPORT_PINNED_IP {
            return None;
        }
        self.sticky_attempt_index = next_index;
        Some(match next_index {
            TELEGRAM_TRANSPORT_STICKY_IPV4 => FallbackLogEvent {
                level: FallbackLogLevel::Debug,
                message: "fetch fallback: enabling sticky IPv4-only dispatcher".to_string(),
            },
            _ => FallbackLogEvent {
                level: FallbackLogLevel::Warn,
                message: "fetch fallback: enabling pinned-IP dispatcher".to_string(),
            },
        })
    }

    /// Records a success on `attempt_index`. A success on a lower tier than
    /// the sticky one recovers the sticky tier downward and logs at debug.
    pub fn record_success(&mut self, attempt_index: usize) -> Option<FallbackLogEvent> {
        if self.sticky_attempt_index == TELEGRAM_TRANSPORT_PRIMARY {
            return None;
        }
        if attempt_index < self.sticky_attempt_index {
            let message = format!(
                "fetch fallback: recovered from attempt {} to attempt {}",
                self.sticky_attempt_index, attempt_index
            );
            self.sticky_attempt_index = attempt_index;
            return Some(FallbackLogEvent {
                level: FallbackLogLevel::Debug,
                message,
            });
        }
        None
    }
}

// ============================================================================
// Error classification (network-errors.ts, send.ts regexes)
// ============================================================================

static MESSAGE_NOT_MODIFIED_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)message is not modified|MESSAGE_NOT_MODIFIED").unwrap()
});
static MESSAGE_HAS_NO_TEXT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)there is no text in the message to edit").unwrap());
static MESSAGE_DELETE_NOOP_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)message to delete not found|message can't be deleted|MESSAGE_ID_INVALID|MESSAGE_DELETE_FORBIDDEN",
    )
    .unwrap()
});
static MESSAGE_EDIT_NOOP_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)message to edit not found|message can't be edited|MESSAGE_ID_INVALID").unwrap()
});
static HTML_PARSE_ERR_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)can't parse entities|parse entities|find end of the entity").unwrap()
});

/// `editMessageText` no-op: content identical — treat as success.
pub fn is_message_not_modified_error(message: &str) -> bool {
    MESSAGE_NOT_MODIFIED_RE.is_match(message)
}

/// `editMessageText` on a non-text message — benign for durable previews.
pub fn is_no_text_to_edit_error(message: &str) -> bool {
    MESSAGE_HAS_NO_TEXT_RE.is_match(message)
}

/// Benign `deleteMessage` 400s (already gone / not deletable / forbidden) —
/// treated as no-op warnings, not failures.
pub fn is_benign_delete_message_error(message: &str) -> bool {
    MESSAGE_DELETE_NOOP_RE.is_match(message)
}

/// Benign `editMessageText` 400s for durable streaming previews (message id
/// vanished or is not editable).
pub fn is_benign_edit_message_error(message: &str) -> bool {
    MESSAGE_EDIT_NOOP_RE.is_match(message)
}

/// HTML parse failures that should trigger the plain-text send fallback.
pub fn is_html_parse_error(message: &str) -> bool {
    HTML_PARSE_ERR_RE.is_match(message)
}

/// Structured Telegram Bot API error.
#[derive(Debug, Clone, thiserror::Error)]
#[error("Telegram API error {error_code:?}: {description}")]
pub struct TelegramApiError {
    pub error_code: Option<i64>,
    pub description: String,
    /// From `parameters.retry_after` (seconds), when present.
    pub retry_after_seconds: Option<u64>,
    /// Transport-level failure (timeout / connect), no Bot API response.
    pub transport: bool,
    pub timed_out: bool,
}

impl TelegramApiError {
    pub fn transport(description: impl Into<String>, timed_out: bool) -> Self {
        Self {
            error_code: None,
            description: description.into(),
            retry_after_seconds: None,
            transport: true,
            timed_out,
        }
    }
}

pub fn is_telegram_rate_limit_error(err: &TelegramApiError) -> bool {
    err.error_code == Some(429)
}

pub fn is_telegram_server_error(err: &TelegramApiError) -> bool {
    matches!(err.error_code, Some(code) if (500..600).contains(&code))
}

pub fn read_telegram_retry_after_ms(err: &TelegramApiError) -> Option<u64> {
    err.retry_after_seconds.map(|s| s.saturating_mul(1000))
}

/// Recoverable network transport errors (upstream `RECOVERABLE_ERROR_CODES`).
pub fn is_recoverable_network_error(err: &TelegramApiError) -> bool {
    err.transport
}

/// 401 detection (upstream `is401Error`): when a structured Telegram
/// `error_code` is present, trust it **exclusively** — a 429 whose message
/// contains "retry after 401" must NOT trigger the 401 suspension path.
/// Fallback for unstructured errors: case-insensitive "unauthorized" match,
/// never a bare "401" substring.
pub fn is_401_error(err: &TelegramApiError) -> bool {
    if let Some(code) = err.error_code {
        return code == 401;
    }
    err.description.to_lowercase().contains("unauthorized")
}

// ============================================================================
// v2026.7.1: transport classifiers (network-errors.ts)
// ============================================================================

/// getUpdates 409 conflict — another poller (or a stale pid after reuse) holds
/// the long poll; the transport must be marked dirty and the offset re-probed.
pub fn is_get_updates_conflict_error(err: &TelegramApiError) -> bool {
    err.error_code == Some(409)
        || err
            .description
            .to_lowercase()
            .contains("terminated by other getupdates request")
}

/// HTTP 421 Misdirected Request — safe to retry (the request never reached the
/// intended Bot API backend).
pub fn is_misdirected_request_error(err: &TelegramApiError) -> bool {
    if err.error_code == Some(421) {
        return true;
    }
    let msg = err.description.to_lowercase();
    msg.contains("421") && msg.contains("misdirected request")
}

/// Pre-connect transport failures that fire before Telegram could have
/// received the request — retrying cannot duplicate messages (upstream
/// `PRE_CONNECT_ERROR_CODES`).
const PRE_CONNECT_ERROR_MARKERS: [&str; 10] = [
    "ECONNREFUSED",
    "ENOTFOUND",
    "EAI_AGAIN",
    "ENETDOWN",
    "ENETUNREACH",
    "EHOSTUNREACH",
    "UND_ERR_CONNECT_TIMEOUT",
    "connection refused",
    "dns error",
    "connect timeout",
];

/// Safe-to-retry send errors (upstream `isSafeToRetrySendError`): HTTP 421 or
/// pre-connect failures only. Post-connect timeouts are NOT safe — Telegram
/// may have already delivered the message.
pub fn is_safe_to_retry_send_error(err: &TelegramApiError) -> bool {
    if is_misdirected_request_error(err) {
        return true;
    }
    if !err.transport {
        return false;
    }
    let msg = err.description.to_lowercase();
    PRE_CONNECT_ERROR_MARKERS
        .iter()
        .any(|marker| msg.contains(&marker.to_lowercase()))
}

/// "message thread not found" — forum topic vanished. Sends targeting a
/// thread must fail closed (never silently retry without the thread id).
pub fn is_thread_not_found_error(message: &str) -> bool {
    message.to_lowercase().contains("message thread not found")
}

// ============================================================================
// v2026.7.1: token fingerprint (token-fingerprint.ts)
// ============================================================================

/// Short, non-reversible fingerprint of a bot token for diagnostics and
/// persisted-state identity checks. Two tokens for the same bot (BotFather
/// `/revoke`) share a bot id but produce different fingerprints, letting
/// callers detect rotation without persisting the secret.
pub fn fingerprint_telegram_bot_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Whether a persisted getUpdates offset must be discarded because the bot
/// token rotated since it was stored (token-rotation offset discard).
pub fn should_discard_update_offset(
    stored_fingerprint: Option<&str>,
    current_fingerprint: &str,
) -> bool {
    match stored_fingerprint {
        None => false,
        Some(stored) => stored != current_fingerprint,
    }
}

// ============================================================================
// v2026.7.1: polling stall detection (polling-session.ts, polling-liveness.ts)
// ============================================================================

pub const DEFAULT_POLL_STALL_THRESHOLD_MS: u64 = 120_000;
pub const MIN_POLL_STALL_THRESHOLD_MS: u64 = 30_000;
pub const MAX_POLL_STALL_THRESHOLD_MS: u64 = 600_000;

/// Clamps `channels.telegram.pollingStallThresholdMs` into [30 s, 600 s],
/// defaulting to 120 s.
pub fn resolve_polling_stall_threshold_ms(value: Option<u64>) -> u64 {
    match value {
        None => DEFAULT_POLL_STALL_THRESHOLD_MS,
        Some(v) => v.clamp(MIN_POLL_STALL_THRESHOLD_MS, MAX_POLL_STALL_THRESHOLD_MS),
    }
}

/// Media-group buffer flush window (`channels.telegram.mediaGroupFlushMs`),
/// default 500 ms, floor 10 ms.
pub fn resolve_media_group_flush_ms(value: Option<u64>) -> u64 {
    match value {
        None => 500,
        Some(v) => v.max(10),
    }
}

/// A detected polling stall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramPollingStall {
    pub message: String,
}

/// Tracks getUpdates liveness so the watchdog restarts polling keyed to real
/// `getUpdates` activity (port of `TelegramPollingLivenessTracker`).
#[derive(Debug)]
pub struct TelegramPollingLivenessTracker {
    last_get_updates_at: u64,
    last_activity_at: u64,
    last_started_at: Option<u64>,
    last_finished_at: Option<u64>,
    last_outcome: String,
    in_flight: u32,
    stall_diag_logged_at: u64,
}

impl TelegramPollingLivenessTracker {
    pub fn new(now_ms: u64) -> Self {
        Self {
            last_get_updates_at: now_ms,
            last_activity_at: now_ms,
            last_started_at: None,
            last_finished_at: None,
            last_outcome: "not-started".to_string(),
            in_flight: 0,
            stall_diag_logged_at: 0,
        }
    }

    pub fn in_flight(&self) -> u32 {
        self.in_flight
    }

    pub fn note_started(&mut self, now_ms: u64) {
        self.last_get_updates_at = now_ms;
        self.last_activity_at = now_ms;
        self.last_started_at = Some(now_ms);
        self.in_flight += 1;
        self.last_outcome = "started".to_string();
    }

    pub fn note_success(&mut self, update_count: usize, now_ms: u64) {
        self.last_activity_at = now_ms;
        self.last_finished_at = Some(now_ms);
        self.last_outcome = format!("ok:{update_count}");
    }

    pub fn note_error(&mut self, now_ms: u64) {
        self.last_activity_at = now_ms;
        self.last_finished_at = Some(now_ms);
        self.last_outcome = "error".to_string();
    }

    pub fn note_finished(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }

    /// Detects a polling stall: an in-flight getUpdates without activity past
    /// the threshold, or no completed getUpdates past the threshold while
    /// idle. Repeat diagnoses are suppressed for half a threshold window.
    pub fn detect_stall(&mut self, threshold_ms: u64, now_ms: u64) -> Option<TelegramPollingStall> {
        let elapsed = if self.in_flight > 0 && self.last_started_at.is_some() {
            now_ms.saturating_sub(self.last_activity_at)
        } else if self.in_flight > 0 {
            0
        } else {
            now_ms.saturating_sub(self.last_finished_at.unwrap_or(self.last_get_updates_at))
        };
        if elapsed <= threshold_ms {
            return None;
        }
        if self.stall_diag_logged_at != 0
            && now_ms.saturating_sub(self.stall_diag_logged_at) < threshold_ms / 2
        {
            return None;
        }
        self.stall_diag_logged_at = now_ms;
        let elapsed_label = if self.in_flight > 0 {
            format!("active getUpdates stuck for {elapsed}ms")
        } else {
            format!("no completed getUpdates for {elapsed}ms")
        };
        Some(TelegramPollingStall {
            message: format!(
                "Polling stall detected ({elapsed_label}); forcing restart. \
                 [diag inFlight={} outcome={}]",
                self.in_flight, self.last_outcome
            ),
        })
    }
}

// ============================================================================
// sendChatAction 401 / transient backoff guard (sendchataction-401-backoff.ts)
// ============================================================================

const BACKOFF_INITIAL_MS: u64 = 1_000;
const BACKOFF_MAX_MS: u64 = 300_000; // 5 minutes
const BACKOFF_FACTOR: u64 = 2;
pub const DEFAULT_MAX_CONSECUTIVE_401: u32 = 10;

/// Deterministic exponential backoff (jitter is applied by the caller when
/// sleeping): 1 s → 2 s → 4 s → … → 5 min.
pub fn compute_backoff_ms(attempt: u32) -> u64 {
    if attempt == 0 {
        return 0;
    }
    let exp = attempt.saturating_sub(1).min(63);
    BACKOFF_INITIAL_MS
        .saturating_mul(BACKOFF_FACTOR.saturating_pow(exp))
        .min(BACKOFF_MAX_MS)
}

/// Decision for a sendChatAction attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatActionDecision {
    /// Guard is suspended — silently skip.
    Suspended,
    /// Transient cooldown active — reject so typing keepalive loops can count
    /// the failure and stop instead of silently hammering Telegram.
    TransientCooldown { remaining_ms: u64 },
    /// Coalesced with a recent same-chat/action call — skip.
    Coalesced,
    /// Proceed; sleep `backoff_ms` first when a 401 streak is active.
    Proceed { backoff_ms: u64 },
}

/// Global (per-account) sendChatAction guard: tracks 401 and transient errors
/// across all message contexts, preventing the infinite typing loop that got
/// bots deleted upstream (#27092). After `max_consecutive_401` failures
/// (default 10) all sendChatAction calls are suspended until `reset()`.
#[derive(Debug)]
pub struct TelegramSendChatActionGuard {
    consecutive_401_failures: u32,
    consecutive_transient_failures: u32,
    suspended: bool,
    transient_cooldown_until_ms: u64,
    blocked_until_by_key: HashMap<String, u64>,
    max_consecutive_401: u32,
    min_interval_ms: u64,
}

impl TelegramSendChatActionGuard {
    pub fn new(max_consecutive_401: u32, min_interval_ms: u64) -> Self {
        Self {
            consecutive_401_failures: 0,
            consecutive_transient_failures: 0,
            suspended: false,
            transient_cooldown_until_ms: 0,
            blocked_until_by_key: HashMap::new(),
            max_consecutive_401,
            min_interval_ms,
        }
    }

    pub fn is_suspended(&self) -> bool {
        self.suspended
    }

    pub fn reset(&mut self) {
        self.consecutive_401_failures = 0;
        self.consecutive_transient_failures = 0;
        self.suspended = false;
        self.transient_cooldown_until_ms = 0;
        self.blocked_until_by_key.clear();
    }

    fn coalesce_key(&self, chat_id: &str, action: &str) -> Option<String> {
        (self.min_interval_ms > 0).then(|| format!("{chat_id}:{action}"))
    }

    /// Call before attempting sendChatAction. `now_ms` is a monotonic clock.
    pub fn begin_attempt(&mut self, chat_id: &str, action: &str, now_ms: u64) -> ChatActionDecision {
        if self.suspended {
            return ChatActionDecision::Suspended;
        }
        if self.transient_cooldown_until_ms > now_ms {
            return ChatActionDecision::TransientCooldown {
                remaining_ms: self.transient_cooldown_until_ms - now_ms,
            };
        }
        if let Some(key) = self.coalesce_key(chat_id, action) {
            if let Some(&blocked_until) = self.blocked_until_by_key.get(&key) {
                if now_ms < blocked_until {
                    return ChatActionDecision::Coalesced;
                }
            }
            // Block concurrent attempts until finish_attempt records the window.
            self.blocked_until_by_key.insert(key, u64::MAX);
        }
        let backoff_ms = if self.consecutive_401_failures > 0 {
            compute_backoff_ms(self.consecutive_401_failures)
        } else {
            0
        };
        ChatActionDecision::Proceed { backoff_ms }
    }

    /// Records a successful sendChatAction.
    pub fn record_success(&mut self) {
        if self.consecutive_401_failures > 0 {
            tracing::info!(
                "sendChatAction recovered after {} consecutive 401 failures",
                self.consecutive_401_failures
            );
            self.consecutive_401_failures = 0;
        }
        self.consecutive_transient_failures = 0;
        self.transient_cooldown_until_ms = 0;
    }

    /// Records a failed sendChatAction. Returns `true` when the guard just
    /// entered the suspended state (caller should log the critical warning).
    pub fn record_failure(
        &mut self,
        err: &TelegramApiError,
        chat_id: &str,
        action: &str,
        attempted_at_ms: u64,
        now_ms: u64,
    ) -> bool {
        let mut just_suspended = false;
        if is_401_error(err) {
            self.consecutive_transient_failures = 0;
            self.transient_cooldown_until_ms = 0;
            self.consecutive_401_failures += 1;
            if self.consecutive_401_failures >= self.max_consecutive_401 {
                self.suspended = true;
                just_suspended = true;
                tracing::warn!(
                    "CRITICAL: sendChatAction suspended after {} consecutive 401 errors. \
                     Bot token is likely invalid. Telegram may DELETE the bot if requests continue. \
                     Replace the token and restart the telegram channel.",
                    self.consecutive_401_failures
                );
            } else {
                tracing::warn!(
                    "sendChatAction 401 error ({}/{}). Retrying with exponential backoff.",
                    self.consecutive_401_failures,
                    self.max_consecutive_401
                );
            }
        } else if is_telegram_rate_limit_error(err)
            || is_telegram_server_error(err)
            || is_recoverable_network_error(err)
        {
            self.consecutive_transient_failures += 1;
            let cooldown_ms = read_telegram_retry_after_ms(err)
                .filter(|&ms| ms > 0)
                .unwrap_or_else(|| compute_backoff_ms(self.consecutive_transient_failures));
            // Keep transient failures rejected through the same-chat coalesce
            // window; otherwise the next typing keepalive can look successful
            // and reset its guard.
            let coalescing_until = if self.coalesce_key(chat_id, action).is_some() {
                attempted_at_ms + self.min_interval_ms
            } else {
                0
            };
            self.transient_cooldown_until_ms = (now_ms + cooldown_ms).max(coalescing_until);
            tracing::warn!(
                "sendChatAction transient error ({}). Cooling down {}ms before retry.",
                self.consecutive_transient_failures,
                self.transient_cooldown_until_ms.saturating_sub(now_ms)
            );
        } else {
            self.consecutive_transient_failures = 0;
            self.transient_cooldown_until_ms = 0;
        }
        just_suspended
    }

    /// Always call after an attempt (success or failure) to open the
    /// same-chat coalesce window.
    pub fn finish_attempt(&mut self, chat_id: &str, action: &str, attempted_at_ms: u64) {
        if let Some(key) = self.coalesce_key(chat_id, action) {
            self.blocked_until_by_key
                .insert(key, attempted_at_ms + self.min_interval_ms);
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- request timeouts ----

    #[test]
    fn outbound_text_and_typing_carry_60s_guard() {
        assert_eq!(
            resolve_telegram_request_timeout_ms("sendmessage", None),
            Some(60_000)
        );
        assert_eq!(
            resolve_telegram_request_timeout_ms("sendchataction", None),
            Some(60_000)
        );
    }

    #[test]
    fn low_configured_timeout_never_preempts_guard() {
        // configured 5 s must not lower the 60 s outbound guard
        assert_eq!(
            resolve_telegram_request_timeout_ms("sendmessage", Some(5)),
            Some(60_000)
        );
        assert_eq!(
            resolve_telegram_request_timeout_ms("editmessagetext", Some(1)),
            Some(15_000)
        );
    }

    #[test]
    fn high_configured_timeout_extends_safe_methods() {
        assert_eq!(
            resolve_telegram_request_timeout_ms("sendmessage", Some(120)),
            Some(120_000)
        );
        assert_eq!(
            resolve_telegram_request_timeout_ms("getfile", Some(90)),
            Some(90_000)
        );
    }

    #[test]
    fn getupdates_guard_is_fixed() {
        assert_eq!(
            resolve_telegram_request_timeout_ms("getupdates", Some(600)),
            Some(45_000)
        );
    }

    #[test]
    fn unknown_method_has_no_timeout() {
        assert_eq!(resolve_telegram_request_timeout_ms("unknownmethod", None), None);
        assert_eq!(resolve_telegram_request_timeout_ms("", Some(10)), None);
    }

    // ---- long-poll clamping ----

    #[test]
    fn long_poll_default_is_30s() {
        assert_eq!(resolve_telegram_long_poll_timeout_seconds(None), 30);
    }

    #[test]
    fn long_poll_clamped_to_http_guard_minus_margin() {
        // 45s guard - 5s margin = 40s cap
        assert_eq!(resolve_telegram_long_poll_timeout_seconds(Some(600)), 40);
    }

    #[test]
    fn long_poll_low_config_floor_is_1s() {
        assert_eq!(resolve_telegram_long_poll_timeout_seconds(Some(0)), 1);
        assert_eq!(resolve_telegram_long_poll_timeout_seconds(Some(7)), 7);
    }

    #[test]
    fn startup_probe_extends_but_never_shrinks() {
        assert_eq!(resolve_telegram_startup_probe_timeout_ms(None), 15_000);
        assert_eq!(resolve_telegram_startup_probe_timeout_ms(Some(5)), 15_000);
        assert_eq!(resolve_telegram_startup_probe_timeout_ms(Some(60)), 60_000);
    }

    // ---- DNS decisions ----

    #[test]
    fn dns_env_overrides_config() {
        let decision = resolve_telegram_dns_result_order_decision(Some("verbatim"), |name| {
            (name == TELEGRAM_DNS_RESULT_ORDER_ENV).then(|| "ipv4first".to_string())
        });
        assert_eq!(decision.value, Some(DnsResultOrder::Ipv4First));
        assert_eq!(
            decision.source.as_deref(),
            Some("env:MYLOBSTER_TELEGRAM_DNS_RESULT_ORDER")
        );
    }

    #[test]
    fn dns_compat_env_honored() {
        let decision = resolve_telegram_dns_result_order_decision(None, |name| {
            (name == TELEGRAM_DNS_RESULT_ORDER_ENV_COMPAT).then(|| "verbatim".to_string())
        });
        assert_eq!(decision.value, Some(DnsResultOrder::Verbatim));
    }

    #[test]
    fn dns_config_used_when_no_env() {
        let decision = resolve_telegram_dns_result_order_decision(Some("IPv4First"), |_| None);
        assert_eq!(decision.value, Some(DnsResultOrder::Ipv4First));
        assert_eq!(decision.source.as_deref(), Some("config"));
    }

    #[test]
    fn dns_defaults_to_inherit_process_order() {
        let decision = resolve_telegram_dns_result_order_decision(None, |_| None);
        assert_eq!(decision.value, None);
        assert_eq!(decision.source, None);
    }

    #[test]
    fn dns_invalid_values_ignored() {
        let decision = resolve_telegram_dns_result_order_decision(Some("bogus"), |_| {
            Some("also-bogus".to_string())
        });
        assert_eq!(decision.value, None);
    }

    // ---- transport fallback logging levels ----

    #[test]
    fn ipv4_promotion_logs_debug_pinned_logs_warn() {
        let mut fallback = TelegramTransportFallback::new();
        let ipv4 = fallback.promote(TELEGRAM_TRANSPORT_STICKY_IPV4).unwrap();
        assert_eq!(ipv4.level, FallbackLogLevel::Debug);
        let pinned = fallback.promote(TELEGRAM_TRANSPORT_PINNED_IP).unwrap();
        assert_eq!(pinned.level, FallbackLogLevel::Warn);
    }

    #[test]
    fn fallback_recovery_logs_debug() {
        let mut fallback = TelegramTransportFallback::new();
        fallback.promote(TELEGRAM_TRANSPORT_STICKY_IPV4);
        let recovered = fallback.record_success(TELEGRAM_TRANSPORT_PRIMARY).unwrap();
        assert_eq!(recovered.level, FallbackLogLevel::Debug);
        assert_eq!(fallback.sticky_attempt_index(), TELEGRAM_TRANSPORT_PRIMARY);
    }

    #[test]
    fn fallback_no_duplicate_promotion() {
        let mut fallback = TelegramTransportFallback::new();
        assert!(fallback.promote(TELEGRAM_TRANSPORT_STICKY_IPV4).is_some());
        assert!(fallback.promote(TELEGRAM_TRANSPORT_STICKY_IPV4).is_none());
        assert!(fallback.promote(TELEGRAM_TRANSPORT_PRIMARY).is_none());
    }

    // ---- error classification ----

    #[test]
    fn benign_delete_errors_detected() {
        assert!(is_benign_delete_message_error(
            "Bad Request: message to delete not found"
        ));
        assert!(is_benign_delete_message_error(
            "Bad Request: message can't be deleted"
        ));
        assert!(is_benign_delete_message_error("MESSAGE_ID_INVALID"));
        assert!(is_benign_delete_message_error("MESSAGE_DELETE_FORBIDDEN"));
        assert!(!is_benign_delete_message_error("Bad Request: chat not found"));
    }

    #[test]
    fn edit_noop_errors_detected() {
        assert!(is_message_not_modified_error(
            "Bad Request: message is not modified"
        ));
        assert!(is_no_text_to_edit_error(
            "Bad Request: there is no text in the message to edit"
        ));
        assert!(is_benign_edit_message_error(
            "Bad Request: message to edit not found"
        ));
    }

    #[test]
    fn html_parse_errors_detected() {
        assert!(is_html_parse_error(
            "Bad Request: can't parse entities: Unsupported start tag"
        ));
        assert!(is_html_parse_error(
            "Bad Request: can't find end of the entity starting at byte offset 5"
        ));
        assert!(!is_html_parse_error("Bad Request: chat not found"));
    }

    #[test]
    fn structured_429_with_retry_after_401_is_not_401() {
        // Root cause of upstream #94787: "(429: Too Many Requests: retry after
        // 401)" must not trigger the 401 suspension path.
        let err = TelegramApiError {
            error_code: Some(429),
            description: "Too Many Requests: retry after 401".to_string(),
            retry_after_seconds: Some(401),
            transport: false,
            timed_out: false,
        };
        assert!(!is_401_error(&err));
        assert!(is_telegram_rate_limit_error(&err));
        assert_eq!(read_telegram_retry_after_ms(&err), Some(401_000));
    }

    #[test]
    fn unstructured_unauthorized_is_401() {
        let err = TelegramApiError::transport("request failed: Unauthorized", false);
        assert!(is_401_error(&err));
        let err2 = TelegramApiError::transport("error 401 something", false);
        assert!(!is_401_error(&err2), "bare 401 substring must not match");
    }

    // ---- backoff math ----

    #[test]
    fn backoff_progression_1s_to_5min() {
        assert_eq!(compute_backoff_ms(1), 1_000);
        assert_eq!(compute_backoff_ms(2), 2_000);
        assert_eq!(compute_backoff_ms(3), 4_000);
        assert_eq!(compute_backoff_ms(9), 256_000);
        assert_eq!(compute_backoff_ms(10), 300_000);
        assert_eq!(compute_backoff_ms(40), 300_000);
    }

    // ---- v2026.7.1 transport classifiers ----

    #[test]
    fn conflict_409_detected() {
        let err = TelegramApiError {
            error_code: Some(409),
            description: "Conflict: terminated by other getUpdates request".to_string(),
            retry_after_seconds: None,
            transport: false,
            timed_out: false,
        };
        assert!(is_get_updates_conflict_error(&err));
        let msg_only = TelegramApiError::transport(
            "Conflict: terminated by other getUpdates request; make sure that only one bot instance is running",
            false,
        );
        assert!(is_get_updates_conflict_error(&msg_only));
    }

    #[test]
    fn safe_retry_classification() {
        // 421 misdirected → safe.
        let misdirected = TelegramApiError {
            error_code: Some(421),
            description: "Misdirected Request".to_string(),
            retry_after_seconds: None,
            transport: false,
            timed_out: false,
        };
        assert!(is_safe_to_retry_send_error(&misdirected));
        // Pre-connect failure → safe.
        let refused = TelegramApiError::transport("sendMessage transport error: connection refused", false);
        assert!(is_safe_to_retry_send_error(&refused));
        let netdown = TelegramApiError::transport("ENETDOWN before connect", false);
        assert!(is_safe_to_retry_send_error(&netdown));
        // Post-connect timeout → NOT safe (could duplicate).
        let timeout = TelegramApiError::transport("operation timed out", true);
        assert!(!is_safe_to_retry_send_error(&timeout));
        // Structured Bot API error → not a transport retry candidate.
        let bad_request = TelegramApiError {
            error_code: Some(400),
            description: "Bad Request: chat not found".to_string(),
            retry_after_seconds: None,
            transport: false,
            timed_out: false,
        };
        assert!(!is_safe_to_retry_send_error(&bad_request));
    }

    #[test]
    fn thread_not_found_fails_closed() {
        assert!(is_thread_not_found_error(
            "Bad Request: message thread not found"
        ));
        assert!(!is_thread_not_found_error("Bad Request: chat not found"));
    }

    #[test]
    fn token_fingerprint_stable_and_rotation_detected() {
        let fp1 = fingerprint_telegram_bot_token("12345:AAAA");
        let fp1b = fingerprint_telegram_bot_token("12345:AAAA");
        let fp2 = fingerprint_telegram_bot_token("12345:BBBB");
        assert_eq!(fp1, fp1b);
        assert_ne!(fp1, fp2);
        assert_eq!(fp1.len(), 16);
        assert!(!should_discard_update_offset(None, &fp1));
        assert!(!should_discard_update_offset(Some(&fp1), &fp1));
        assert!(should_discard_update_offset(Some(&fp1), &fp2));
    }

    #[test]
    fn stall_threshold_clamped() {
        assert_eq!(resolve_polling_stall_threshold_ms(None), 120_000);
        assert_eq!(resolve_polling_stall_threshold_ms(Some(1_000)), 30_000);
        assert_eq!(resolve_polling_stall_threshold_ms(Some(999_999_999)), 600_000);
        assert_eq!(resolve_polling_stall_threshold_ms(Some(200_000)), 200_000);
    }

    #[test]
    fn media_group_flush_defaults_and_floor() {
        assert_eq!(resolve_media_group_flush_ms(None), 500);
        assert_eq!(resolve_media_group_flush_ms(Some(3)), 10);
        assert_eq!(resolve_media_group_flush_ms(Some(1_500)), 1_500);
    }

    #[test]
    fn polling_liveness_detects_active_stall() {
        let mut tracker = TelegramPollingLivenessTracker::new(0);
        tracker.note_started(0);
        // Within the threshold: no stall.
        assert!(tracker.detect_stall(120_000, 100_000).is_none());
        // Past the threshold with the request still in flight: stall.
        let stall = tracker.detect_stall(120_000, 130_000).unwrap();
        assert!(stall.message.contains("active getUpdates stuck"));
        // Diag suppression within half a threshold window.
        assert!(tracker.detect_stall(120_000, 140_000).is_none());
        // After the suppression window a new stall report fires.
        assert!(tracker.detect_stall(120_000, 200_000).is_some());
    }

    #[test]
    fn polling_liveness_idle_stall_and_recovery() {
        let mut tracker = TelegramPollingLivenessTracker::new(0);
        tracker.note_started(0);
        tracker.note_success(3, 1_000);
        tracker.note_finished();
        assert_eq!(tracker.in_flight(), 0);
        // Idle past the threshold from last finish → stall.
        let stall = tracker.detect_stall(120_000, 125_000).unwrap();
        assert!(stall.message.contains("no completed getUpdates"));
        // A fresh poll resets activity.
        tracker.note_started(126_000);
        tracker.note_success(0, 126_500);
        tracker.note_finished();
        assert!(tracker.detect_stall(120_000, 130_000).is_none());
    }

    // ---- sendChatAction guard ----

    fn err_401() -> TelegramApiError {
        TelegramApiError {
            error_code: Some(401),
            description: "Unauthorized".to_string(),
            retry_after_seconds: None,
            transport: false,
            timed_out: false,
        }
    }

    #[test]
    fn guard_suspends_after_max_401() {
        let mut guard = TelegramSendChatActionGuard::new(3, 0);
        for i in 0..3u64 {
            match guard.begin_attempt("1", "typing", i) {
                ChatActionDecision::Proceed { .. } => {}
                other => panic!("expected proceed, got {other:?}"),
            }
            let suspended = guard.record_failure(&err_401(), "1", "typing", i, i);
            guard.finish_attempt("1", "typing", i);
            assert_eq!(suspended, i == 2);
        }
        assert!(guard.is_suspended());
        assert_eq!(
            guard.begin_attempt("1", "typing", 100),
            ChatActionDecision::Suspended
        );
        guard.reset();
        assert!(!guard.is_suspended());
    }

    #[test]
    fn guard_backoff_applied_on_401_streak() {
        let mut guard = TelegramSendChatActionGuard::new(10, 0);
        assert_eq!(
            guard.begin_attempt("1", "typing", 0),
            ChatActionDecision::Proceed { backoff_ms: 0 }
        );
        guard.record_failure(&err_401(), "1", "typing", 0, 0);
        assert_eq!(
            guard.begin_attempt("1", "typing", 1),
            ChatActionDecision::Proceed { backoff_ms: 1_000 }
        );
        guard.record_failure(&err_401(), "1", "typing", 1, 1);
        assert_eq!(
            guard.begin_attempt("1", "typing", 2),
            ChatActionDecision::Proceed { backoff_ms: 2_000 }
        );
        guard.record_success();
        assert_eq!(
            guard.begin_attempt("1", "typing", 3),
            ChatActionDecision::Proceed { backoff_ms: 0 }
        );
    }

    #[test]
    fn guard_transient_cooldown_rejects() {
        let mut guard = TelegramSendChatActionGuard::new(10, 0);
        let rate_limited = TelegramApiError {
            error_code: Some(429),
            description: "Too Many Requests: retry after 3".to_string(),
            retry_after_seconds: Some(3),
            transport: false,
            timed_out: false,
        };
        guard.begin_attempt("1", "typing", 1_000);
        guard.record_failure(&rate_limited, "1", "typing", 1_000, 1_000);
        match guard.begin_attempt("1", "typing", 2_000) {
            ChatActionDecision::TransientCooldown { remaining_ms } => {
                assert_eq!(remaining_ms, 2_000); // until 4_000
            }
            other => panic!("expected cooldown, got {other:?}"),
        }
        // After the cooldown expires, proceeds again.
        assert_eq!(
            guard.begin_attempt("1", "typing", 4_500),
            ChatActionDecision::Proceed { backoff_ms: 0 }
        );
    }

    #[test]
    fn guard_coalesces_same_chat_action() {
        let mut guard = TelegramSendChatActionGuard::new(10, 5_000);
        assert!(matches!(
            guard.begin_attempt("1", "typing", 0),
            ChatActionDecision::Proceed { .. }
        ));
        guard.record_success();
        guard.finish_attempt("1", "typing", 0);
        assert_eq!(
            guard.begin_attempt("1", "typing", 3_000),
            ChatActionDecision::Coalesced
        );
        // Different chat is not coalesced.
        assert!(matches!(
            guard.begin_attempt("2", "typing", 3_000),
            ChatActionDecision::Proceed { .. }
        ));
        // After the interval the same chat proceeds again.
        assert!(matches!(
            guard.begin_attempt("1", "typing", 6_000),
            ChatActionDecision::Proceed { .. }
        ));
    }
}
