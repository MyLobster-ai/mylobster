//! Outbound delivery types and chat-type classification (v2026.2.26).
//!
//! Types and helpers for the delivery pipeline: queue recovery,
//! session context, drain reliability, and stale message detection.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A channel that can receive delivered messages.
///
/// In OpenClaw this is `DeliverableMessageChannel` — one of the known
/// platform channel identifiers.
pub type DeliverableMessageChannel = String;

/// An outbound channel target: either a deliverable channel name or `"none"`.
pub type OutboundChannel = String;

/// Identifies a specific delivery target for outbound messages.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OutboundTarget {
    /// Channel identifier (e.g. "telegram", "discord", "slack", "none").
    pub channel: String,
    /// Recipient address within the channel (chat ID, user ID, etc.).
    pub to: Option<String>,
    /// Why this target was selected (e.g. "heartbeat", "reply", "explicit").
    pub reason: Option<String>,
    /// Account ID within the channel.
    pub account_id: Option<String>,
    /// Thread/topic ID for threaded channels.
    pub thread_id: Option<String>,
}

/// A resolved delivery target for a session, including routing metadata.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionDeliveryTarget {
    /// Channel identifier.
    pub channel: String,
    /// Recipient address.
    pub to: Option<String>,
    /// Account ID within the channel.
    pub account_id: Option<String>,
    /// Thread/topic ID.
    pub thread_id: Option<String>,
    /// Whether the thread ID was explicitly set (vs inferred).
    pub thread_id_explicit: Option<bool>,
    /// Delivery mode hint.
    pub mode: Option<String>,
}

/// Classification of a chat interaction type for delivery routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatType {
    /// One-on-one direct message.
    Direct,
    /// Public or semi-public channel message.
    Channel,
    /// Group chat (multiple participants, but not a public channel).
    Group,
}

/// Resolve the delivery target for a heartbeat based on `DirectPolicy`.
///
/// - `DirectPolicy::Last` → deliver to `last_channel` if available.
/// - `DirectPolicy::None` → no direct delivery; returns `None`.
/// - If no policy is set (default), falls back to `Last` behaviour.
pub fn resolve_heartbeat_delivery_target(
    policy: Option<crate::config::DirectPolicy>,
    last_channel: Option<&str>,
) -> Option<String> {
    match policy.unwrap_or(crate::config::DirectPolicy::Last) {
        crate::config::DirectPolicy::Last => last_channel.map(|s| s.to_string()),
        crate::config::DirectPolicy::None => None,
    }
}

/// Resolve the chat type for heartbeat delivery based on channel-specific
/// target parsing.
///
/// This is a stub — full implementation requires per-channel target format
/// knowledge (e.g. Telegram chat IDs are negative for groups, Discord has
/// guild channels vs DMs, etc.).
///
/// When a `DirectPolicy` is set to `None`, this function returns
/// `ChatType::Channel` regardless of the channel, since DM delivery is
/// suppressed.
pub fn resolve_heartbeat_delivery_chat_type(
    channel: &str,
    _to: Option<&str>,
    direct_policy: Option<crate::config::DirectPolicy>,
) -> ChatType {
    // If direct policy is None, heartbeats are never DMs.
    if direct_policy == Some(crate::config::DirectPolicy::None) {
        return ChatType::Channel;
    }

    // Default heuristic: heartbeats are typically direct messages.
    // Per-channel refinement would go here.
    match channel {
        "discord" | "slack" | "msteams" | "irc" => ChatType::Channel,
        _ => ChatType::Direct,
    }
}

// ============================================================================
// Session Context (v2026.2.26)
// ============================================================================

/// Unified session context replacing separate key/agent_id parameters.
///
/// Used by delivery and session management to avoid parameter duplication.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionContext {
    /// Session key.
    pub key: String,
    /// Agent ID bound to this session.
    pub agent_id: Option<String>,
    /// Account ID for the session.
    pub account_id: Option<String>,
}

impl SessionContext {
    /// Create a new session context.
    pub fn new(key: impl Into<String>, agent_id: Option<String>) -> Self {
        Self {
            key: key.into(),
            agent_id,
            account_id: None,
        }
    }

    /// Create with all fields.
    pub fn with_account(
        key: impl Into<String>,
        agent_id: Option<String>,
        account_id: Option<String>,
    ) -> Self {
        Self {
            key: key.into(),
            agent_id,
            account_id,
        }
    }
}

// ============================================================================
// Delivery Queue Recovery (v2026.2.26)
// ============================================================================

/// Tracks last attempt timestamp and failure count for delivery recovery.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryAttemptInfo {
    /// Timestamp of the last delivery attempt (ms since epoch).
    pub last_attempt_at: Option<u64>,
    /// Number of consecutive failures.
    pub consecutive_failures: u32,
    /// Whether this target is temporarily suspended.
    pub suspended: bool,
}

/// Summary of a delivery recovery cycle.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryRecoverySummary {
    /// Total targets processed.
    pub total_targets: usize,
    /// Targets that succeeded.
    pub succeeded: usize,
    /// Targets that failed.
    pub failed: usize,
    /// Targets skipped (stale or suspended).
    pub skipped: usize,
}

/// Maximum age for a queued message before it's considered stale (5 min).
const STALE_MESSAGE_THRESHOLD_MS: u64 = 5 * 60 * 1000;

/// Check if a queued message is stale and should be skipped.
pub fn is_stale_message(queued_at_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(queued_at_ms) > STALE_MESSAGE_THRESHOLD_MS
}

/// Process delivery queue with head-of-line blocking fix.
///
/// Unlike the pre-v2026.2.26 implementation that used `break` on failure
/// (blocking all subsequent targets), this uses `continue` to skip failed
/// targets and attempt delivery to remaining targets.
pub fn process_delivery_queue(
    targets: &[OutboundTarget],
    attempt_info: &mut Vec<DeliveryAttemptInfo>,
    now_ms: u64,
) -> DeliveryRecoverySummary {
    let mut summary = DeliveryRecoverySummary {
        total_targets: targets.len(),
        ..Default::default()
    };

    // Ensure attempt_info is the same length as targets.
    attempt_info.resize_with(targets.len(), Default::default);

    for (i, target) in targets.iter().enumerate() {
        let info = &mut attempt_info[i];

        // Skip suspended targets.
        if info.suspended {
            summary.skipped += 1;
            continue; // v2026.2.26: continue instead of break
        }

        // Skip stale messages.
        if let Some(last) = info.last_attempt_at {
            if is_stale_message(last, now_ms) {
                summary.skipped += 1;
                continue; // v2026.2.26: continue instead of break
            }
        }

        // Record attempt timestamp.
        info.last_attempt_at = Some(now_ms);

        // Check if target is valid (non-empty channel, not "none").
        if target.channel.is_empty() || target.channel == "none" {
            info.consecutive_failures += 1;
            summary.failed += 1;
            continue; // v2026.2.26: continue instead of break
        }

        // Delivery would happen here in full implementation.
        // For now, mark as succeeded.
        info.consecutive_failures = 0;
        summary.succeeded += 1;
    }

    summary
}

// ============================================================================
// Drain Guard (v2026.2.26)
// ============================================================================

/// RAII guard that guarantees the drain flag is reset when dropped.
///
/// Prevents enqueues during restart and ensures cleanup even on panic.
pub struct DrainGuard {
    flag: Arc<AtomicBool>,
}

impl DrainGuard {
    /// Start a drain cycle. Sets the flag to `true`.
    pub fn start(flag: Arc<AtomicBool>) -> Self {
        flag.store(true, Ordering::SeqCst);
        Self { flag }
    }

    /// Check if draining is active.
    pub fn is_draining(flag: &AtomicBool) -> bool {
        flag.load(Ordering::SeqCst)
    }
}

impl Drop for DrainGuard {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

// ============================================================================
// Telegram sendChatAction 401 Backoff (v2026.2.26)
// ============================================================================

/// Per-account exponential backoff state for Telegram 401 errors.
///
/// Prevents bot deletion by backing off when Telegram returns 401
/// on `sendChatAction` requests.
#[derive(Debug, Clone)]
pub struct TelegramBackoff {
    /// Current backoff duration in milliseconds.
    pub backoff_ms: u64,
    /// Number of consecutive 401 errors.
    pub consecutive_401s: u32,
    /// Timestamp when backoff expires (ms since epoch).
    pub backoff_until_ms: u64,
    /// Whether the circuit is open (stop all requests).
    pub circuit_open: bool,
}

impl Default for TelegramBackoff {
    fn default() -> Self {
        Self {
            backoff_ms: 1000,
            consecutive_401s: 0,
            backoff_until_ms: 0,
            circuit_open: false,
        }
    }
}

/// Initial backoff duration: 1 second.
const TELEGRAM_INITIAL_BACKOFF_MS: u64 = 1000;
/// Maximum backoff duration: 5 minutes.
const TELEGRAM_MAX_BACKOFF_MS: u64 = 5 * 60 * 1000;
/// Circuit breaker threshold: 10 consecutive 401s.
const TELEGRAM_CIRCUIT_BREAKER_THRESHOLD: u32 = 10;

impl TelegramBackoff {
    /// Record a 401 error and update backoff state.
    pub fn record_401(&mut self, now_ms: u64) {
        self.consecutive_401s += 1;

        // Exponential backoff: 1s → 2s → 4s → … → 5min cap
        self.backoff_ms = (TELEGRAM_INITIAL_BACKOFF_MS << self.consecutive_401s.min(20))
            .min(TELEGRAM_MAX_BACKOFF_MS);
        self.backoff_until_ms = now_ms + self.backoff_ms;

        // Circuit breaker after threshold consecutive 401s.
        if self.consecutive_401s >= TELEGRAM_CIRCUIT_BREAKER_THRESHOLD {
            self.circuit_open = true;
        }
    }

    /// Record a successful request — reset backoff.
    pub fn record_success(&mut self) {
        self.consecutive_401s = 0;
        self.backoff_ms = TELEGRAM_INITIAL_BACKOFF_MS;
        self.backoff_until_ms = 0;
        self.circuit_open = false;
    }

    /// Check if we should skip a request due to backoff or circuit breaker.
    pub fn should_skip(&self, now_ms: u64) -> bool {
        self.circuit_open || now_ms < self.backoff_until_ms
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_context_new() {
        let ctx = SessionContext::new("session:1", Some("agent-1".into()));
        assert_eq!(ctx.key, "session:1");
        assert_eq!(ctx.agent_id.as_deref(), Some("agent-1"));
        assert!(ctx.account_id.is_none());
    }

    #[test]
    fn session_context_with_account() {
        let ctx = SessionContext::with_account(
            "s:2",
            Some("a1".into()),
            Some("acct-1".into()),
        );
        assert_eq!(ctx.account_id.as_deref(), Some("acct-1"));
    }

    #[test]
    fn stale_message_detection() {
        let now = 1_000_000;
        // 6 min old — stale
        assert!(is_stale_message(now - 360_000, now));
        // 1 min old — fresh
        assert!(!is_stale_message(now - 60_000, now));
        // Same time — fresh
        assert!(!is_stale_message(now, now));
    }

    #[test]
    fn delivery_queue_continues_past_failures() {
        let targets = vec![
            OutboundTarget {
                channel: "none".into(), // will fail
                ..Default::default()
            },
            OutboundTarget {
                channel: "telegram".into(), // should succeed
                to: Some("123".into()),
                ..Default::default()
            },
        ];
        let mut info = vec![];
        let now = 1_000_000;

        let summary = process_delivery_queue(&targets, &mut info, now);
        assert_eq!(summary.total_targets, 2);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.succeeded, 1);
        // Key assertion: second target was attempted (no head-of-line blocking)
    }

    #[test]
    fn drain_guard_resets_flag() {
        let flag = Arc::new(AtomicBool::new(false));
        {
            let _guard = DrainGuard::start(flag.clone());
            assert!(DrainGuard::is_draining(&flag));
        }
        // Flag must be false after guard is dropped.
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[test]
    fn telegram_backoff_exponential() {
        let mut backoff = TelegramBackoff::default();
        let now = 1_000_000u64;

        // First 401: 2s backoff
        backoff.record_401(now);
        assert_eq!(backoff.consecutive_401s, 1);
        assert_eq!(backoff.backoff_ms, 2000);
        assert!(!backoff.circuit_open);

        // Second 401: 4s backoff
        backoff.record_401(now + 2000);
        assert_eq!(backoff.consecutive_401s, 2);
        assert_eq!(backoff.backoff_ms, 4000);

        // Success resets
        backoff.record_success();
        assert_eq!(backoff.consecutive_401s, 0);
        assert!(!backoff.circuit_open);
    }

    #[test]
    fn telegram_circuit_breaker() {
        let mut backoff = TelegramBackoff::default();
        let now = 1_000_000u64;

        for i in 0..10 {
            backoff.record_401(now + i * 1000);
        }

        assert!(backoff.circuit_open);
        assert!(backoff.should_skip(now + 100_000));

        // Recovery after success
        backoff.record_success();
        assert!(!backoff.circuit_open);
        assert!(!backoff.should_skip(now + 200_000));
    }

    #[test]
    fn heartbeat_target_none_policy() {
        let result = resolve_heartbeat_delivery_target(
            Some(crate::config::DirectPolicy::None),
            Some("telegram"),
        );
        assert!(result.is_none());
    }

    #[test]
    fn heartbeat_target_last_policy() {
        let result = resolve_heartbeat_delivery_target(
            Some(crate::config::DirectPolicy::Last),
            Some("telegram"),
        );
        assert_eq!(result.as_deref(), Some("telegram"));
    }
}
