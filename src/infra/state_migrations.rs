//! Versioned state migrations with a migration lease (v2026.7.1 parity).
//!
//! State migrations run to convergence before gateway readiness; failures
//! fail closed with `doctor --fix` guidance. A file-based lease prevents two
//! processes from migrating concurrently, and the lease is released on exit
//! (or reclaimed when stale).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Current state schema version for this build.
pub const CURRENT_STATE_VERSION: u32 = 3;

/// Lease staleness threshold (5 minutes).
pub const LEASE_STALE_MS: u64 = 5 * 60 * 1000;

/// A registered migration step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationStep {
    /// Version this step migrates *to*.
    pub to_version: u32,
    pub name: &'static str,
}

/// The registered migration chain (must be contiguous and ascending).
pub const MIGRATIONS: &[MigrationStep] = &[
    MigrationStep { to_version: 1, name: "init-state-dir" },
    MigrationStep { to_version: 2, name: "session-org-store" },
    MigrationStep { to_version: 3, name: "boot-ledger" },
];

/// Plan the steps needed to migrate from `from_version` to the current
/// version. Returns an error (fail closed, with doctor guidance) when the
/// stored version is *newer* than this build supports.
pub fn plan_migrations(from_version: u32) -> Result<Vec<&'static MigrationStep>, String> {
    if from_version > CURRENT_STATE_VERSION {
        return Err(format!(
            "state version {from_version} is newer than this build supports \
             ({CURRENT_STATE_VERSION}); refusing to start — run a newer gateway or \
             `mylobster doctor --fix` to inspect state"
        ));
    }
    Ok(MIGRATIONS
        .iter()
        .filter(|m| m.to_version > from_version)
        .collect())
}

// ============================================================================
// Version file
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateVersionFile {
    pub version: u32,
}

pub fn read_state_version(path: &Path) -> u32 {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<StateVersionFile>(&b).ok())
        .map(|f| f.version)
        .unwrap_or(0)
}

pub fn write_state_version(path: &Path, version: u32) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(&StateVersionFile { version })?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ============================================================================
// Migration lease
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationLease {
    pub pid: u32,
    pub acquired_at_ms: u64,
}

/// RAII guard that releases the lease file on drop (release-on-exit).
#[derive(Debug)]
pub struct LeaseGuard {
    path: PathBuf,
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Acquire the migration lease. An existing fresh lease from another live
/// process fails closed; a stale lease is reclaimed.
pub fn acquire_lease(path: &Path, now_ms: u64) -> Result<LeaseGuard, String> {
    if let Some(existing) = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<MigrationLease>(&b).ok())
    {
        let age = now_ms.saturating_sub(existing.acquired_at_ms);
        if age <= LEASE_STALE_MS && existing.pid != std::process::id() {
            return Err(format!(
                "state migration lease held by pid {} ({}s old); refusing to start — \
                 if that process is dead, run `mylobster doctor --fix`",
                existing.pid,
                age / 1000
            ));
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let lease = MigrationLease {
        pid: std::process::id(),
        acquired_at_ms: now_ms,
    };
    std::fs::write(path, serde_json::to_vec(&lease).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    Ok(LeaseGuard {
        path: path.to_path_buf(),
    })
}

/// Run all pending migrations to convergence. Steps are currently structural
/// no-ops (state directories are created lazily); the version file is
/// advanced step-by-step so a crash resumes where it left off.
pub fn run_migrations(state_dir: &Path, now_ms: u64) -> Result<u32, String> {
    let version_path = state_dir.join("state-version.json");
    let lease_path = state_dir.join("migration.lease");
    let from = read_state_version(&version_path);
    let plan = plan_migrations(from)?;
    if plan.is_empty() {
        return Ok(from);
    }
    let _lease = acquire_lease(&lease_path, now_ms)?;
    let mut version = from;
    for step in plan {
        // Structural migration hooks land here as state stores gain schemas.
        version = step.to_version;
        write_state_version(&version_path, version).map_err(|e| {
            format!(
                "state migration '{}' failed to persist version {}: {e} — \
                 run `mylobster doctor --fix`",
                step.name, version
            )
        })?;
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn migration_chain_is_contiguous_ascending() {
        let mut prev = 0;
        for step in MIGRATIONS {
            assert_eq!(step.to_version, prev + 1, "chain must be contiguous");
            prev = step.to_version;
        }
        assert_eq!(prev, CURRENT_STATE_VERSION);
    }

    #[test]
    fn plan_from_zero_runs_all() {
        let plan = plan_migrations(0).unwrap();
        assert_eq!(plan.len(), MIGRATIONS.len());
    }

    #[test]
    fn plan_from_current_is_empty() {
        assert!(plan_migrations(CURRENT_STATE_VERSION).unwrap().is_empty());
    }

    #[test]
    fn plan_from_partial_resumes() {
        let plan = plan_migrations(1).unwrap();
        assert_eq!(plan[0].to_version, 2);
    }

    #[test]
    fn newer_state_fails_closed_with_doctor_guidance() {
        let err = plan_migrations(CURRENT_STATE_VERSION + 1).unwrap_err();
        assert!(err.contains("doctor --fix"), "{err}");
        assert!(err.contains("refusing to start"), "{err}");
    }

    #[test]
    fn version_file_roundtrip_and_default_zero() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state-version.json");
        assert_eq!(read_state_version(&path), 0);
        write_state_version(&path, 2).unwrap();
        assert_eq!(read_state_version(&path), 2);
        std::fs::write(&path, b"junk").unwrap();
        assert_eq!(read_state_version(&path), 0);
    }

    #[test]
    fn lease_acquire_release_reclaim() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("migration.lease");

        // Fresh foreign lease blocks (fail closed).
        let foreign = MigrationLease {
            pid: std::process::id() + 1,
            acquired_at_ms: 1_000_000,
        };
        std::fs::write(&path, serde_json::to_vec(&foreign).unwrap()).unwrap();
        let err = acquire_lease(&path, 1_000_100).unwrap_err();
        assert!(err.contains("lease held by"), "{err}");

        // Stale foreign lease is reclaimed.
        let guard = acquire_lease(&path, 1_000_000 + LEASE_STALE_MS + 1).unwrap();
        assert!(path.exists());
        drop(guard); // release-on-exit
        assert!(!path.exists());
    }

    #[test]
    fn run_migrations_advances_and_converges() {
        let dir = TempDir::new().unwrap();
        let v = run_migrations(dir.path(), 5_000).unwrap();
        assert_eq!(v, CURRENT_STATE_VERSION);
        // Lease released after run.
        assert!(!dir.path().join("migration.lease").exists());
        // Idempotent second run.
        let v2 = run_migrations(dir.path(), 6_000).unwrap();
        assert_eq!(v2, CURRENT_STATE_VERSION);
    }
}
