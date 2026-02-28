//! Three-phase secrets workflow: audit → configure → apply.
//!
//! The snapshot captures the state of all secret refs at a point in time
//! and supports atomic application to the runtime config.

use super::types::{SecretProvider, SecretRef, SecretStatus};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

// ============================================================================
// Snapshot
// ============================================================================

/// A point-in-time snapshot of all secret resolutions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretSnapshot {
    /// Timestamp when the snapshot was created (ms since epoch).
    pub created_at: u64,
    /// Status of each secret reference.
    pub statuses: Vec<SecretStatus>,
    /// Number of successfully resolved secrets.
    pub resolved_count: usize,
    /// Number of failed resolutions.
    pub failed_count: usize,
    /// Whether all required secrets were resolved.
    pub all_required_resolved: bool,
}

impl SecretSnapshot {
    /// Create a snapshot from a list of statuses.
    pub fn from_statuses(
        statuses: Vec<SecretStatus>,
        required_paths: &[&str],
    ) -> Self {
        let resolved_count = statuses.iter().filter(|s| s.resolved).count();
        let failed_count = statuses.len() - resolved_count;

        let unresolved_required = super::resolver::check_unresolved_required(
            &statuses,
            required_paths,
        );

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        Self {
            created_at: now,
            statuses,
            resolved_count,
            failed_count,
            all_required_resolved: unresolved_required.is_empty(),
        }
    }

    /// Get all unresolved secret refs.
    pub fn unresolved(&self) -> Vec<&SecretRef> {
        self.statuses
            .iter()
            .filter(|s| !s.resolved)
            .map(|s| &s.secret_ref)
            .collect()
    }
}

// ============================================================================
// Workflow
// ============================================================================

/// Three-phase workflow state machine for secrets management.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowPhase {
    /// Phase 1: Audit — scan config for refs, check provider availability.
    Audit,
    /// Phase 2: Configure — resolve refs through providers.
    Configure,
    /// Phase 3: Apply — inject resolved values into runtime config.
    Apply,
}

/// Manages the three-phase secrets workflow.
pub struct SecretWorkflow {
    phase: WorkflowPhase,
    snapshot: Option<SecretSnapshot>,
    resolved_config: Option<serde_json::Value>,
}

impl SecretWorkflow {
    pub fn new() -> Self {
        Self {
            phase: WorkflowPhase::Audit,
            snapshot: None,
            resolved_config: None,
        }
    }

    /// Get the current workflow phase.
    pub fn phase(&self) -> WorkflowPhase {
        self.phase
    }

    /// Get the current snapshot, if available.
    pub fn snapshot(&self) -> Option<&SecretSnapshot> {
        self.snapshot.as_ref()
    }

    /// Phase 1: Audit — scan config for secret references.
    ///
    /// Returns the list of found refs without resolving them.
    pub fn audit(&mut self, config: &serde_json::Value) -> Vec<SecretRef> {
        let refs = super::types::parse_secret_refs(config, "");
        info!(
            "Secrets audit: found {} reference(s) in config",
            refs.len()
        );
        self.phase = WorkflowPhase::Configure;
        refs
    }

    /// Phase 2: Configure — resolve all secret references.
    ///
    /// Calls each provider to resolve its refs and produces a snapshot.
    pub async fn configure(
        &mut self,
        config: &serde_json::Value,
        providers: &[Box<dyn SecretProvider>],
        required_paths: &[&str],
    ) -> SecretSnapshot {
        let (resolved_config, statuses) =
            super::resolver::resolve_all_secrets(config, providers).await;

        let snapshot =
            SecretSnapshot::from_statuses(statuses, required_paths);

        if !snapshot.all_required_resolved {
            warn!(
                "Not all required secrets resolved: {} of {} failed",
                snapshot.failed_count,
                snapshot.statuses.len()
            );
        }

        self.snapshot = Some(snapshot.clone());
        self.resolved_config = Some(resolved_config);
        self.phase = WorkflowPhase::Apply;

        snapshot
    }

    /// Phase 3: Apply — return the config with all resolved secrets injected.
    ///
    /// Returns `None` if configure phase hasn't completed or required secrets
    /// are missing (fail-fast).
    pub fn apply(&mut self, fail_on_missing: bool) -> Option<serde_json::Value> {
        if self.phase != WorkflowPhase::Apply {
            warn!("Cannot apply secrets: not in Apply phase (current: {:?})", self.phase);
            return None;
        }

        if fail_on_missing {
            if let Some(ref snapshot) = self.snapshot {
                if !snapshot.all_required_resolved {
                    warn!(
                        "Refusing to apply config: {} required secret(s) unresolved",
                        snapshot.failed_count
                    );
                    return None;
                }
            }
        }

        let config = self.resolved_config.take();
        self.phase = WorkflowPhase::Audit; // Reset for next cycle.
        config
    }

    /// Reset the workflow to the Audit phase.
    pub fn reset(&mut self) {
        self.phase = WorkflowPhase::Audit;
        self.snapshot = None;
        self.resolved_config = None;
    }
}

impl Default for SecretWorkflow {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::types::SecretRefKind;

    #[test]
    fn snapshot_from_statuses() {
        let statuses = vec![
            SecretStatus {
                secret_ref: SecretRef {
                    kind: SecretRefKind::Env,
                    key: "A".into(),
                    config_path: "models.key".into(),
                    raw: "$ENV{A}".into(),
                },
                resolved: true,
                redacted_value: Some("xx…yy".into()),
                error: None,
            },
            SecretStatus {
                secret_ref: SecretRef {
                    kind: SecretRefKind::Env,
                    key: "B".into(),
                    config_path: "channels.token".into(),
                    raw: "$ENV{B}".into(),
                },
                resolved: false,
                redacted_value: None,
                error: Some("not set".into()),
            },
        ];

        let snapshot = SecretSnapshot::from_statuses(statuses, &["models"]);
        assert_eq!(snapshot.resolved_count, 1);
        assert_eq!(snapshot.failed_count, 1);
        assert!(snapshot.all_required_resolved);
    }

    #[test]
    fn snapshot_required_failure() {
        let statuses = vec![SecretStatus {
            secret_ref: SecretRef {
                kind: SecretRefKind::Env,
                key: "KEY".into(),
                config_path: "models.apiKey".into(),
                raw: "$ENV{KEY}".into(),
            },
            resolved: false,
            redacted_value: None,
            error: Some("not set".into()),
        }];

        let snapshot = SecretSnapshot::from_statuses(statuses, &["models"]);
        assert!(!snapshot.all_required_resolved);
    }

    #[test]
    fn workflow_phase_progression() {
        let mut wf = SecretWorkflow::new();
        assert_eq!(wf.phase(), WorkflowPhase::Audit);

        let config = serde_json::json!({"key": "plain"});
        let _ = wf.audit(&config);
        assert_eq!(wf.phase(), WorkflowPhase::Configure);
    }

    #[test]
    fn workflow_apply_without_configure_fails() {
        let mut wf = SecretWorkflow::new();
        assert!(wf.apply(true).is_none());
    }

    #[test]
    fn workflow_reset() {
        let mut wf = SecretWorkflow::new();
        let config = serde_json::json!({});
        let _ = wf.audit(&config);
        assert_eq!(wf.phase(), WorkflowPhase::Configure);

        wf.reset();
        assert_eq!(wf.phase(), WorkflowPhase::Audit);
    }

    #[test]
    fn snapshot_unresolved() {
        let statuses = vec![
            SecretStatus {
                secret_ref: SecretRef {
                    kind: SecretRefKind::Env,
                    key: "OK".into(),
                    config_path: "a".into(),
                    raw: "$ENV{OK}".into(),
                },
                resolved: true,
                redacted_value: Some("xx".into()),
                error: None,
            },
            SecretStatus {
                secret_ref: SecretRef {
                    kind: SecretRefKind::Env,
                    key: "FAIL".into(),
                    config_path: "b".into(),
                    raw: "$ENV{FAIL}".into(),
                },
                resolved: false,
                redacted_value: None,
                error: Some("not set".into()),
            },
        ];

        let snapshot = SecretSnapshot::from_statuses(statuses, &[]);
        let unresolved = snapshot.unresolved();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].key, "FAIL");
    }
}
