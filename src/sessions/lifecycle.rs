//! Session lifecycle state (v2026.5.2 / v2026.7.1 parity).
//!
//! - Canonical terminal-outcome normalization: prose outcome strings map to a
//!   small canonical set instead of ad-hoc classifiers (v2026.7.1).
//! - Preserve terminal lifecycle state when final run metadata is persisted
//!   from a stale in-memory snapshot (v2026.5.2): a snapshot that predates
//!   the recorded terminal transition must not downgrade the stored state.
//! - Reject stale / pre-reset lifecycle events: events carrying an older
//!   reset epoch than the current record are dropped (v2026.7.1).

use serde::{Deserialize, Serialize};

/// Canonical lifecycle states for a session run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LifecycleState {
    Pending,
    Active,
    Completed,
    Aborted,
    Errored,
}

impl LifecycleState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Aborted | Self::Errored)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Aborted => "aborted",
            Self::Errored => "errored",
        }
    }
}

/// Canonical terminal-outcome normalization: maps the assorted outcome
/// strings produced by runs, providers, and older store rows to the
/// canonical lifecycle states. Returns `None` for non-terminal / unknown
/// outcomes.
pub fn normalize_terminal_outcome(raw: &str) -> Option<LifecycleState> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "completed" | "complete" | "done" | "success" | "succeeded" | "ok" | "finished" => {
            Some(LifecycleState::Completed)
        }
        "aborted" | "abort" | "cancelled" | "canceled" | "cancel" | "stopped" | "interrupted" => {
            Some(LifecycleState::Aborted)
        }
        "errored" | "error" | "failed" | "failure" | "fatal" | "timeout" | "timed_out"
        | "timed-out" => Some(LifecycleState::Errored),
        _ => None,
    }
}

/// Parse any lifecycle state string (terminal or not).
pub fn parse_lifecycle_state(raw: &str) -> Option<LifecycleState> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "pending" | "queued" => Some(LifecycleState::Pending),
        "active" | "running" | "processing" => Some(LifecycleState::Active),
        other => normalize_terminal_outcome(other),
    }
}

/// Stored lifecycle record for a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleRecord {
    pub state: LifecycleState,
    /// Bumped on every session reset; events from before a reset are stale.
    pub reset_epoch: u64,
    pub updated_at_ms: i64,
}

impl LifecycleRecord {
    pub fn new(state: LifecycleState, reset_epoch: u64, updated_at_ms: i64) -> Self {
        Self {
            state,
            reset_epoch,
            updated_at_ms,
        }
    }
}

/// A lifecycle event (or a final-run-metadata snapshot's lifecycle part).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleEvent {
    pub state: LifecycleState,
    pub reset_epoch: u64,
    pub at_ms: i64,
}

/// How an event application resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// Event applied; record updated.
    Applied,
    /// Event carried a reset epoch older than the record — stale, dropped.
    RejectedStaleEpoch,
    /// Event was a stale non-terminal (or older terminal-snapshot) write
    /// against an already-terminal record; the terminal state was preserved.
    PreservedTerminal,
}

/// Apply a lifecycle event to the current record, enforcing:
/// 1. pre-reset (older epoch) events are rejected outright;
/// 2. a terminal state is never downgraded by a stale non-terminal event;
/// 3. an older-timestamp terminal snapshot does not overwrite a newer
///    terminal transition.
pub fn apply_lifecycle_event(record: &mut LifecycleRecord, event: &LifecycleEvent) -> ApplyOutcome {
    if event.reset_epoch < record.reset_epoch {
        return ApplyOutcome::RejectedStaleEpoch;
    }
    if event.reset_epoch == record.reset_epoch && record.state.is_terminal() {
        // Terminal already recorded for this epoch. Only a *newer* terminal
        // event may replace it (e.g. errored → operator abort reconciliation);
        // stale snapshots — non-terminal, or not newer than the recorded
        // transition — preserve the terminal state.
        let is_newer_terminal = event.state.is_terminal() && event.at_ms > record.updated_at_ms;
        if !is_newer_terminal {
            return ApplyOutcome::PreservedTerminal;
        }
    }
    record.state = event.state;
    record.reset_epoch = event.reset_epoch;
    record.updated_at_ms = event.at_ms;
    ApplyOutcome::Applied
}

/// Final run metadata captured in an in-memory snapshot at run end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalRunMetadata {
    pub state: LifecycleState,
    pub reset_epoch: u64,
    pub snapshot_at_ms: i64,
    pub run_id: Option<String>,
}

/// Merge final run metadata into the stored record, preserving terminal
/// lifecycle state when the snapshot is stale (v2026.5.2). Returns the state
/// that should be persisted and how the merge resolved.
pub fn merge_final_run_metadata(
    record: &mut LifecycleRecord,
    snapshot: &FinalRunMetadata,
) -> ApplyOutcome {
    apply_lifecycle_event(
        record,
        &LifecycleEvent {
            state: snapshot.state,
            reset_epoch: snapshot.reset_epoch,
            at_ms: snapshot.snapshot_at_ms,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(state: LifecycleState, epoch: u64, at: i64) -> LifecycleRecord {
        LifecycleRecord::new(state, epoch, at)
    }

    fn event(state: LifecycleState, epoch: u64, at: i64) -> LifecycleEvent {
        LifecycleEvent {
            state,
            reset_epoch: epoch,
            at_ms: at,
        }
    }

    // ------------------------------------------------------------------
    // Canonical terminal-outcome normalization
    // ------------------------------------------------------------------

    #[test]
    fn normalizes_prose_terminal_outcomes() {
        for raw in ["done", "Success", "COMPLETED", "finished", "ok"] {
            assert_eq!(
                normalize_terminal_outcome(raw),
                Some(LifecycleState::Completed),
                "{raw}"
            );
        }
        for raw in ["cancelled", "canceled", "Abort", "stopped", "interrupted"] {
            assert_eq!(
                normalize_terminal_outcome(raw),
                Some(LifecycleState::Aborted),
                "{raw}"
            );
        }
        for raw in ["failed", "error", "timeout", "timed_out", "fatal"] {
            assert_eq!(
                normalize_terminal_outcome(raw),
                Some(LifecycleState::Errored),
                "{raw}"
            );
        }
    }

    #[test]
    fn non_terminal_and_unknown_outcomes_do_not_normalize() {
        assert_eq!(normalize_terminal_outcome("active"), None);
        assert_eq!(normalize_terminal_outcome("running"), None);
        assert_eq!(normalize_terminal_outcome("banana"), None);
        assert_eq!(normalize_terminal_outcome(""), None);
    }

    #[test]
    fn parse_lifecycle_state_covers_all_states() {
        assert_eq!(parse_lifecycle_state("queued"), Some(LifecycleState::Pending));
        assert_eq!(parse_lifecycle_state("running"), Some(LifecycleState::Active));
        assert_eq!(parse_lifecycle_state("done"), Some(LifecycleState::Completed));
    }

    // ------------------------------------------------------------------
    // Terminal preservation vs stale snapshots (v2026.5.2)
    // ------------------------------------------------------------------

    #[test]
    fn stale_snapshot_does_not_downgrade_terminal_state() {
        // Run completed at t=200; a stale in-memory snapshot captured while
        // the run was still active (t=150) persists afterwards.
        let mut rec = record(LifecycleState::Completed, 3, 200);
        let snapshot = FinalRunMetadata {
            state: LifecycleState::Active,
            reset_epoch: 3,
            snapshot_at_ms: 150,
            run_id: Some("run-1".into()),
        };
        let outcome = merge_final_run_metadata(&mut rec, &snapshot);
        assert_eq!(outcome, ApplyOutcome::PreservedTerminal);
        assert_eq!(rec.state, LifecycleState::Completed);
        assert_eq!(rec.updated_at_ms, 200);
    }

    #[test]
    fn stale_snapshot_with_newer_wallclock_still_preserves_terminal() {
        // Even a snapshot flushed *after* the terminal transition must not
        // downgrade to non-terminal.
        let mut rec = record(LifecycleState::Aborted, 1, 200);
        let snapshot = FinalRunMetadata {
            state: LifecycleState::Active,
            reset_epoch: 1,
            snapshot_at_ms: 250,
            run_id: None,
        };
        assert_eq!(
            merge_final_run_metadata(&mut rec, &snapshot),
            ApplyOutcome::PreservedTerminal
        );
        assert_eq!(rec.state, LifecycleState::Aborted);
    }

    #[test]
    fn older_terminal_snapshot_does_not_overwrite_newer_terminal() {
        let mut rec = record(LifecycleState::Aborted, 1, 300);
        let snapshot = FinalRunMetadata {
            state: LifecycleState::Completed,
            reset_epoch: 1,
            snapshot_at_ms: 250,
            run_id: None,
        };
        assert_eq!(
            merge_final_run_metadata(&mut rec, &snapshot),
            ApplyOutcome::PreservedTerminal
        );
        assert_eq!(rec.state, LifecycleState::Aborted);
    }

    #[test]
    fn newer_terminal_event_may_replace_terminal() {
        let mut rec = record(LifecycleState::Errored, 1, 100);
        let outcome =
            apply_lifecycle_event(&mut rec, &event(LifecycleState::Aborted, 1, 150));
        assert_eq!(outcome, ApplyOutcome::Applied);
        assert_eq!(rec.state, LifecycleState::Aborted);
    }

    #[test]
    fn normal_transition_applies() {
        let mut rec = record(LifecycleState::Active, 1, 100);
        let outcome =
            apply_lifecycle_event(&mut rec, &event(LifecycleState::Completed, 1, 200));
        assert_eq!(outcome, ApplyOutcome::Applied);
        assert_eq!(rec.state, LifecycleState::Completed);
        assert_eq!(rec.updated_at_ms, 200);
    }

    // ------------------------------------------------------------------
    // Stale / pre-reset epoch rejection (v2026.7.1)
    // ------------------------------------------------------------------

    #[test]
    fn pre_reset_events_are_rejected() {
        let mut rec = record(LifecycleState::Active, 5, 100);
        let outcome =
            apply_lifecycle_event(&mut rec, &event(LifecycleState::Errored, 4, 500));
        assert_eq!(outcome, ApplyOutcome::RejectedStaleEpoch);
        assert_eq!(rec.state, LifecycleState::Active);
        assert_eq!(rec.reset_epoch, 5);
    }

    #[test]
    fn newer_epoch_event_resets_even_over_terminal() {
        // After a /reset, the new epoch starts fresh — a terminal state from
        // the previous epoch does not pin the record forever.
        let mut rec = record(LifecycleState::Completed, 2, 100);
        let outcome = apply_lifecycle_event(&mut rec, &event(LifecycleState::Active, 3, 50));
        assert_eq!(outcome, ApplyOutcome::Applied);
        assert_eq!(rec.state, LifecycleState::Active);
        assert_eq!(rec.reset_epoch, 3);
    }
}
