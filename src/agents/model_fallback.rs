//! Model fallback with per-model cooldown tracking (v2026.2.25).
//!
//! When a primary model provider fails (rate limit, timeout, auth error),
//! the fallback system selects an alternative model from the configured
//! fallback chain while respecting per-model cooldown windows.
//!
//! This module provides the state types and stub resolution function.
//! Actual execution wiring into the agent pipeline is future work.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

/// Tracks per-model cooldown state for the fallback system.
#[derive(Debug, Clone)]
pub struct ModelFallbackState {
    /// Map of model identifier → cooldown expiry time.
    cooldowns: HashMap<String, SystemTime>,
    /// Default cooldown duration applied after a model failure.
    pub default_cooldown: Duration,
}

impl ModelFallbackState {
    /// Create a new fallback state with the given default cooldown.
    pub fn new(default_cooldown: Duration) -> Self {
        Self {
            cooldowns: HashMap::new(),
            default_cooldown,
        }
    }

    /// Record a failure for a model, placing it on cooldown.
    pub fn record_failure(&mut self, model: &str) {
        let expires = SystemTime::now() + self.default_cooldown;
        self.cooldowns.insert(model.to_string(), expires);
    }

    /// Record a failure with a custom cooldown duration.
    pub fn record_failure_with_cooldown(&mut self, model: &str, cooldown: Duration) {
        let expires = SystemTime::now() + cooldown;
        self.cooldowns.insert(model.to_string(), expires);
    }

    /// Check whether a model is currently on cooldown.
    pub fn is_on_cooldown(&self, model: &str) -> bool {
        if let Some(expires) = self.cooldowns.get(model) {
            SystemTime::now() < *expires
        } else {
            false
        }
    }

    /// Clear expired cooldowns (garbage collection).
    pub fn clear_expired(&mut self) {
        let now = SystemTime::now();
        self.cooldowns.retain(|_, expires| now < *expires);
    }

    /// Clear all cooldowns (e.g. on config reload).
    pub fn clear_all(&mut self) {
        self.cooldowns.clear();
    }
}

impl Default for ModelFallbackState {
    fn default() -> Self {
        Self::new(Duration::from_secs(60))
    }
}

/// A single fallback attempt record.
#[derive(Debug, Clone)]
pub struct FallbackAttempt {
    /// Provider name (e.g. "anthropic", "openai").
    pub provider: String,
    /// Model identifier that was attempted.
    pub model: String,
    /// Error message if the attempt failed.
    pub error: Option<String>,
    /// Why this model was tried as a fallback.
    pub reason: FailoverReason,
    /// HTTP status code from the provider, if available.
    pub status: Option<u16>,
}

/// Reason why a failover to the next model in the chain was triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailoverReason {
    /// Request timed out.
    Timeout,
    /// Model's context window was exceeded.
    ContextOverflow,
    /// Authentication/authorization error (invalid key, expired token).
    AuthError,
    /// Rate limit (429) from the provider.
    RateLimit,
    /// Insufficient balance (402) — Venice, Poe (v2026.3.11).
    InsufficientBalance,
    /// Malformed response from provider — retryable (v2026.3.11 Gemini).
    MalformedResponse,
    /// Client closed connection (HTTP 499) — transient (v2026.3.11).
    ClientClosed,
    /// Unclassified error.
    Unknown,
}

/// Structured lifecycle event for model fallback decisions (v2026.3.11).
#[derive(Debug, Clone)]
pub struct FallbackDecisionEvent {
    /// Unique identifier for this run.
    pub run_id: String,
    /// Model that failed.
    pub failed_model: String,
    /// Selected fallback model (if any).
    pub fallback_model: Option<String>,
    /// Reason for the failover.
    pub reason: FailoverReason,
    /// HTTP status code from the failed request.
    pub status_code: Option<u16>,
    /// Number of probe attempts this run.
    pub probe_count: u32,
    /// Whether probe cap was hit (v2026.3.11).
    pub probe_capped: bool,
}

/// Classify an HTTP status code into a FailoverReason (v2026.3.11).
pub fn classify_http_error(status: u16, body: Option<&str>) -> FailoverReason {
    match status {
        429 => FailoverReason::RateLimit,
        401 | 403 => FailoverReason::AuthError,
        402 => {
            // v2026.3.11: Detect Venice "Insufficient balance" and Poe "insufficient points"
            if let Some(b) = body {
                let lower = b.to_ascii_lowercase();
                if lower.contains("insufficient balance") || lower.contains("insufficient points") {
                    return FailoverReason::InsufficientBalance;
                }
            }
            FailoverReason::InsufficientBalance
        }
        408 | 504 => FailoverReason::Timeout,
        499 => FailoverReason::ClientClosed,
        _ => FailoverReason::Unknown,
    }
}

/// Check if a FailoverReason is retryable (v2026.3.11).
pub fn is_retryable(reason: FailoverReason) -> bool {
    matches!(
        reason,
        FailoverReason::Timeout
            | FailoverReason::RateLimit
            | FailoverReason::MalformedResponse
            | FailoverReason::ClientClosed
    )
}

/// Resolve the next available model from the fallback chain.
///
/// Iterates through `fallback_chain` and returns the first model that:
/// 1. Is not the `failed_model`.
/// 2. Is not currently on cooldown in `state`.
///
/// Returns `None` if no models are available.
pub fn resolve_next_fallback(
    fallback_chain: &[String],
    failed_model: &str,
    state: &ModelFallbackState,
) -> Option<String> {
    fallback_chain
        .iter()
        .find(|m| m.as_str() != failed_model && !state.is_on_cooldown(m))
        .cloned()
}

/// Stub for the full fallback execution loop.
///
/// In a future release, this will:
/// 1. Attempt the primary model.
/// 2. On failure, record the failure and find the next fallback.
/// 3. Retry with the fallback model.
/// 4. Return the first successful result or all attempts.
///
/// For now, this is a type signature placeholder.
pub async fn resolve_with_fallback(
    _primary_model: &str,
    _fallback_chain: &[String],
    _state: &mut ModelFallbackState,
) -> Result<(), Vec<FallbackAttempt>> {
    // Stub — actual execution wiring is future work.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_state_has_no_cooldowns() {
        let state = ModelFallbackState::default();
        assert!(!state.is_on_cooldown("claude-sonnet-4-6"));
    }

    #[test]
    fn record_failure_puts_model_on_cooldown() {
        let mut state = ModelFallbackState::new(Duration::from_secs(60));
        state.record_failure("claude-sonnet-4-6");
        assert!(state.is_on_cooldown("claude-sonnet-4-6"));
        assert!(!state.is_on_cooldown("gpt-4o"));
    }

    #[test]
    fn clear_all_removes_cooldowns() {
        let mut state = ModelFallbackState::default();
        state.record_failure("model-a");
        state.record_failure("model-b");
        state.clear_all();
        assert!(!state.is_on_cooldown("model-a"));
        assert!(!state.is_on_cooldown("model-b"));
    }

    #[test]
    fn resolve_next_skips_failed_and_cooled() {
        let mut state = ModelFallbackState::default();
        state.record_failure("model-b");

        let chain = vec![
            "model-a".to_string(),
            "model-b".to_string(),
            "model-c".to_string(),
        ];

        // model-a is the failed model, model-b is on cooldown → model-c
        let next = resolve_next_fallback(&chain, "model-a", &state);
        assert_eq!(next, Some("model-c".to_string()));
    }

    #[test]
    fn resolve_next_returns_none_when_exhausted() {
        let mut state = ModelFallbackState::default();
        state.record_failure("model-b");

        let chain = vec!["model-a".to_string(), "model-b".to_string()];

        let next = resolve_next_fallback(&chain, "model-a", &state);
        assert_eq!(next, None);
    }

    // ====================================================================
    // classify_http_error (v2026.3.11)
    // ====================================================================

    #[test]
    fn classify_429_as_rate_limit() {
        assert_eq!(classify_http_error(429, None), FailoverReason::RateLimit);
    }

    #[test]
    fn classify_401_as_auth_error() {
        assert_eq!(classify_http_error(401, None), FailoverReason::AuthError);
    }

    #[test]
    fn classify_403_as_auth_error() {
        assert_eq!(classify_http_error(403, None), FailoverReason::AuthError);
    }

    #[test]
    fn classify_402_as_insufficient_balance() {
        assert_eq!(classify_http_error(402, None), FailoverReason::InsufficientBalance);
    }

    #[test]
    fn classify_402_venice_insufficient_balance() {
        let body = r#"{"error": "Insufficient balance to process request"}"#;
        assert_eq!(
            classify_http_error(402, Some(body)),
            FailoverReason::InsufficientBalance
        );
    }

    #[test]
    fn classify_402_poe_insufficient_points() {
        let body = r#"{"detail": "insufficient points for this model"}"#;
        assert_eq!(
            classify_http_error(402, Some(body)),
            FailoverReason::InsufficientBalance
        );
    }

    #[test]
    fn classify_408_as_timeout() {
        assert_eq!(classify_http_error(408, None), FailoverReason::Timeout);
    }

    #[test]
    fn classify_504_as_timeout() {
        assert_eq!(classify_http_error(504, None), FailoverReason::Timeout);
    }

    #[test]
    fn classify_499_as_client_closed() {
        assert_eq!(classify_http_error(499, None), FailoverReason::ClientClosed);
    }

    #[test]
    fn classify_500_as_unknown() {
        assert_eq!(classify_http_error(500, None), FailoverReason::Unknown);
    }

    // ====================================================================
    // is_retryable (v2026.3.11)
    // ====================================================================

    #[test]
    fn timeout_is_retryable() {
        assert!(is_retryable(FailoverReason::Timeout));
    }

    #[test]
    fn rate_limit_is_retryable() {
        assert!(is_retryable(FailoverReason::RateLimit));
    }

    #[test]
    fn malformed_response_is_retryable() {
        assert!(is_retryable(FailoverReason::MalformedResponse));
    }

    #[test]
    fn client_closed_is_retryable() {
        assert!(is_retryable(FailoverReason::ClientClosed));
    }

    #[test]
    fn auth_error_is_not_retryable() {
        assert!(!is_retryable(FailoverReason::AuthError));
    }

    #[test]
    fn insufficient_balance_is_not_retryable() {
        assert!(!is_retryable(FailoverReason::InsufficientBalance));
    }

    #[test]
    fn context_overflow_is_not_retryable() {
        assert!(!is_retryable(FailoverReason::ContextOverflow));
    }

    #[test]
    fn unknown_is_not_retryable() {
        assert!(!is_retryable(FailoverReason::Unknown));
    }

    // ====================================================================
    // FallbackDecisionEvent (v2026.3.11)
    // ====================================================================

    #[test]
    fn fallback_decision_event_construction() {
        let event = FallbackDecisionEvent {
            run_id: "run-123".to_string(),
            failed_model: "claude-sonnet-4-6".to_string(),
            fallback_model: Some("gpt-4o".to_string()),
            reason: FailoverReason::RateLimit,
            status_code: Some(429),
            probe_count: 1,
            probe_capped: false,
        };
        assert_eq!(event.run_id, "run-123");
        assert_eq!(event.fallback_model.as_deref(), Some("gpt-4o"));
        assert_eq!(event.reason, FailoverReason::RateLimit);
        assert!(!event.probe_capped);
    }

    #[test]
    fn fallback_decision_event_no_fallback_available() {
        let event = FallbackDecisionEvent {
            run_id: "run-456".to_string(),
            failed_model: "model-a".to_string(),
            fallback_model: None,
            reason: FailoverReason::AuthError,
            status_code: Some(401),
            probe_count: 3,
            probe_capped: true,
        };
        assert!(event.fallback_model.is_none());
        assert!(event.probe_capped);
    }

    // ====================================================================
    // record_failure_with_cooldown (v2026.3.11)
    // ====================================================================

    #[test]
    fn record_failure_with_custom_cooldown() {
        let mut state = ModelFallbackState::new(Duration::from_secs(60));
        state.record_failure_with_cooldown("model-a", Duration::from_secs(300));
        assert!(state.is_on_cooldown("model-a"));
    }

    #[test]
    fn clear_expired_removes_old_cooldowns() {
        let mut state = ModelFallbackState::new(Duration::from_secs(0));
        // Use zero-duration cooldown so it expires immediately
        state.record_failure_with_cooldown("model-a", Duration::from_secs(0));
        std::thread::sleep(Duration::from_millis(10));
        state.clear_expired();
        assert!(!state.is_on_cooldown("model-a"));
    }
}
