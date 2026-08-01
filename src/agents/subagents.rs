//! Subagent runtime policy (OpenClaw v2026.4.29 / v2026.5.2 / v2026.7.1).
//!
//! Runtime-side subagent behaviors (the spawn *tool* lives in
//! `agents/tools/subagents.rs`; this module owns the policy the runtime
//! applies around spawns and completions):
//! - `expectsCompletionMessage: false` spawns skip the parent completion
//!   handoff entirely (v2026.5.2).
//! - `sessions_send` to the agent's **own persistent subagent session**
//!   must not produce a duplicate parent-visible reply — the tool result
//!   already carries the child's answer (v2026.5.2).
//! - Spawned-subagent routing metadata: `spawnedBy` attached to chat /
//!   broadcast payloads so clients can route child output (v2026.4.29).
//! - Completion outcome classification: blocked / progress-only completions
//!   are errors, not successes (v2026.7.1, partial).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;

// ============================================================================
// Spawn options / completion handoff (v2026.5.2)
// ============================================================================

/// Options controlling a subagent spawn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnOptions {
    /// When `false`, the parent gets no completion announcement for this
    /// child (fire-and-forget). Default `true`.
    #[serde(default = "default_true")]
    pub expects_completion_message: bool,
    /// Routing metadata identifying the spawner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawned_by: Option<SpawnedBy>,
}

fn default_true() -> bool {
    true
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            expects_completion_message: true,
            spawned_by: None,
        }
    }
}

/// Whether the runtime should announce this child's completion to the
/// parent session (v2026.5.2: honors `expectsCompletionMessage: false`).
pub fn should_announce_completion(opts: &SpawnOptions) -> bool {
    opts.expects_completion_message
}

// ============================================================================
// spawnedBy routing metadata (v2026.4.29)
// ============================================================================

/// Metadata identifying which session/agent spawned a subagent. Attached to
/// chat and broadcast payloads for the child so clients can route output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnedBy {
    pub session_key: String,
    pub agent_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// Attach `spawnedBy` metadata to an outgoing event payload (object payloads
/// only; non-objects are returned unchanged).
pub fn attach_spawned_by(mut payload: serde_json::Value, spawned_by: &SpawnedBy) -> serde_json::Value {
    if let Some(obj) = payload.as_object_mut() {
        obj.insert(
            "spawnedBy".to_string(),
            serde_json::to_value(spawned_by).unwrap_or(serde_json::Value::Null),
        );
    }
    payload
}

// ============================================================================
// Duplicate parent-visible reply suppression (v2026.5.2)
// ============================================================================

/// Window during which a child completion following a direct `sessions_send`
/// from its parent is treated as already-delivered.
pub const SESSIONS_SEND_REPLY_WINDOW: Duration = Duration::from_secs(300);

/// Tracks `sessions_send` calls from a parent to its own persistent subagent
/// sessions so the child's completion is not ALSO announced as a
/// parent-visible reply (the send's tool result already contained it).
#[derive(Debug, Default)]
pub struct DuplicateReplyGuard {
    recent_sends: HashMap<(String, String), Instant>,
}

impl DuplicateReplyGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `parent` sent directly into `child` via `sessions_send`.
    pub fn record_sessions_send(&mut self, parent: &str, child: &str, now: Instant) {
        self.recent_sends
            .insert((parent.to_string(), child.to_string()), now);
    }

    /// Whether the parent-visible reply for this child completion should be
    /// suppressed. One-shot: a suppressed entry is consumed.
    pub fn should_suppress_parent_reply(
        &mut self,
        parent: &str,
        child: &str,
        now: Instant,
    ) -> bool {
        let key = (parent.to_string(), child.to_string());
        match self.recent_sends.get(&key) {
            Some(sent_at) if now.duration_since(*sent_at) <= SESSIONS_SEND_REPLY_WINDOW => {
                self.recent_sends.remove(&key);
                true
            }
            Some(_) => {
                // Stale entry — clean up, do not suppress.
                self.recent_sends.remove(&key);
                false
            }
            None => false,
        }
    }
}

/// Whether `target_session` is `owner_session`'s own persistent subagent
/// session. Persistent subagent sessions are keyed
/// `subagent:<owner-session>:<name>` in the port's session-key scheme.
pub fn is_own_persistent_subagent_session(owner_session: &str, target_session: &str) -> bool {
    target_session
        .strip_prefix("subagent:")
        .and_then(|rest| rest.strip_prefix(owner_session))
        .map(|tail| tail.starts_with(':'))
        .unwrap_or(false)
}

// ============================================================================
// Completion outcome classification (v2026.7.1, partial)
// ============================================================================

/// Terminal outcome of a subagent run as seen by the parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentOutcome {
    Success,
    /// Blocked (policy/permission) — reported as an error, not success.
    Error,
}

/// Classify a subagent completion. Blocked and progress-only completions
/// ("still working…", tool-loop blocks) are errors, not successes.
pub fn classify_subagent_completion(final_text: &str, was_blocked: bool) -> SubagentOutcome {
    if was_blocked {
        return SubagentOutcome::Error;
    }
    let trimmed = final_text.trim();
    if trimmed.is_empty() {
        return SubagentOutcome::Error;
    }
    let lower = trimmed.to_ascii_lowercase();
    let progress_only = ["still working", "in progress", "working on it", "no result yet"]
        .iter()
        .any(|p| lower.starts_with(p));
    if progress_only {
        SubagentOutcome::Error
    } else {
        SubagentOutcome::Success
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // completion handoff
    // ------------------------------------------------------------------

    #[test]
    fn default_spawn_expects_completion() {
        let opts = SpawnOptions::default();
        assert!(should_announce_completion(&opts));
    }

    #[test]
    fn expects_completion_false_skips_handoff() {
        let opts = SpawnOptions {
            expects_completion_message: false,
            spawned_by: None,
        };
        assert!(!should_announce_completion(&opts));
    }

    #[test]
    fn spawn_options_deserialize_camel_case_and_default() {
        let opts: SpawnOptions =
            serde_json::from_value(serde_json::json!({"expectsCompletionMessage": false})).unwrap();
        assert!(!opts.expects_completion_message);
        let defaulted: SpawnOptions = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(defaulted.expects_completion_message);
    }

    // ------------------------------------------------------------------
    // spawnedBy metadata
    // ------------------------------------------------------------------

    #[test]
    fn attach_spawned_by_adds_camel_case_field() {
        let sb = SpawnedBy {
            session_key: "parent-sess".into(),
            agent_id: "main".into(),
            run_id: Some("run-1".into()),
        };
        let out = attach_spawned_by(serde_json::json!({"state": "final"}), &sb);
        assert_eq!(out["spawnedBy"]["sessionKey"], "parent-sess");
        assert_eq!(out["spawnedBy"]["agentId"], "main");
        assert_eq!(out["spawnedBy"]["runId"], "run-1");
        assert_eq!(out["state"], "final");
    }

    #[test]
    fn attach_spawned_by_non_object_unchanged() {
        let sb = SpawnedBy {
            session_key: "p".into(),
            agent_id: "a".into(),
            run_id: None,
        };
        let out = attach_spawned_by(serde_json::json!("scalar"), &sb);
        assert_eq!(out, serde_json::json!("scalar"));
    }

    // ------------------------------------------------------------------
    // duplicate reply suppression (tokio::time::pause)
    // ------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn completion_after_sessions_send_is_suppressed_once() {
        let mut guard = DuplicateReplyGuard::new();
        guard.record_sessions_send("parent", "subagent:parent:research", Instant::now());
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(guard.should_suppress_parent_reply(
            "parent",
            "subagent:parent:research",
            Instant::now()
        ));
        // One-shot: a second completion is not suppressed.
        assert!(!guard.should_suppress_parent_reply(
            "parent",
            "subagent:parent:research",
            Instant::now()
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn stale_send_does_not_suppress() {
        let mut guard = DuplicateReplyGuard::new();
        guard.record_sessions_send("p", "c", Instant::now());
        tokio::time::advance(SESSIONS_SEND_REPLY_WINDOW + Duration::from_secs(1)).await;
        assert!(!guard.should_suppress_parent_reply("p", "c", Instant::now()));
    }

    #[tokio::test(start_paused = true)]
    async fn unrelated_completion_not_suppressed() {
        let mut guard = DuplicateReplyGuard::new();
        guard.record_sessions_send("p", "c1", Instant::now());
        assert!(!guard.should_suppress_parent_reply("p", "c2", Instant::now()));
        assert!(!guard.should_suppress_parent_reply("other", "c1", Instant::now()));
    }

    // ------------------------------------------------------------------
    // own persistent subagent session detection
    // ------------------------------------------------------------------

    #[test]
    fn own_persistent_subagent_session_detected() {
        assert!(is_own_persistent_subagent_session("main", "subagent:main:research"));
        assert!(!is_own_persistent_subagent_session("main", "subagent:other:research"));
        assert!(!is_own_persistent_subagent_session("main", "main"));
        assert!(!is_own_persistent_subagent_session("main", "subagent:mainX:r"));
    }

    // ------------------------------------------------------------------
    // completion classification
    // ------------------------------------------------------------------

    #[test]
    fn blocked_completion_is_error() {
        assert_eq!(
            classify_subagent_completion("did some work", true),
            SubagentOutcome::Error
        );
    }

    #[test]
    fn progress_only_completion_is_error() {
        assert_eq!(
            classify_subagent_completion("Still working on the analysis…", false),
            SubagentOutcome::Error
        );
        assert_eq!(classify_subagent_completion("", false), SubagentOutcome::Error);
    }

    #[test]
    fn real_completion_is_success() {
        assert_eq!(
            classify_subagent_completion("Here is the summary: …", false),
            SubagentOutcome::Success
        );
    }
}
