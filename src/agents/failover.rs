//! Failover policy helpers (OpenClaw v2026.5.2 / v2026.7.1 parity).
//!
//! Complements `agents::model_fallback` (per-model cooldown state) with:
//! - [`FailoverError`] carrying `sessionId` / `lane` / `provider` / `model` /
//!   `profileId` context through the failover engine (v2026.5.2).
//! - Message-level retryability classification, including bare
//!   `status: internal server error` provider messages (v2026.5.2).
//! - Run-phase awareness: run-level timeouts that fire **during tool
//!   execution** are exempt from model fallback and timeout-triggered
//!   compaction (v2026.5.2) — a long-running tool is not a provider failure.
//! - Cost-runaway breaker: halt after 5 consecutive idle timeouts without
//!   progress (v2026.7.1 failover engine, partial).
//! - Format-level rejections never cool down auth profiles (v2026.7.1,
//!   partial).

use crate::agents::model_fallback::FailoverReason;
use std::fmt;

// ============================================================================
// FailoverError with run context (v2026.5.2)
// ============================================================================

/// Failover error carrying the run context needed for diagnostics and lane
/// suspension decisions. All context fields are optional so partial contexts
/// still flow through.
#[derive(Debug, Clone)]
pub struct FailoverError {
    pub message: String,
    pub reason: FailoverReason,
    pub status: Option<u16>,
    pub session_id: Option<String>,
    pub lane: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub profile_id: Option<String>,
}

impl FailoverError {
    pub fn new(message: impl Into<String>, reason: FailoverReason) -> Self {
        Self {
            message: message.into(),
            reason,
            status: None,
            session_id: None,
            lane: None,
            provider: None,
            model: None,
            profile_id: None,
        }
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_lane(mut self, lane: impl Into<String>) -> Self {
        self.lane = Some(lane.into());
        self
    }

    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_profile_id(mut self, profile_id: impl Into<String>) -> Self {
        self.profile_id = Some(profile_id.into());
        self
    }
}

impl fmt::Display for FailoverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "failover: {}", self.message)?;
        if let Some(p) = &self.provider {
            write!(f, " provider={p}")?;
        }
        if let Some(m) = &self.model {
            write!(f, " model={m}")?;
        }
        if let Some(l) = &self.lane {
            write!(f, " lane={l}")?;
        }
        if let Some(s) = &self.session_id {
            write!(f, " session={s}")?;
        }
        if let Some(pr) = &self.profile_id {
            write!(f, " profile={pr}")?;
        }
        Ok(())
    }
}

impl std::error::Error for FailoverError {}

// ============================================================================
// Message-level classification (v2026.5.2)
// ============================================================================

/// Classify a provider error *message* (no HTTP status available) into a
/// failover reason.
///
/// v2026.5.2: bare `status: internal server error` messages (some gateways
/// return this string with HTTP 200) are retryable server-side failures.
pub fn classify_provider_message(message: &str) -> FailoverReason {
    let lower = message.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return FailoverReason::Unknown;
    }
    if lower.contains("rate limit") || lower.contains("too many requests") || lower.contains("429")
    {
        return FailoverReason::RateLimit;
    }
    if lower.contains("timed out") || lower.contains("timeout") {
        return FailoverReason::Timeout;
    }
    if lower.contains("unauthorized")
        || lower.contains("invalid api key")
        || lower.contains("authentication")
        || lower.contains("forbidden")
    {
        return FailoverReason::AuthError;
    }
    if lower.contains("context length")
        || lower.contains("context window")
        || lower.contains("maximum context")
    {
        return FailoverReason::ContextOverflow;
    }
    // v2026.5.2: bare "status: internal server error" (and close variants)
    // are transient server failures → retryable.
    if lower.contains("internal server error")
        || lower.contains("bad gateway")
        || lower.contains("service unavailable")
        || lower.contains("overloaded")
        || lower.contains("server_error")
    {
        return FailoverReason::MalformedResponse;
    }
    FailoverReason::Unknown
}

/// Whether a provider error message is retryable per the failover policy.
pub fn is_retryable_provider_message(message: &str) -> bool {
    crate::agents::model_fallback::is_retryable(classify_provider_message(message))
}

/// Format-level rejections (schema/`response_format`/tool-shape errors) are
/// deterministic — retrying the same profile cannot help, but the profile is
/// healthy, so it must NOT be cooled down (v2026.7.1).
pub fn is_format_level_rejection(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("response_format")
        || lower.contains("invalid schema")
        || lower.contains("invalid_request_error") && lower.contains("schema")
        || lower.contains("tool_choice")
        || lower.contains("unsupported parameter")
        || lower.contains("tuple schema")
}

/// Whether the auth profile that produced this failure should be placed on
/// cooldown. Format-level rejections and auth errors never cool a profile
/// down (auth errors disable it through a different path).
pub fn should_cooldown_profile(reason: FailoverReason, message: &str) -> bool {
    if is_format_level_rejection(message) {
        return false;
    }
    !matches!(reason, FailoverReason::AuthError)
}

// ============================================================================
// Run-phase timeout exemption (v2026.5.2)
// ============================================================================

/// Coarse phase of an agent run, used to scope timeout handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunPhase {
    /// Waiting on / consuming a provider stream.
    Streaming,
    /// Executing tool calls between provider turns.
    ToolExecution,
    /// Post-stream finalization (persist, deliver).
    Finalizing,
}

/// Run-level timeouts that fire while tools are executing must NOT trigger
/// model fallback — the provider did nothing wrong (v2026.5.2).
pub fn timeout_triggers_model_fallback(phase: RunPhase) -> bool {
    !matches!(phase, RunPhase::ToolExecution)
}

/// Run-level timeouts during tool execution are also exempt from
/// timeout-triggered compaction (v2026.5.2).
pub fn timeout_triggers_compaction(phase: RunPhase) -> bool {
    !matches!(phase, RunPhase::ToolExecution)
}

// ============================================================================
// Cost-runaway breaker (v2026.7.1, partial)
// ============================================================================

/// Consecutive idle timeouts without progress before the run is halted.
pub const IDLE_TIMEOUT_BREAKER_LIMIT: u32 = 5;

/// Tracks consecutive idle timeouts without progress. Any progress event
/// (delta, tool call, usage) resets the streak.
#[derive(Debug, Default, Clone)]
pub struct IdleTimeoutBreaker {
    consecutive: u32,
}

impl IdleTimeoutBreaker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an idle timeout. Returns `true` when the breaker trips (the
    /// run must halt instead of rotating to yet another profile/model).
    pub fn record_idle_timeout(&mut self) -> bool {
        self.consecutive = self.consecutive.saturating_add(1);
        self.tripped()
    }

    /// Record any forward progress; resets the streak.
    pub fn record_progress(&mut self) {
        self.consecutive = 0;
    }

    pub fn tripped(&self) -> bool {
        self.consecutive >= IDLE_TIMEOUT_BREAKER_LIMIT
    }

    pub fn consecutive(&self) -> u32 {
        self.consecutive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // classification table
    // ------------------------------------------------------------------

    #[test]
    fn classification_table() {
        let cases: &[(&str, FailoverReason, bool)] = &[
            // (message, reason, retryable)
            ("status: internal server error", FailoverReason::MalformedResponse, true),
            ("Internal Server Error", FailoverReason::MalformedResponse, true),
            ("upstream returned 502 Bad Gateway", FailoverReason::MalformedResponse, true),
            ("503 Service Unavailable", FailoverReason::MalformedResponse, true),
            ("model is overloaded, try again", FailoverReason::MalformedResponse, true),
            ("api_error: server_error", FailoverReason::MalformedResponse, true),
            ("rate limit exceeded", FailoverReason::RateLimit, true),
            ("HTTP 429 too many requests", FailoverReason::RateLimit, true),
            ("request timed out after 60s", FailoverReason::Timeout, true),
            ("connect timeout", FailoverReason::Timeout, true),
            ("invalid api key provided", FailoverReason::AuthError, false),
            ("401 Unauthorized", FailoverReason::AuthError, false),
            ("prompt exceeds maximum context length", FailoverReason::ContextOverflow, false),
            ("input exceeds the context window", FailoverReason::ContextOverflow, false),
            ("something inexplicable", FailoverReason::Unknown, false),
            ("", FailoverReason::Unknown, false),
        ];
        for (msg, expected_reason, expected_retryable) in cases {
            assert_eq!(
                classify_provider_message(msg),
                *expected_reason,
                "reason mismatch for {msg:?}"
            );
            assert_eq!(
                is_retryable_provider_message(msg),
                *expected_retryable,
                "retryability mismatch for {msg:?}"
            );
        }
    }

    #[test]
    fn bare_internal_server_error_is_retryable() {
        // The v2026.5.2 regression case verbatim.
        assert!(is_retryable_provider_message("status: internal server error"));
    }

    // ------------------------------------------------------------------
    // FailoverError context
    // ------------------------------------------------------------------

    #[test]
    fn failover_error_carries_full_context() {
        let err = FailoverError::new("stream failed", FailoverReason::Timeout)
            .with_status(504)
            .with_session_id("sess-1")
            .with_lane("chat")
            .with_provider("openai")
            .with_model("gpt-4.1")
            .with_profile_id("profile-a");
        assert_eq!(err.session_id.as_deref(), Some("sess-1"));
        assert_eq!(err.lane.as_deref(), Some("chat"));
        assert_eq!(err.provider.as_deref(), Some("openai"));
        assert_eq!(err.model.as_deref(), Some("gpt-4.1"));
        assert_eq!(err.profile_id.as_deref(), Some("profile-a"));
        assert_eq!(err.status, Some(504));

        let display = err.to_string();
        for needle in ["stream failed", "openai", "gpt-4.1", "chat", "sess-1", "profile-a"] {
            assert!(display.contains(needle), "display missing {needle}: {display}");
        }
    }

    #[test]
    fn failover_error_partial_context_ok() {
        let err = FailoverError::new("boom", FailoverReason::Unknown);
        assert!(err.session_id.is_none());
        assert_eq!(err.to_string(), "failover: boom");
    }

    // ------------------------------------------------------------------
    // run-phase timeout exemption
    // ------------------------------------------------------------------

    #[test]
    fn tool_execution_timeouts_exempt_from_fallback_and_compaction() {
        assert!(!timeout_triggers_model_fallback(RunPhase::ToolExecution));
        assert!(!timeout_triggers_compaction(RunPhase::ToolExecution));
    }

    #[test]
    fn streaming_and_finalizing_timeouts_trigger_normally() {
        assert!(timeout_triggers_model_fallback(RunPhase::Streaming));
        assert!(timeout_triggers_compaction(RunPhase::Streaming));
        assert!(timeout_triggers_model_fallback(RunPhase::Finalizing));
        assert!(timeout_triggers_compaction(RunPhase::Finalizing));
    }

    // ------------------------------------------------------------------
    // profile cooldown policy
    // ------------------------------------------------------------------

    #[test]
    fn format_level_rejections_never_cooldown() {
        assert!(is_format_level_rejection("invalid response_format for model"));
        assert!(is_format_level_rejection("tool_choice not supported"));
        assert!(!should_cooldown_profile(
            FailoverReason::Unknown,
            "invalid response_format for model"
        ));
    }

    #[test]
    fn auth_errors_never_cooldown() {
        assert!(!should_cooldown_profile(FailoverReason::AuthError, "401 unauthorized"));
    }

    #[test]
    fn transient_failures_do_cooldown() {
        assert!(should_cooldown_profile(FailoverReason::RateLimit, "rate limit"));
        assert!(should_cooldown_profile(
            FailoverReason::MalformedResponse,
            "status: internal server error"
        ));
    }

    // ------------------------------------------------------------------
    // cost-runaway breaker
    // ------------------------------------------------------------------

    #[test]
    fn breaker_trips_after_five_consecutive_idle_timeouts() {
        let mut b = IdleTimeoutBreaker::new();
        for i in 1..IDLE_TIMEOUT_BREAKER_LIMIT {
            assert!(!b.record_idle_timeout(), "must not trip at {i}");
        }
        assert!(b.record_idle_timeout(), "must trip at limit");
        assert!(b.tripped());
    }

    #[test]
    fn breaker_resets_on_progress() {
        let mut b = IdleTimeoutBreaker::new();
        for _ in 0..4 {
            b.record_idle_timeout();
        }
        b.record_progress();
        assert_eq!(b.consecutive(), 0);
        assert!(!b.record_idle_timeout());
        assert!(!b.tripped());
    }
}
