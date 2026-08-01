//! Boot-outcome ledger, crash-loop safe mode, and supervised exit-code
//! classification (v2026.7.1 + v2026.5.2 parity).
//!
//! - Persist boot outcomes; after repeated unclean starts the gateway enters
//!   control-plane-safe mode (transports/providers held until recovery).
//! - Exit `EX_CONFIG` (sysexits 78) on fatal config errors and on supervised
//!   lock / EADDRINUSE conflicts so `Restart=always` supervisors stop
//!   hot-looping (v2026.5.2: "Gateway/systemd: exit sysexits 78").

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// sysexits(3) EX_CONFIG.
pub const EX_CONFIG: i32 = 78;

/// Unclean boots (consecutive) required to trigger control-plane-safe mode.
pub const SAFE_MODE_UNCLEAN_THRESHOLD: usize = 3;

/// Maximum ledger entries retained.
pub const BOOT_LEDGER_MAX_ENTRIES: usize = 32;

// ============================================================================
// Exit-code classification
// ============================================================================

/// Whether the gateway is running under a service supervisor
/// (systemd/launchd/k8s style `Restart=always` loops).
pub fn is_supervised() -> bool {
    std::env::var_os("INVOCATION_ID").is_some() // systemd
        || std::env::var_os("MYLOBSTER_SUPERVISED").is_some()
        || std::env::var_os("KUBERNETES_SERVICE_HOST").is_some()
}

/// Whether an error chain indicates an address-in-use / gateway-lock
/// conflict — i.e. another gateway already owns the port or lock.
pub fn is_lock_or_addr_conflict(error_chain: &str) -> bool {
    let lower = error_chain.to_ascii_lowercase();
    lower.contains("eaddrinuse")
        || lower.contains("address already in use")
        || lower.contains("address in use")
        || lower.contains("gateway lock")
        || lower.contains("lock held by")
        || lower.contains("already running")
}

/// Whether an error chain indicates a fatal configuration error.
pub fn is_fatal_config_error(error_chain: &str) -> bool {
    let lower = error_chain.to_ascii_lowercase();
    lower.contains("configuration validation failed")
        || lower.contains("invalid config")
        || lower.contains("config file")
            && (lower.contains("parse") || lower.contains("symlink") || lower.contains("hard links"))
}

/// Classify a fatal gateway-start error into a process exit code.
///
/// * supervised + lock/EADDRINUSE → 78 (stop `Restart=always` loops)
/// * fatal config error → 78 (`EX_CONFIG`) regardless of supervision
/// * everything else → 1
pub fn classify_fatal_exit(error_chain: &str, supervised: bool) -> i32 {
    if is_fatal_config_error(error_chain) {
        return EX_CONFIG;
    }
    if supervised && is_lock_or_addr_conflict(error_chain) {
        return EX_CONFIG;
    }
    1
}

// ============================================================================
// Boot-outcome ledger
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BootOutcome {
    /// Boot reached readiness and shut down cleanly.
    Clean,
    /// Process died before marking a clean shutdown.
    Unclean,
    /// Boot aborted on a fatal config error.
    ConfigError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootRecord {
    pub started_at_ms: u64,
    pub outcome: BootOutcome,
    pub version: String,
}

/// Ledger file location.
pub fn boot_ledger_path() -> PathBuf {
    if let Ok(dir) = std::env::var("MYLOBSTER_STATE_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir).join("boot-ledger.json");
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mylobster")
        .join("state")
        .join("boot-ledger.json")
}

pub fn read_ledger(path: &Path) -> Vec<BootRecord> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn write_ledger(path: &Path, records: &[BootRecord]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bounded: Vec<&BootRecord> = records
        .iter()
        .rev()
        .take(BOOT_LEDGER_MAX_ENTRIES)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&bounded)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Record a boot start. The new record is written as `Unclean` immediately;
/// `mark_clean_exit` upgrades it on graceful shutdown, so a crash leaves the
/// unclean marker behind.
pub fn record_boot_start(path: &Path, version: &str, now_ms: u64) -> anyhow::Result<()> {
    let mut records = read_ledger(path);
    records.push(BootRecord {
        started_at_ms: now_ms,
        outcome: BootOutcome::Unclean,
        version: version.to_string(),
    });
    write_ledger(path, &records)
}

/// Mark the latest boot record as a clean exit.
pub fn mark_clean_exit(path: &Path) -> anyhow::Result<()> {
    let mut records = read_ledger(path);
    if let Some(last) = records.last_mut() {
        last.outcome = BootOutcome::Clean;
    }
    write_ledger(path, &records)
}

/// Mark the latest boot record as a config error.
pub fn mark_config_error(path: &Path) -> anyhow::Result<()> {
    let mut records = read_ledger(path);
    if let Some(last) = records.last_mut() {
        last.outcome = BootOutcome::ConfigError;
    }
    write_ledger(path, &records)
}

/// Assess whether the gateway should start in control-plane-safe mode:
/// the most recent `SAFE_MODE_UNCLEAN_THRESHOLD` completed boots (i.e.
/// excluding the record for the boot currently in progress) were all unclean.
pub fn assess_safe_mode(records: &[BootRecord]) -> bool {
    let completed: Vec<&BootRecord> = records.iter().collect();
    if completed.len() < SAFE_MODE_UNCLEAN_THRESHOLD {
        return false;
    }
    completed
        .iter()
        .rev()
        .take(SAFE_MODE_UNCLEAN_THRESHOLD)
        .all(|r| r.outcome == BootOutcome::Unclean)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn rec(outcome: BootOutcome) -> BootRecord {
        BootRecord {
            started_at_ms: 0,
            outcome,
            version: "test".to_string(),
        }
    }

    // ---- exit classification ----

    #[test]
    fn supervised_addr_in_use_exits_78() {
        assert_eq!(
            classify_fatal_exit("Error: Address already in use (os error 48)", true),
            EX_CONFIG
        );
        assert_eq!(classify_fatal_exit("bind failed: EADDRINUSE", true), EX_CONFIG);
        assert_eq!(
            classify_fatal_exit("gateway lock held by pid 1234", true),
            EX_CONFIG
        );
    }

    #[test]
    fn unsupervised_addr_in_use_exits_1() {
        assert_eq!(
            classify_fatal_exit("Address already in use (os error 48)", false),
            1
        );
    }

    #[test]
    fn config_errors_exit_78_regardless() {
        assert_eq!(
            classify_fatal_exit("Configuration validation failed:\ngateway.port: bad", false),
            EX_CONFIG
        );
        assert_eq!(
            classify_fatal_exit("Configuration validation failed: x", true),
            EX_CONFIG
        );
    }

    #[test]
    fn other_errors_exit_1() {
        assert_eq!(classify_fatal_exit("some transient failure", true), 1);
        assert_eq!(classify_fatal_exit("connection refused", false), 1);
    }

    // ---- safe mode ----

    #[test]
    fn safe_mode_requires_three_consecutive_unclean() {
        assert!(!assess_safe_mode(&[]));
        assert!(!assess_safe_mode(&[rec(BootOutcome::Unclean), rec(BootOutcome::Unclean)]));
        assert!(assess_safe_mode(&[
            rec(BootOutcome::Unclean),
            rec(BootOutcome::Unclean),
            rec(BootOutcome::Unclean),
        ]));
        // Clean boot breaks the streak
        assert!(!assess_safe_mode(&[
            rec(BootOutcome::Unclean),
            rec(BootOutcome::Clean),
            rec(BootOutcome::Unclean),
        ]));
        // Config errors do not count toward crash-loop detection
        assert!(!assess_safe_mode(&[
            rec(BootOutcome::Unclean),
            rec(BootOutcome::Unclean),
            rec(BootOutcome::ConfigError),
        ]));
    }

    // ---- ledger persistence ----

    #[test]
    fn ledger_lifecycle_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("boot-ledger.json");

        record_boot_start(&path, "1.0", 100).unwrap();
        let records = read_ledger(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, BootOutcome::Unclean);

        mark_clean_exit(&path).unwrap();
        assert_eq!(read_ledger(&path)[0].outcome, BootOutcome::Clean);

        record_boot_start(&path, "1.0", 200).unwrap();
        mark_config_error(&path).unwrap();
        let records = read_ledger(&path);
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].outcome, BootOutcome::ConfigError);
    }

    #[test]
    fn ledger_is_bounded() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ledger.json");
        for i in 0..(BOOT_LEDGER_MAX_ENTRIES + 10) {
            record_boot_start(&path, "1.0", i as u64).unwrap();
        }
        let records = read_ledger(&path);
        assert_eq!(records.len(), BOOT_LEDGER_MAX_ENTRIES);
        // Oldest entries were dropped; latest retained.
        assert_eq!(
            records.last().unwrap().started_at_ms,
            (BOOT_LEDGER_MAX_ENTRIES + 9) as u64
        );
    }

    #[test]
    fn unreadable_ledger_is_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nope.json");
        assert!(read_ledger(&path).is_empty());
        std::fs::write(&path, b"garbage").unwrap();
        assert!(read_ledger(&path).is_empty());
    }
}
