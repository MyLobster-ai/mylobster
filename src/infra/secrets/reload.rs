//! Hot-reload integration for secrets.
//!
//! Provides the `secrets.reload` RPC method that re-resolves all secret
//! references and applies them to the running config.

use super::env_provider::EnvSecretProvider;
use super::exec_provider::ExecSecretProvider;
use super::sops_provider::SopsSecretProvider;
use super::types::SecretProvider;
use super::SecretWorkflow;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Result of a secrets reload operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretsReloadResult {
    /// Whether the reload was successful.
    pub ok: bool,
    /// Number of secrets resolved.
    pub resolved_count: usize,
    /// Number of secrets that failed to resolve.
    pub failed_count: usize,
    /// Whether the config was updated.
    pub config_updated: bool,
    /// Error message if reload failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// List of unresolved secret keys.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unresolved_keys: Vec<String>,
}

/// Build the default set of secret providers.
pub fn default_providers(base_dir: Option<String>) -> Vec<Box<dyn SecretProvider>> {
    let mut providers: Vec<Box<dyn SecretProvider>> = vec![
        Box::new(EnvSecretProvider::new()),
        Box::new(ExecSecretProvider::new(base_dir.clone())),
    ];

    let sops = SopsSecretProvider::new(base_dir);
    if sops.is_available() {
        providers.push(Box::new(sops));
    }

    providers
}

/// Perform a secrets reload cycle on a config value.
///
/// This runs all three phases: audit → configure → apply.
pub async fn reload_secrets(
    config: &serde_json::Value,
    base_dir: Option<String>,
    required_paths: &[&str],
    fail_on_missing: bool,
) -> (Option<serde_json::Value>, SecretsReloadResult) {
    let providers = default_providers(base_dir);
    let mut workflow = SecretWorkflow::new();

    // Phase 1: Audit
    let refs = workflow.audit(config);
    if refs.is_empty() {
        info!("No secret references found — nothing to reload");
        return (
            Some(config.clone()),
            SecretsReloadResult {
                ok: true,
                resolved_count: 0,
                failed_count: 0,
                config_updated: false,
                error: None,
                unresolved_keys: vec![],
            },
        );
    }

    // Phase 2: Configure
    let snapshot = workflow
        .configure(config, &providers, required_paths)
        .await;

    let unresolved_keys: Vec<String> = snapshot
        .unresolved()
        .iter()
        .map(|r| r.key.clone())
        .collect();

    // Phase 3: Apply
    match workflow.apply(fail_on_missing) {
        Some(resolved_config) => (
            Some(resolved_config),
            SecretsReloadResult {
                ok: true,
                resolved_count: snapshot.resolved_count,
                failed_count: snapshot.failed_count,
                config_updated: true,
                error: None,
                unresolved_keys,
            },
        ),
        None => {
            warn!("Secrets reload failed: required secrets unresolved");
            (
                None,
                SecretsReloadResult {
                    ok: false,
                    resolved_count: snapshot.resolved_count,
                    failed_count: snapshot.failed_count,
                    config_updated: false,
                    error: Some("Required secrets unresolved".into()),
                    unresolved_keys,
                },
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn reload_no_refs() {
        let config = json!({"plain": "value"});
        let (result, status) =
            reload_secrets(&config, None, &[], false).await;
        assert!(result.is_some());
        assert!(status.ok);
        assert_eq!(status.resolved_count, 0);
        assert!(!status.config_updated);
    }

    #[tokio::test]
    async fn reload_with_env_ref() {
        // Use a real env var that exists
        let config = json!({
            "path": "$ENV{PATH}"
        });
        let (result, status) =
            reload_secrets(&config, None, &[], false).await;
        assert!(result.is_some());
        assert!(status.ok);
        assert_eq!(status.resolved_count, 1);
        assert!(status.config_updated);

        // The resolved config should have the actual PATH value
        let resolved = result.unwrap();
        assert_ne!(resolved["path"], "$ENV{PATH}");
    }

    #[tokio::test]
    async fn reload_fail_on_missing() {
        let config = json!({
            "models": {
                "apiKey": "$ENV{MYLOBSTER_NONEXISTENT_KEY_XYZ}"
            }
        });
        let (result, status) =
            reload_secrets(&config, None, &["models"], true).await;
        assert!(result.is_none());
        assert!(!status.ok);
        assert_eq!(status.failed_count, 1);
    }
}
