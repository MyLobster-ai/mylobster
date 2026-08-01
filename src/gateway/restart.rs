//! Gateway restart coordination (v2026.5.2 `--force`/`--wait` gateway-side
//! support + v2026.7.1 restart/handoff hardening).
//!
//! The CLI cluster owns `mylobster gateway restart --force/--wait`; this
//! module provides the gateway-side semantics it drives:
//!
//! - active-blocker counting (active chat runs defer a restart),
//! - run IDs logged before a restart is deferred,
//! - `force` overrides blockers, `wait` bounds the deferral window,
//! - a durable restart-handoff record that a successor process validates
//!   (stale / wrong-process / superseded records are discarded).

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use tracing::info;

// ============================================================================
// Restart decision (pure)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartDecision {
    /// No blockers (or forced) — restart immediately.
    Now { forced: bool },
    /// Blockers present — defer; retry until the wait budget expires.
    Deferred {
        blockers: usize,
        run_ids: Vec<String>,
    },
    /// The wait budget expired while blockers remained — restart anyway.
    WaitExpired { blockers: usize },
}

/// Decide how a restart request should proceed.
///
/// * `active_run_ids` — chat runs currently in flight (restart blockers).
/// * `force` — operator override: restart regardless of blockers.
/// * `waited` — how long this request has already been deferred.
/// * `wait_budget` — optional maximum deferral (`--wait <duration>`).
pub fn resolve_restart_decision(
    active_run_ids: &[String],
    force: bool,
    waited: Duration,
    wait_budget: Option<Duration>,
) -> RestartDecision {
    if force {
        return RestartDecision::Now { forced: true };
    }
    if active_run_ids.is_empty() {
        return RestartDecision::Now { forced: false };
    }
    if let Some(budget) = wait_budget {
        if waited >= budget {
            return RestartDecision::WaitExpired {
                blockers: active_run_ids.len(),
            };
        }
    }
    RestartDecision::Deferred {
        blockers: active_run_ids.len(),
        run_ids: active_run_ids.to_vec(),
    }
}

/// Log active task-run IDs before a restart deferral (v2026.5.2: "log active
/// task run IDs before restart deferral").
pub fn log_deferral(run_ids: &[String]) {
    info!(
        "restart deferred: {} active run(s) blocking: [{}]",
        run_ids.len(),
        run_ids.join(", ")
    );
}

// ============================================================================
// Durable restart handoff (v2026.7.1)
// ============================================================================

/// Maximum age of a restart-handoff record before it is considered stale.
pub const HANDOFF_MAX_AGE_MS: u64 = 10 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RestartHandoff {
    /// PID of the process that requested the restart.
    pub pid: u32,
    /// Gateway version that wrote the record.
    pub version: String,
    /// Wall-clock timestamp when the restart was requested.
    pub requested_at_ms: u64,
    /// Monotonically increasing handoff sequence; a higher sequence
    /// supersedes lower ones.
    pub sequence: u64,
}

/// Why a handoff record was discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffRejection {
    Stale,
    WrongProcess,
    Superseded,
}

/// Validate a restart-handoff record read at successor startup.
///
/// * `now_ms` — current wall-clock ms.
/// * `expected_pid` — the predecessor PID recorded by the supervisor, if
///   known. `None` skips the process check.
/// * `latest_sequence` — highest handoff sequence observed; records with a
///   lower sequence are superseded.
pub fn validate_handoff(
    handoff: &RestartHandoff,
    now_ms: u64,
    expected_pid: Option<u32>,
    latest_sequence: u64,
) -> Result<(), HandoffRejection> {
    if now_ms.saturating_sub(handoff.requested_at_ms) > HANDOFF_MAX_AGE_MS {
        return Err(HandoffRejection::Stale);
    }
    if let Some(pid) = expected_pid {
        if handoff.pid != pid {
            return Err(HandoffRejection::WrongProcess);
        }
    }
    if handoff.sequence < latest_sequence {
        return Err(HandoffRejection::Superseded);
    }
    Ok(())
}

/// Persist a handoff record (atomic write via temp file + rename).
pub fn write_handoff(path: &Path, handoff: &RestartHandoff) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(handoff)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Read a handoff record; returns `None` when absent or unparsable
/// (unparsable records are treated as discarded).
pub fn read_handoff(path: &Path) -> Option<RestartHandoff> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

// ============================================================================
// Coordinator
// ============================================================================

/// Tracks restart requests against the live run ledger.
#[derive(Default)]
pub struct RestartCoordinator {
    sequence: std::sync::atomic::AtomicU64,
    pending: parking_lot::Mutex<Option<PendingRestart>>,
}

#[derive(Debug, Clone)]
struct PendingRestart {
    requested_at: std::time::Instant,
    force: bool,
    wait_budget: Option<Duration>,
}

impl RestartCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn next_sequence(&self) -> u64 {
        self.sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1
    }

    /// Register a restart request; returns the decision for the current
    /// blocker set.
    pub fn request(
        &self,
        active_run_ids: &[String],
        force: bool,
        wait_budget: Option<Duration>,
    ) -> RestartDecision {
        let mut pending = self.pending.lock();
        let waited = pending
            .as_ref()
            .map(|p| p.requested_at.elapsed())
            .unwrap_or(Duration::ZERO);
        let decision = resolve_restart_decision(active_run_ids, force, waited, wait_budget);
        match &decision {
            RestartDecision::Deferred { run_ids, .. } => {
                log_deferral(run_ids);
                if pending.is_none() {
                    *pending = Some(PendingRestart {
                        requested_at: std::time::Instant::now(),
                        force,
                        wait_budget,
                    });
                }
            }
            _ => {
                *pending = None;
            }
        }
        decision
    }

    pub fn has_pending(&self) -> bool {
        self.pending.lock().is_some()
    }

    /// Re-evaluate a pending deferral (e.g. when a run completes).
    pub fn reevaluate(&self, active_run_ids: &[String]) -> Option<RestartDecision> {
        let pending = self.pending.lock().clone();
        pending.map(|p| {
            resolve_restart_decision(
                active_run_ids,
                p.force,
                p.requested_at.elapsed(),
                p.wait_budget,
            )
        })
    }

    pub fn clear(&self) {
        *self.pending.lock() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_blockers_restarts_now() {
        let d = resolve_restart_decision(&[], false, Duration::ZERO, None);
        assert_eq!(d, RestartDecision::Now { forced: false });
    }

    #[test]
    fn force_overrides_blockers() {
        let d = resolve_restart_decision(&runs(&["r1", "r2"]), true, Duration::ZERO, None);
        assert_eq!(d, RestartDecision::Now { forced: true });
    }

    #[test]
    fn blockers_defer_and_report_run_ids() {
        let d = resolve_restart_decision(&runs(&["r1", "r2"]), false, Duration::ZERO, None);
        match d {
            RestartDecision::Deferred { blockers, run_ids } => {
                assert_eq!(blockers, 2);
                assert_eq!(run_ids, runs(&["r1", "r2"]));
            }
            other => panic!("expected deferred, got {other:?}"),
        }
    }

    #[test]
    fn wait_budget_expiry_restarts_anyway() {
        let d = resolve_restart_decision(
            &runs(&["r1"]),
            false,
            Duration::from_secs(31),
            Some(Duration::from_secs(30)),
        );
        assert_eq!(d, RestartDecision::WaitExpired { blockers: 1 });
    }

    #[test]
    fn within_wait_budget_still_defers() {
        let d = resolve_restart_decision(
            &runs(&["r1"]),
            false,
            Duration::from_secs(5),
            Some(Duration::from_secs(30)),
        );
        assert!(matches!(d, RestartDecision::Deferred { .. }));
    }

    // ---- handoff ----

    fn handoff(seq: u64, at_ms: u64, pid: u32) -> RestartHandoff {
        RestartHandoff {
            pid,
            version: "0.1.0".to_string(),
            requested_at_ms: at_ms,
            sequence: seq,
        }
    }

    #[test]
    fn handoff_valid_roundtrip() {
        let h = handoff(3, 1_000, 42);
        assert!(validate_handoff(&h, 2_000, Some(42), 3).is_ok());
    }

    #[test]
    fn handoff_stale_rejected() {
        let h = handoff(1, 0, 42);
        assert_eq!(
            validate_handoff(&h, HANDOFF_MAX_AGE_MS + 1, None, 1),
            Err(HandoffRejection::Stale)
        );
    }

    #[test]
    fn handoff_wrong_process_rejected() {
        let h = handoff(1, 1_000, 42);
        assert_eq!(
            validate_handoff(&h, 1_500, Some(43), 1),
            Err(HandoffRejection::WrongProcess)
        );
    }

    #[test]
    fn handoff_superseded_rejected() {
        let h = handoff(1, 1_000, 42);
        assert_eq!(
            validate_handoff(&h, 1_500, Some(42), 2),
            Err(HandoffRejection::Superseded)
        );
    }

    #[test]
    fn handoff_persistence_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("state/restart-handoff.json");
        let h = handoff(7, 123, 99);
        write_handoff(&path, &h).unwrap();
        assert_eq!(read_handoff(&path), Some(h));
        // Unparsable → None
        std::fs::write(&path, b"not json").unwrap();
        assert!(read_handoff(&path).is_none());
        // Absent → None
        assert!(read_handoff(&dir.path().join("missing.json")).is_none());
    }

    // ---- coordinator ----

    #[test]
    fn coordinator_defers_then_releases() {
        let c = RestartCoordinator::new();
        let d = c.request(&runs(&["r1"]), false, None);
        assert!(matches!(d, RestartDecision::Deferred { .. }));
        assert!(c.has_pending());
        // Run completes → reevaluate → Now
        let d = c.reevaluate(&[]).unwrap();
        assert_eq!(d, RestartDecision::Now { forced: false });
        c.clear();
        assert!(!c.has_pending());
    }

    #[test]
    fn coordinator_sequence_increments() {
        let c = RestartCoordinator::new();
        assert_eq!(c.next_sequence(), 1);
        assert_eq!(c.next_sequence(), 2);
    }
}
