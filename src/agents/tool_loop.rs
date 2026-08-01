//! Tool-loop circuit breaker (OpenClaw v2026.5.2 / v2026.7.1 parity).
//!
//! Detects degenerate tool loops (the model calling the same tool with the
//! same arguments over and over) and stops them. The v2026.5.2 behavior
//! change: critical circuit-breaker stops are surfaced to the model as
//! **blocked tool results** — a normal `role:"tool"` message with an error
//! payload — rather than thrown run failures, so the model gets a chance to
//! recover and the run ends cleanly instead of erroring out.
//!
//! Also carries the post-compaction loop guard (v2026.6.x): right after a
//! compaction the effective repeat window tightens, because summarized
//! context makes models likelier to re-issue previously-answered calls.

use crate::agents::tools::ToolResult;

/// Consecutive identical calls tolerated before the breaker blocks the call.
pub const DEFAULT_MAX_IDENTICAL_REPEATS: usize = 3;

/// Blocked calls tolerated after the breaker first trips before the run is
/// critically stopped.
pub const DEFAULT_MAX_BLOCKED_BEFORE_CRITICAL: usize = 2;

/// Post-compaction guard window (v2026.6.x
/// `tools.loopDetection.postCompactionGuard.windowSize` default).
pub const DEFAULT_POST_COMPACTION_WINDOW: usize = 2;

/// Decision for a proposed tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolLoopDecision {
    /// Execute the tool normally.
    Proceed,
    /// Do not execute; feed the model a blocked tool result instead.
    Blocked { reason: String },
    /// The model kept repeating after being blocked — end the run (still
    /// delivered as a blocked tool result plus a final, never a thrown
    /// failure).
    CriticalStop { reason: String },
}

/// Tracks consecutive identical tool calls within a run.
#[derive(Debug)]
pub struct ToolLoopGuard {
    max_repeats: usize,
    max_blocked: usize,
    last_signature: Option<String>,
    consecutive: usize,
    blocked_count: usize,
}

impl ToolLoopGuard {
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_MAX_IDENTICAL_REPEATS, DEFAULT_MAX_BLOCKED_BEFORE_CRITICAL)
    }

    pub fn with_limits(max_repeats: usize, max_blocked: usize) -> Self {
        Self {
            max_repeats: max_repeats.max(1),
            max_blocked: max_blocked.max(1),
            last_signature: None,
            consecutive: 0,
            blocked_count: 0,
        }
    }

    /// Tighten the repeat window right after a compaction (v2026.6.x
    /// post-compaction guard).
    pub fn apply_post_compaction_guard(&mut self, window_size: Option<usize>) {
        let window = window_size.unwrap_or(DEFAULT_POST_COMPACTION_WINDOW).max(1);
        self.max_repeats = self.max_repeats.min(window);
    }

    fn signature(tool_name: &str, input: &serde_json::Value) -> String {
        // Canonical: serde_json serializes maps in key order for Value
        // (preserve_order is not enabled), so equal inputs share a signature.
        format!("{tool_name}:{}", serde_json::to_string(input).unwrap_or_default())
    }

    /// Check a proposed tool call. Identical consecutive calls beyond the
    /// window are blocked; continued repeats after blocking become a
    /// critical stop. Any different call resets the streak.
    pub fn check(&mut self, tool_name: &str, input: &serde_json::Value) -> ToolLoopDecision {
        let sig = Self::signature(tool_name, input);
        if self.last_signature.as_deref() == Some(sig.as_str()) {
            self.consecutive += 1;
        } else {
            self.last_signature = Some(sig);
            self.consecutive = 1;
            self.blocked_count = 0;
        }

        if self.consecutive <= self.max_repeats {
            return ToolLoopDecision::Proceed;
        }

        self.blocked_count += 1;
        let reason = format!(
            "Tool loop detected: `{tool_name}` was called {} times in a row with identical \
             arguments. The call was blocked — change your approach instead of retrying the \
             same call.",
            self.consecutive
        );
        if self.blocked_count > self.max_blocked {
            ToolLoopDecision::CriticalStop { reason }
        } else {
            ToolLoopDecision::Blocked { reason }
        }
    }
}

impl Default for ToolLoopGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the blocked tool result fed back to the model in place of the tool
/// execution (v2026.5.2: blocked result, not a thrown failure).
pub fn blocked_tool_result(reason: &str) -> ToolResult {
    ToolResult {
        text: Some(reason.to_string()),
        json: Some(serde_json::json!({ "blocked": true, "reason": reason })),
        image: None,
        is_error: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(v: u64) -> serde_json::Value {
        serde_json::json!({ "q": v })
    }

    #[test]
    fn distinct_calls_always_proceed() {
        let mut g = ToolLoopGuard::new();
        for i in 0..20 {
            assert_eq!(g.check("web_search", &input(i)), ToolLoopDecision::Proceed);
        }
    }

    #[test]
    fn identical_calls_proceed_within_window_then_block() {
        let mut g = ToolLoopGuard::new();
        for _ in 0..DEFAULT_MAX_IDENTICAL_REPEATS {
            assert_eq!(g.check("web_search", &input(1)), ToolLoopDecision::Proceed);
        }
        match g.check("web_search", &input(1)) {
            ToolLoopDecision::Blocked { reason } => {
                assert!(reason.contains("web_search"));
                assert!(reason.contains("blocked"));
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn continued_repeats_after_block_become_critical_stop() {
        let mut g = ToolLoopGuard::new();
        // 3 proceeds, then blocks.
        for _ in 0..DEFAULT_MAX_IDENTICAL_REPEATS {
            g.check("t", &input(1));
        }
        let mut saw_blocked = 0;
        loop {
            match g.check("t", &input(1)) {
                ToolLoopDecision::Blocked { .. } => saw_blocked += 1,
                ToolLoopDecision::CriticalStop { .. } => break,
                ToolLoopDecision::Proceed => panic!("must not proceed while repeating"),
            }
            assert!(saw_blocked <= 10, "critical stop never reached");
        }
        assert_eq!(saw_blocked, DEFAULT_MAX_BLOCKED_BEFORE_CRITICAL);
    }

    #[test]
    fn different_call_resets_streak_and_block_count() {
        let mut g = ToolLoopGuard::new();
        for _ in 0..4 {
            g.check("t", &input(1)); // 4th is Blocked
        }
        assert_eq!(g.check("t", &input(2)), ToolLoopDecision::Proceed);
        // Streak restarted — window applies fresh.
        for _ in 0..DEFAULT_MAX_IDENTICAL_REPEATS - 1 {
            assert_eq!(g.check("t", &input(2)), ToolLoopDecision::Proceed);
        }
        assert!(matches!(g.check("t", &input(2)), ToolLoopDecision::Blocked { .. }));
    }

    #[test]
    fn same_tool_different_args_not_a_loop() {
        let mut g = ToolLoopGuard::new();
        for i in 0..10 {
            assert_eq!(g.check("t", &input(i % 2)), ToolLoopDecision::Proceed);
        }
    }

    #[test]
    fn different_tool_same_args_not_a_loop() {
        let mut g = ToolLoopGuard::new();
        for _ in 0..3 {
            assert_eq!(g.check("a", &input(1)), ToolLoopDecision::Proceed);
            assert_eq!(g.check("b", &input(1)), ToolLoopDecision::Proceed);
        }
    }

    #[test]
    fn post_compaction_guard_tightens_window() {
        let mut g = ToolLoopGuard::new();
        g.apply_post_compaction_guard(None); // default window 2
        assert_eq!(g.check("t", &input(1)), ToolLoopDecision::Proceed);
        assert_eq!(g.check("t", &input(1)), ToolLoopDecision::Proceed);
        assert!(matches!(g.check("t", &input(1)), ToolLoopDecision::Blocked { .. }));
    }

    #[test]
    fn post_compaction_guard_never_loosens() {
        let mut g = ToolLoopGuard::with_limits(2, 2);
        g.apply_post_compaction_guard(Some(10));
        // Still the tighter of the two (2).
        g.check("t", &input(1));
        g.check("t", &input(1));
        assert!(matches!(g.check("t", &input(1)), ToolLoopDecision::Blocked { .. }));
    }

    #[test]
    fn blocked_tool_result_is_error_not_panic() {
        let r = blocked_tool_result("loop detected");
        assert!(r.is_error);
        assert_eq!(r.json.as_ref().unwrap()["blocked"], true);
        assert!(r.text.as_deref().unwrap().contains("loop detected"));
    }
}
