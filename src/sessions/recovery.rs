//! Bounded automatic orphan recovery + wedged-session tombstones
//! (v2026.5.2 parity, "Subagents: bound automatic orphan recovery +
//! wedged-session tombstone").
//!
//! After a restart, subagent sessions whose parent run is gone are orphans.
//! Automatic recovery is bounded two ways:
//! - at most `max_recoveries_per_scan` orphans are recovered per sweep (the
//!   rest are deferred to the next sweep), and
//! - a session that keeps failing recovery (`max_attempts_per_session`) is
//!   tombstoned as wedged and never automatically retried again.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Bounds for automatic orphan recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryLimits {
    /// Max orphans recovered per scan; the rest are deferred.
    pub max_recoveries_per_scan: usize,
    /// Recovery attempts after which a session is tombstoned as wedged.
    pub max_attempts_per_session: u32,
}

impl Default for RecoveryLimits {
    fn default() -> Self {
        Self {
            max_recoveries_per_scan: 5,
            max_attempts_per_session: 3,
        }
    }
}

/// Persistent marker for a wedged session that must not be auto-recovered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tombstone {
    pub session_key: String,
    pub reason: String,
    pub attempts: u32,
    pub created_at_ms: i64,
}

/// Result of planning one recovery sweep.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryPlan {
    /// Orphans to attempt recovery on this sweep (bounded).
    pub recover: Vec<String>,
    /// Orphans deferred to a later sweep by the per-scan bound.
    pub deferred: Vec<String>,
    /// Sessions newly tombstoned as wedged this sweep.
    pub tombstoned: Vec<Tombstone>,
    /// Orphans skipped because they were already tombstoned.
    pub skipped_tombstoned: Vec<String>,
}

/// Tracks recovery attempts across sweeps and plans bounded recovery.
#[derive(Debug, Default)]
pub struct OrphanRecovery {
    limits: RecoveryLimits,
    attempts: HashMap<String, u32>,
    tombstoned: HashSet<String>,
}

impl OrphanRecovery {
    pub fn new(limits: RecoveryLimits) -> Self {
        Self {
            limits,
            ..Default::default()
        }
    }

    /// Seed already-persisted tombstones (loaded at startup) so wedged
    /// sessions stay excluded across restarts.
    pub fn seed_tombstones<I: IntoIterator<Item = String>>(&mut self, keys: I) {
        self.tombstoned.extend(keys);
    }

    pub fn is_tombstoned(&self, session_key: &str) -> bool {
        self.tombstoned.contains(session_key)
    }

    /// Plan one sweep over the currently detected orphans. Increments the
    /// attempt counter for every session selected for recovery; sessions
    /// that already exhausted their attempts are tombstoned instead.
    pub fn plan_sweep(&mut self, orphans: &[String], now_ms: i64) -> RecoveryPlan {
        let mut plan = RecoveryPlan::default();
        for key in orphans {
            if self.tombstoned.contains(key) {
                plan.skipped_tombstoned.push(key.clone());
                continue;
            }
            let attempts = self.attempts.get(key).copied().unwrap_or(0);
            if attempts >= self.limits.max_attempts_per_session {
                let tomb = Tombstone {
                    session_key: key.clone(),
                    reason: format!(
                        "wedged: exhausted {attempts} automatic recovery attempts"
                    ),
                    attempts,
                    created_at_ms: now_ms,
                };
                self.tombstoned.insert(key.clone());
                plan.tombstoned.push(tomb);
                continue;
            }
            if plan.recover.len() >= self.limits.max_recoveries_per_scan {
                plan.deferred.push(key.clone());
                continue;
            }
            *self.attempts.entry(key.clone()).or_insert(0) += 1;
            plan.recover.push(key.clone());
        }
        plan
    }

    /// Mark a session successfully recovered — clears its attempt counter.
    pub fn mark_recovered(&mut self, session_key: &str) {
        self.attempts.remove(session_key);
    }
}

// ============================================================================
// Tombstone persistence
// ============================================================================

fn tombstone_file_name(session_key: &str) -> String {
    let sanitized: String = session_key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{sanitized}.tombstone.json")
}

/// Persist a tombstone marker into `dir`.
pub fn write_tombstone(dir: &Path, tombstone: &Tombstone) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(tombstone_file_name(&tombstone.session_key));
    let body = serde_json::to_vec_pretty(tombstone)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    crate::sessions::sandbox::atomic_replace_preserving_mode(&path, &body)?;
    Ok(path)
}

/// Load all persisted tombstones from `dir` (missing dir → empty).
pub fn load_tombstones(dir: &Path) -> std::io::Result<Vec<Tombstone>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let is_tombstone = path
            .file_name()
            .and_then(|f| f.to_str())
            .is_some_and(|f| f.ends_with(".tombstone.json"));
        if !is_tombstone {
            continue;
        }
        let Ok(body) = std::fs::read(&path) else { continue };
        if let Ok(tomb) = serde_json::from_slice::<Tombstone>(&body) {
            out.push(tomb);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("subagent:task-{i}")).collect()
    }

    #[test]
    fn recovery_is_bounded_per_scan() {
        let mut rec = OrphanRecovery::new(RecoveryLimits {
            max_recoveries_per_scan: 3,
            max_attempts_per_session: 5,
        });
        let orphans = keys(10);
        let plan = rec.plan_sweep(&orphans, 1_000);
        assert_eq!(plan.recover.len(), 3);
        assert_eq!(plan.deferred.len(), 7);
        assert!(plan.tombstoned.is_empty());
    }

    #[test]
    fn deferred_orphans_recover_on_later_sweeps() {
        let mut rec = OrphanRecovery::new(RecoveryLimits {
            max_recoveries_per_scan: 2,
            max_attempts_per_session: 5,
        });
        let orphans = keys(3);
        let first = rec.plan_sweep(&orphans, 0);
        assert_eq!(first.recover.len(), 2);
        // First two recovered successfully; third orphan remains.
        for k in &first.recover {
            rec.mark_recovered(k);
        }
        let second = rec.plan_sweep(&[orphans[2].clone()], 0);
        assert_eq!(second.recover, vec![orphans[2].clone()]);
    }

    #[test]
    fn wedged_session_is_tombstoned_after_exhausted_attempts() {
        let mut rec = OrphanRecovery::new(RecoveryLimits {
            max_recoveries_per_scan: 10,
            max_attempts_per_session: 3,
        });
        let orphan = vec!["subagent:wedged".to_string()];
        for _ in 0..3 {
            let plan = rec.plan_sweep(&orphan, 0);
            assert_eq!(plan.recover.len(), 1, "attempts under the cap recover");
        }
        // Fourth sweep: attempts exhausted → tombstone, not recovery.
        let plan = rec.plan_sweep(&orphan, 42);
        assert!(plan.recover.is_empty());
        assert_eq!(plan.tombstoned.len(), 1);
        assert_eq!(plan.tombstoned[0].session_key, "subagent:wedged");
        assert_eq!(plan.tombstoned[0].attempts, 3);
        assert_eq!(plan.tombstoned[0].created_at_ms, 42);

        // Fifth sweep: tombstoned sessions are skipped, never retried.
        let plan = rec.plan_sweep(&orphan, 43);
        assert!(plan.recover.is_empty());
        assert!(plan.tombstoned.is_empty());
        assert_eq!(plan.skipped_tombstoned, orphan);
    }

    #[test]
    fn successful_recovery_resets_attempt_counter() {
        let mut rec = OrphanRecovery::new(RecoveryLimits {
            max_recoveries_per_scan: 10,
            max_attempts_per_session: 2,
        });
        let orphan = vec!["subagent:flaky".to_string()];
        rec.plan_sweep(&orphan, 0);
        rec.mark_recovered("subagent:flaky");
        // Counter reset — two more attempts available before tombstoning.
        rec.plan_sweep(&orphan, 0);
        rec.plan_sweep(&orphan, 0);
        let plan = rec.plan_sweep(&orphan, 0);
        assert_eq!(plan.tombstoned.len(), 1);
    }

    #[test]
    fn seeded_tombstones_survive_restart_semantics() {
        let mut rec = OrphanRecovery::new(RecoveryLimits::default());
        rec.seed_tombstones(vec!["subagent:old-wedge".to_string()]);
        let plan = rec.plan_sweep(&["subagent:old-wedge".to_string()], 0);
        assert!(plan.recover.is_empty());
        assert_eq!(plan.skipped_tombstoned.len(), 1);
    }

    #[test]
    fn tombstones_roundtrip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let tomb = Tombstone {
            session_key: "subagent:task/1:weird key".to_string(),
            reason: "wedged".to_string(),
            attempts: 3,
            created_at_ms: 99,
        };
        let path = write_tombstone(dir.path(), &tomb).unwrap();
        assert!(path.exists());
        let loaded = load_tombstones(dir.path()).unwrap();
        assert_eq!(loaded, vec![tomb]);
    }

    #[test]
    fn load_tombstones_from_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load_tombstones(&dir.path().join("nope")).unwrap();
        assert!(loaded.is_empty());
    }
}
