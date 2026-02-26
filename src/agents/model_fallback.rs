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
    /// Unclassified error.
    Unknown,
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
}
