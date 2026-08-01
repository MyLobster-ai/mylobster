//! Gateway diagnostics helpers (v2026.5.2 / v2026.7.1 parity).
//!
//! - Bounded, redacted startup error messages for stability bundles.
//! - Idle liveness samples routed to telemetry counters instead of visible
//!   warning logs.
//! - Bounded async diagnostic queue with drop summaries (v2026.7.1).
//! - Stuck-session abort threshold resolution (`diagnostics.stuckSessionAbortMs`).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

// ============================================================================
// Bounded redacted startup errors (v2026.5.2)
// ============================================================================

/// Maximum length of a single startup error message in a stability bundle.
pub const STABILITY_ERROR_MAX_CHARS: usize = 500;

/// Maximum number of startup errors retained in a stability bundle.
pub const STABILITY_ERROR_MAX_COUNT: usize = 8;

/// Redact obviously secret-bearing substrings from an error message.
///
/// Covers `sk-`-style API keys, bearer tokens, `key=`/`token=`/`password=`
/// query or config fragments, and long hex/base64 runs.
pub fn redact_error_message(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    for token in msg.split_whitespace() {
        let redacted = redact_token(token);
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&redacted);
    }
    out
}

fn redact_token(token: &str) -> String {
    let lower = token.to_ascii_lowercase();
    // key=value style secrets
    for prefix in ["key=", "token=", "password=", "secret=", "apikey=", "api_key="] {
        if let Some(pos) = lower.find(prefix) {
            let end = pos + prefix.len();
            return format!("{}[REDACTED]", &token[..end]);
        }
    }
    // API-key style prefixes
    for prefix in ["sk-", "sk_", "xoxb-", "xoxp-", "ghp_", "Bearer"] {
        if token.starts_with(prefix) && token.len() > prefix.len() + 4 {
            return "[REDACTED]".to_string();
        }
    }
    // long unbroken hex / base64-ish runs (potential keys)
    if token.len() >= 32
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=' || c == '-' || c == '_')
    {
        return "[REDACTED]".to_string();
    }
    token.to_string()
}

/// Bound a redacted error message to `max_chars` characters (char-safe).
pub fn bounded_redacted_error(msg: &str, max_chars: usize) -> String {
    let redacted = redact_error_message(msg);
    if redacted.chars().count() <= max_chars {
        return redacted;
    }
    let truncated: String = redacted.chars().take(max_chars).collect();
    format!("{truncated}… [truncated]")
}

/// Collects startup errors for inclusion in stability bundles, bounded in
/// both count and per-message size, with all messages redacted.
#[derive(Default)]
pub struct StartupErrorLog {
    errors: parking_lot::Mutex<Vec<String>>,
    dropped: AtomicU64,
}

impl StartupErrorLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, msg: &str) {
        let mut errors = self.errors.lock();
        if errors.len() >= STABILITY_ERROR_MAX_COUNT {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        errors.push(bounded_redacted_error(msg, STABILITY_ERROR_MAX_CHARS));
    }

    /// Snapshot for the stability bundle.
    pub fn stability_bundle(&self) -> serde_json::Value {
        serde_json::json!({
            "startupErrors": self.errors.lock().clone(),
            "droppedErrorCount": self.dropped.load(Ordering::Relaxed),
        })
    }
}

// ============================================================================
// Idle liveness telemetry (v2026.5.2)
// ============================================================================

/// Idle liveness samples are recorded as telemetry counters — never emitted
/// as visible warning logs (v2026.5.2: "Idle liveness samples → telemetry
/// not visible warning logs").
#[derive(Default)]
pub struct IdleLivenessTelemetry {
    samples: AtomicU64,
    total_lag_ms: AtomicU64,
    max_lag_ms: AtomicU64,
}

impl IdleLivenessTelemetry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one liveness sample. This intentionally performs no logging.
    pub fn record_sample(&self, lag_ms: u64) {
        self.samples.fetch_add(1, Ordering::Relaxed);
        self.total_lag_ms.fetch_add(lag_ms, Ordering::Relaxed);
        self.max_lag_ms.fetch_max(lag_ms, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> serde_json::Value {
        let samples = self.samples.load(Ordering::Relaxed);
        let total = self.total_lag_ms.load(Ordering::Relaxed);
        serde_json::json!({
            "samples": samples,
            "maxLagMs": self.max_lag_ms.load(Ordering::Relaxed),
            "avgLagMs": if samples > 0 { total / samples } else { 0 },
        })
    }
}

// ============================================================================
// Bounded async diagnostic queue with drop summaries (v2026.7.1)
// ============================================================================

pub struct BoundedDiagnosticQueue<T> {
    cap: usize,
    items: parking_lot::Mutex<VecDeque<T>>,
    dropped: AtomicU64,
}

impl<T> BoundedDiagnosticQueue<T> {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            items: parking_lot::Mutex::new(VecDeque::new()),
            dropped: AtomicU64::new(0),
        }
    }

    /// Push an item; returns false (and counts a drop) when the queue is full.
    pub fn push(&self, item: T) -> bool {
        let mut items = self.items.lock();
        if items.len() >= self.cap {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        items.push_back(item);
        true
    }

    pub fn drain(&self) -> Vec<T> {
        self.items.lock().drain(..).collect()
    }

    pub fn len(&self) -> usize {
        self.items.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Drop summary for periodic reporting; resets the drop counter.
    pub fn take_drop_summary(&self) -> Option<String> {
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        if dropped == 0 {
            None
        } else {
            Some(format!("dropped {dropped} diagnostic event(s) (queue full)"))
        }
    }
}

// ============================================================================
// Stuck-session abort threshold (v2026.7.1: diagnostics.stuckSessionAbortMs)
// ============================================================================

/// Default stuck-session abort threshold: 10 minutes.
pub const DEFAULT_STUCK_SESSION_ABORT_MS: u64 = 10 * 60 * 1000;

/// Resolve the configured stuck-session abort threshold, clamped to a sane
/// floor of 10 seconds (0/absurdly small values would abort healthy runs).
pub fn resolve_stuck_session_abort_ms(configured: Option<u64>) -> u64 {
    match configured {
        Some(0) | None => DEFAULT_STUCK_SESSION_ABORT_MS,
        Some(ms) => ms.max(10_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- redaction ----

    #[test]
    fn redacts_api_key_prefixes() {
        let msg = "provider auth failed: sk-abc123def456ghi789 rejected";
        let out = redact_error_message(msg);
        assert!(!out.contains("sk-abc123"));
        assert!(out.contains("[REDACTED]"));
        assert!(out.contains("provider auth failed:"));
    }

    #[test]
    fn redacts_key_value_fragments() {
        let out = redact_error_message("request failed: url?token=abcd1234 status=401");
        assert!(out.contains("token=[REDACTED]"));
        assert!(out.contains("status=401"));
    }

    #[test]
    fn redacts_long_opaque_runs() {
        let secret = "A".repeat(40);
        let out = redact_error_message(&format!("bad value {secret} here"));
        assert!(!out.contains(&secret));
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn keeps_ordinary_text() {
        let msg = "EADDRINUSE: port 18789 already in use";
        assert_eq!(redact_error_message(msg), msg);
    }

    #[test]
    fn bounded_error_truncates_char_safe() {
        let msg = "é".repeat(600);
        let out = bounded_redacted_error(&msg, 100);
        assert!(out.ends_with("… [truncated]"));
        assert!(out.chars().count() < 130);
    }

    #[test]
    fn bounded_error_short_passthrough() {
        assert_eq!(bounded_redacted_error("short err", 100), "short err");
    }

    // ---- startup error log ----

    #[test]
    fn startup_error_log_bounds_count_and_size() {
        let log = StartupErrorLog::new();
        for i in 0..20 {
            log.record(&format!("error {i} {}", "x".repeat(1000)));
        }
        let bundle = log.stability_bundle();
        let errors = bundle["startupErrors"].as_array().unwrap();
        assert_eq!(errors.len(), STABILITY_ERROR_MAX_COUNT);
        for e in errors {
            assert!(e.as_str().unwrap().chars().count() <= STABILITY_ERROR_MAX_CHARS + 20);
        }
        assert_eq!(bundle["droppedErrorCount"], 20 - STABILITY_ERROR_MAX_COUNT as u64);
    }

    // ---- idle liveness ----

    #[test]
    fn idle_liveness_counters() {
        let t = IdleLivenessTelemetry::new();
        t.record_sample(10);
        t.record_sample(30);
        t.record_sample(20);
        let snap = t.snapshot();
        assert_eq!(snap["samples"], 3);
        assert_eq!(snap["maxLagMs"], 30);
        assert_eq!(snap["avgLagMs"], 20);
    }

    #[test]
    fn idle_liveness_empty_snapshot() {
        let t = IdleLivenessTelemetry::new();
        let snap = t.snapshot();
        assert_eq!(snap["samples"], 0);
        assert_eq!(snap["avgLagMs"], 0);
    }

    // ---- bounded queue ----

    #[test]
    fn bounded_queue_caps_and_summarizes_drops() {
        let q = BoundedDiagnosticQueue::new(2);
        assert!(q.push(1));
        assert!(q.push(2));
        assert!(!q.push(3));
        assert!(!q.push(4));
        assert_eq!(q.len(), 2);
        assert_eq!(q.dropped_count(), 2);
        let summary = q.take_drop_summary().unwrap();
        assert!(summary.contains("dropped 2"));
        assert!(q.take_drop_summary().is_none());
        assert_eq!(q.drain(), vec![1, 2]);
        assert!(q.is_empty());
    }

    // ---- stuck session ----

    #[test]
    fn stuck_session_threshold_defaults_and_clamps() {
        assert_eq!(resolve_stuck_session_abort_ms(None), DEFAULT_STUCK_SESSION_ABORT_MS);
        assert_eq!(resolve_stuck_session_abort_ms(Some(0)), DEFAULT_STUCK_SESSION_ABORT_MS);
        assert_eq!(resolve_stuck_session_abort_ms(Some(5)), 10_000);
        assert_eq!(resolve_stuck_session_abort_ms(Some(120_000)), 120_000);
    }
}
