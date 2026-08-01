//! Session store maintenance: age/count/disk eviction (v2026.5.2 parity).
//!
//! - Durable external conversation pointers (group/thread/topic-scoped
//!   sessions that map an external chat to its session) are exempt from
//!   age/count/disk maintenance eviction — evicting them would sever the
//!   external conversation binding (v2026.5.2).
//! - Session-store *reads* must not trigger stale prune/cap maintenance
//!   while startup is still in progress: the [`MaintenanceGate`] holds
//!   read-triggered maintenance until the store is marked ready
//!   (v2026.5.2).
//! - Large transcripts are never rewritten through a synchronous whole-file
//!   path: eviction planning marks them for the async streaming rewrite in
//!   [`crate::sessions::transcript`] (v2026.5.2).

use crate::config::SessionMaintenanceConfig;
use crate::sessions::transcript;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Resolved maintenance policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MaintenancePolicy {
    /// Evict sessions idle longer than this.
    pub max_age: Option<Duration>,
    /// Cap on the number of retained (non-durable) sessions.
    pub max_count: Option<usize>,
    /// Disk budget for stored transcripts.
    pub max_disk_bytes: Option<u64>,
}

impl MaintenancePolicy {
    pub fn from_config(cfg: Option<&SessionMaintenanceConfig>) -> Self {
        let Some(cfg) = cfg else {
            return Self::default();
        };
        Self {
            max_age: cfg
                .prune_days
                .filter(|&d| d > 0)
                .map(|d| Duration::from_secs(u64::from(d) * 24 * 60 * 60)),
            max_count: cfg
                .max_entries
                .filter(|&n| n > 0)
                .map(|n| n as usize),
            max_disk_bytes: cfg.max_disk_bytes.filter(|&b| b > 0),
        }
    }

    pub fn is_noop(&self) -> bool {
        self.max_age.is_none() && self.max_count.is_none() && self.max_disk_bytes.is_none()
    }
}

/// Maintenance-relevant view of one stored session.
#[derive(Debug, Clone)]
pub struct SessionMaintRecord {
    pub session_key: String,
    /// Last-activity timestamp (unix ms).
    pub updated_at_ms: i64,
    /// On-disk transcript size.
    pub transcript_bytes: u64,
    /// Durable external conversation pointer (group/thread-scoped) — exempt
    /// from eviction.
    pub durable_external_pointer: bool,
}

/// Classify a session key as a durable external conversation pointer.
///
/// Group- and thread-scoped session keys bind an external conversation
/// (a group chat, forum topic, or channel thread) to its session; losing the
/// key loses the conversation. DM/main sessions are not durable pointers.
pub fn is_durable_external_pointer(session_key: &str) -> bool {
    let key = session_key.to_ascii_lowercase();
    const MARKERS: [&str; 4] = [":group:", ":thread:", ":topic:", ":channel:"];
    if MARKERS.iter().any(|m| key.contains(m)) {
        return true;
    }
    const PREFIXES: [&str; 3] = ["group:", "thread:", "topic:"];
    PREFIXES.iter().any(|p| key.starts_with(p))
}

/// One planned eviction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvictionCandidate {
    pub session_key: String,
    pub reason: EvictionReason,
    /// Transcript is large enough that any rewrite/removal bookkeeping must
    /// go through the async streaming path, never a synchronous whole-file
    /// reopen.
    pub requires_streaming_rewrite: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictionReason {
    Age,
    Count,
    Disk,
}

/// Select eviction candidates under the policy. Durable external
/// conversation pointers are never selected, for any reason.
pub fn select_eviction_candidates(
    records: &[SessionMaintRecord],
    policy: &MaintenancePolicy,
    now_ms: i64,
) -> Vec<EvictionCandidate> {
    let mut candidates: Vec<EvictionCandidate> = Vec::new();
    let mut evicted: std::collections::HashSet<String> = std::collections::HashSet::new();

    let push = |rec: &SessionMaintRecord,
                reason: EvictionReason,
                candidates: &mut Vec<EvictionCandidate>,
                evicted: &mut std::collections::HashSet<String>|
     -> bool {
        if rec.durable_external_pointer || is_durable_external_pointer(&rec.session_key) {
            return false;
        }
        if !evicted.insert(rec.session_key.clone()) {
            return false;
        }
        candidates.push(EvictionCandidate {
            session_key: rec.session_key.clone(),
            reason,
            requires_streaming_rewrite: transcript::requires_streaming_rewrite(
                rec.transcript_bytes,
            ),
        });
        true
    };

    // 1. Age-based pruning.
    if let Some(max_age) = policy.max_age {
        let cutoff = now_ms - max_age.as_millis() as i64;
        for rec in records {
            if rec.updated_at_ms < cutoff {
                push(rec, EvictionReason::Age, &mut candidates, &mut evicted);
            }
        }
    }

    // Remaining records, oldest first, for count/disk caps.
    let mut remaining: Vec<&SessionMaintRecord> = records
        .iter()
        .filter(|r| !evicted.contains(r.session_key.as_str()))
        .collect();
    remaining.sort_by_key(|r| r.updated_at_ms);

    // 2. Count cap (durable pointers don't count toward the cap and are
    //    never evicted for it).
    if let Some(max_count) = policy.max_count {
        let evictable: Vec<&SessionMaintRecord> = remaining
            .iter()
            .copied()
            .filter(|r| !r.durable_external_pointer && !is_durable_external_pointer(&r.session_key))
            .collect();
        if evictable.len() > max_count {
            let overflow = evictable.len() - max_count;
            for rec in evictable.into_iter().take(overflow) {
                push(rec, EvictionReason::Count, &mut candidates, &mut evicted);
            }
        }
    }

    // 3. Disk budget: evict oldest non-durable sessions until under budget.
    //    Durable-pointer bytes count toward the total but their sessions are
    //    never evicted.
    if let Some(max_disk) = policy.max_disk_bytes {
        let mut total: u64 = records
            .iter()
            .filter(|r| !evicted.contains(r.session_key.as_str()))
            .map(|r| r.transcript_bytes)
            .sum();
        if total > max_disk {
            for rec in remaining {
                if total <= max_disk {
                    break;
                }
                if evicted.contains(rec.session_key.as_str()) {
                    continue;
                }
                if push(rec, EvictionReason::Disk, &mut candidates, &mut evicted) {
                    total = total.saturating_sub(rec.transcript_bytes);
                }
            }
        }
    }

    candidates
}

// ============================================================================
// Startup gate
// ============================================================================

/// Gate that keeps read-triggered maintenance from running until startup is
/// complete, and serializes maintenance passes.
#[derive(Debug, Default)]
pub struct MaintenanceGate {
    startup_complete: AtomicBool,
    running: AtomicBool,
}

/// RAII token for an in-flight maintenance pass.
pub struct MaintenanceRun<'a> {
    gate: &'a MaintenanceGate,
}

impl Drop for MaintenanceRun<'_> {
    fn drop(&mut self) {
        self.gate.running.store(false, Ordering::SeqCst);
    }
}

impl MaintenanceGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the session store fully loaded; read-triggered maintenance is
    /// allowed from this point on.
    pub fn mark_startup_complete(&self) {
        self.startup_complete.store(true, Ordering::SeqCst);
    }

    pub fn startup_complete(&self) -> bool {
        self.startup_complete.load(Ordering::SeqCst)
    }

    /// Whether a session-store *read* may opportunistically kick off
    /// prune/cap maintenance right now. Always false during startup: reads
    /// against a store that is still loading would run maintenance over a
    /// stale, partially-hydrated view (v2026.5.2).
    pub fn read_may_trigger_maintenance(&self) -> bool {
        self.startup_complete() && !self.running.load(Ordering::SeqCst)
    }

    /// Begin a maintenance pass, unless startup is still in progress or a
    /// pass is already running.
    pub fn try_begin(&self) -> Option<MaintenanceRun<'_>> {
        if !self.startup_complete() {
            return None;
        }
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return None;
        }
        Some(MaintenanceRun { gate: self })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(key: &str, updated_at_ms: i64, bytes: u64) -> SessionMaintRecord {
        SessionMaintRecord {
            session_key: key.to_string(),
            updated_at_ms,
            transcript_bytes: bytes,
            durable_external_pointer: is_durable_external_pointer(key),
        }
    }

    const DAY_MS: i64 = 24 * 60 * 60 * 1000;

    // ------------------------------------------------------------------
    // Durable pointer classification
    // ------------------------------------------------------------------

    #[test]
    fn group_and_thread_scoped_keys_are_durable_pointers() {
        assert!(is_durable_external_pointer("telegram:group:12345"));
        assert!(is_durable_external_pointer("discord:guild:1:thread:99"));
        assert!(is_durable_external_pointer("telegram:g1:topic:42"));
        assert!(is_durable_external_pointer("group:whatsapp:abc"));
        assert!(is_durable_external_pointer("slack:T1:channel:C42"));
    }

    #[test]
    fn dm_and_main_keys_are_not_durable_pointers() {
        assert!(!is_durable_external_pointer("default"));
        assert!(!is_durable_external_pointer("telegram:dm:123"));
        assert!(!is_durable_external_pointer("subagent:task-1"));
    }

    // ------------------------------------------------------------------
    // Eviction exemption logic
    // ------------------------------------------------------------------

    #[test]
    fn age_eviction_skips_durable_pointers() {
        let now = 100 * DAY_MS;
        let records = vec![
            rec("old-dm", now - 40 * DAY_MS, 100),
            rec("telegram:group:1", now - 40 * DAY_MS, 100),
        ];
        let policy = MaintenancePolicy {
            max_age: Some(Duration::from_secs(30 * 24 * 60 * 60)),
            ..Default::default()
        };
        let out = select_eviction_candidates(&records, &policy, now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session_key, "old-dm");
        assert_eq!(out[0].reason, EvictionReason::Age);
    }

    #[test]
    fn count_cap_evicts_oldest_non_durable_only() {
        let now = 100 * DAY_MS;
        let records = vec![
            rec("s1", now - 5 * DAY_MS, 10),
            rec("s2", now - 4 * DAY_MS, 10),
            rec("s3", now - 3 * DAY_MS, 10),
            rec("thread:ext:1", now - 90 * DAY_MS, 10), // oldest, but durable
        ];
        let policy = MaintenancePolicy {
            max_count: Some(2),
            ..Default::default()
        };
        let out = select_eviction_candidates(&records, &policy, now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session_key, "s1");
        assert_eq!(out[0].reason, EvictionReason::Count);
    }

    #[test]
    fn disk_budget_evicts_oldest_non_durable_until_under_budget() {
        let now = 100 * DAY_MS;
        let records = vec![
            rec("s1", now - 5 * DAY_MS, 600),
            rec("s2", now - 4 * DAY_MS, 300),
            rec("telegram:group:1", now - 50 * DAY_MS, 500), // durable, big, old
            rec("s3", now - 1 * DAY_MS, 100),
        ];
        let policy = MaintenancePolicy {
            max_disk_bytes: Some(1000),
            ..Default::default()
        };
        let out = select_eviction_candidates(&records, &policy, now);
        // total = 1500 → must free ≥500; oldest non-durable is s1 (600).
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session_key, "s1");
        assert_eq!(out[0].reason, EvictionReason::Disk);
    }

    #[test]
    fn disk_budget_never_selects_durable_even_when_over_budget() {
        let now = 10 * DAY_MS;
        let records = vec![rec("telegram:group:1", now - 9 * DAY_MS, 10_000)];
        let policy = MaintenancePolicy {
            max_disk_bytes: Some(100),
            ..Default::default()
        };
        assert!(select_eviction_candidates(&records, &policy, now).is_empty());
    }

    #[test]
    fn candidates_are_deduped_across_reasons() {
        let now = 100 * DAY_MS;
        let records = vec![
            rec("old-and-big", now - 90 * DAY_MS, 10_000),
            rec("fresh", now, 10),
        ];
        let policy = MaintenancePolicy {
            max_age: Some(Duration::from_secs(30 * 24 * 60 * 60)),
            max_count: Some(1),
            max_disk_bytes: Some(100),
        };
        let out = select_eviction_candidates(&records, &policy, now);
        let count = out
            .iter()
            .filter(|c| c.session_key == "old-and-big")
            .count();
        assert_eq!(count, 1, "each session evicted at most once");
    }

    #[test]
    fn large_transcripts_flagged_for_streaming_rewrite() {
        let now = 100 * DAY_MS;
        let records = vec![rec(
            "huge-old",
            now - 90 * DAY_MS,
            transcript::LARGE_TRANSCRIPT_STREAMING_THRESHOLD_BYTES + 1,
        )];
        let policy = MaintenancePolicy {
            max_age: Some(Duration::from_secs(30 * 24 * 60 * 60)),
            ..Default::default()
        };
        let out = select_eviction_candidates(&records, &policy, now);
        assert!(out[0].requires_streaming_rewrite);
    }

    #[test]
    fn noop_policy_selects_nothing() {
        let records = vec![rec("s1", 0, 10)];
        let out = select_eviction_candidates(&records, &MaintenancePolicy::default(), DAY_MS);
        assert!(out.is_empty());
    }

    // ------------------------------------------------------------------
    // Startup gate
    // ------------------------------------------------------------------

    #[test]
    fn reads_never_trigger_maintenance_during_startup() {
        let gate = MaintenanceGate::new();
        assert!(!gate.read_may_trigger_maintenance());
        assert!(gate.try_begin().is_none(), "no maintenance during startup");

        gate.mark_startup_complete();
        assert!(gate.read_may_trigger_maintenance());
        let run = gate.try_begin();
        assert!(run.is_some());
    }

    #[test]
    fn maintenance_passes_are_serialized() {
        let gate = MaintenanceGate::new();
        gate.mark_startup_complete();
        let first = gate.try_begin().unwrap();
        assert!(gate.try_begin().is_none(), "second pass blocked while first runs");
        assert!(!gate.read_may_trigger_maintenance());
        drop(first);
        assert!(gate.try_begin().is_some());
    }

    // ------------------------------------------------------------------
    // Policy resolution
    // ------------------------------------------------------------------

    #[test]
    fn policy_resolves_from_config() {
        let cfg = SessionMaintenanceConfig {
            prune_days: Some(30),
            max_entries: Some(500),
            max_disk_bytes: Some(1_000_000),
            ..Default::default()
        };
        let policy = MaintenancePolicy::from_config(Some(&cfg));
        assert_eq!(policy.max_age, Some(Duration::from_secs(30 * 24 * 60 * 60)));
        assert_eq!(policy.max_count, Some(500));
        assert_eq!(policy.max_disk_bytes, Some(1_000_000));
        assert!(MaintenancePolicy::from_config(None).is_noop());
    }
}
