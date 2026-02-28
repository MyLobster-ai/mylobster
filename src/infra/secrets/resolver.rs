//! Config tree walker and secret reference resolver.
//!
//! Walks a JSON config tree, finds all `$ENV{...}`, `$EXEC{...}`, `$SOPS{...}`
//! references, resolves them through their respective providers, and produces
//! a config with all references replaced by their resolved values.

use super::types::{
    parse_secret_refs, SecretProvider, SecretRef, SecretRefKind,
    SecretResolution, SecretStatus,
};
use std::collections::HashMap;
use tracing::{debug, error, info, warn};

/// Resolve all secret references in a config value.
///
/// Returns the config with all resolvable refs replaced, plus a status
/// report for each ref found.
pub async fn resolve_all_secrets(
    config: &serde_json::Value,
    providers: &[Box<dyn SecretProvider>],
) -> (serde_json::Value, Vec<SecretStatus>) {
    // 1. Scan for all secret refs.
    let refs = parse_secret_refs(config, "");
    info!("Found {} secret reference(s) in config", refs.len());

    if refs.is_empty() {
        return (config.clone(), vec![]);
    }

    // 2. Build provider lookup.
    let provider_map: HashMap<SecretRefKind, &dyn SecretProvider> = providers
        .iter()
        .map(|p| (p.kind(), p.as_ref()))
        .collect();

    // 3. Resolve each ref.
    let mut resolved_values: HashMap<String, String> = HashMap::new();
    let mut statuses: Vec<SecretStatus> = Vec::new();

    for secret_ref in &refs {
        let resolution = resolve_single_ref(secret_ref, &provider_map).await;
        let status = SecretStatus {
            secret_ref: secret_ref.clone(),
            resolved: resolution.is_resolved(),
            redacted_value: resolution
                .value()
                .map(super::types::redact_secret),
            error: resolution.error_message().map(String::from),
        };

        if let Some(value) = resolution.value() {
            resolved_values.insert(secret_ref.raw.clone(), value.to_string());
        }

        statuses.push(status);
    }

    // 4. Apply resolved values to config.
    let resolved_config = apply_resolved_secrets(config, &resolved_values);

    let resolved_count = statuses.iter().filter(|s| s.resolved).count();
    let failed_count = statuses.len() - resolved_count;
    info!(
        "Secret resolution complete: {resolved_count} resolved, {failed_count} failed"
    );

    (resolved_config, statuses)
}

/// Resolve a single secret reference through its provider.
async fn resolve_single_ref(
    secret_ref: &SecretRef,
    providers: &HashMap<SecretRefKind, &dyn SecretProvider>,
) -> SecretResolution {
    let provider = match providers.get(&secret_ref.kind) {
        Some(p) => *p,
        None => {
            return SecretResolution::Failed(format!(
                "No provider registered for {:?} refs",
                secret_ref.kind
            ));
        }
    };

    if !provider.is_available() {
        return SecretResolution::Failed(format!(
            "Provider '{}' is not available",
            provider.name()
        ));
    }

    debug!(
        "Resolving {:?} ref '{}' via provider '{}'",
        secret_ref.kind,
        secret_ref.key,
        provider.name()
    );

    let result = provider.resolve(&secret_ref.key).await;

    match &result {
        SecretResolution::Resolved(_) => {
            debug!(
                "Resolved {:?} ref '{}' at config path '{}'",
                secret_ref.kind, secret_ref.key, secret_ref.config_path
            );
        }
        SecretResolution::Failed(msg) => {
            error!(
                "Failed to resolve {:?} ref '{}': {}",
                secret_ref.kind, secret_ref.key, msg
            );
        }
        SecretResolution::NotFound(msg) => {
            warn!(
                "Secret ref {:?} '{}' not found: {}",
                secret_ref.kind, secret_ref.key, msg
            );
        }
    }

    result
}

/// Apply resolved secret values to a config tree.
///
/// Walks the tree and replaces occurrences of raw ref strings with their
/// resolved values. If a string contains multiple refs (e.g., a DSN),
/// each ref is replaced inline.
fn apply_resolved_secrets(
    value: &serde_json::Value,
    resolved: &HashMap<String, String>,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            let mut result = s.clone();
            for (raw, resolved_value) in resolved {
                result = result.replace(raw, resolved_value);
            }
            serde_json::Value::String(result)
        }
        serde_json::Value::Object(map) => {
            let new_map: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), apply_resolved_secrets(v, resolved)))
                .collect();
            serde_json::Value::Object(new_map)
        }
        serde_json::Value::Array(arr) => {
            let new_arr: Vec<serde_json::Value> = arr
                .iter()
                .map(|v| apply_resolved_secrets(v, resolved))
                .collect();
            serde_json::Value::Array(new_arr)
        }
        other => other.clone(),
    }
}

/// Check if any required secret refs are unresolved. Returns the list of
/// unresolved refs for fail-fast behavior.
pub fn check_unresolved_required(
    statuses: &[SecretStatus],
    required_paths: &[&str],
) -> Vec<SecretRef> {
    statuses
        .iter()
        .filter(|s| !s.resolved)
        .filter(|s| {
            required_paths
                .iter()
                .any(|req| s.secret_ref.config_path.starts_with(req))
        })
        .map(|s| s.secret_ref.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn apply_single_ref() {
        let config = json!({
            "key": "$ENV{MY_SECRET}"
        });
        let mut resolved = HashMap::new();
        resolved.insert("$ENV{MY_SECRET}".to_string(), "actual-value".to_string());

        let result = apply_resolved_secrets(&config, &resolved);
        assert_eq!(result["key"], "actual-value");
    }

    #[test]
    fn apply_inline_refs() {
        let config = json!({
            "dsn": "postgres://$ENV{USER}:$ENV{PASS}@localhost/db"
        });
        let mut resolved = HashMap::new();
        resolved.insert("$ENV{USER}".to_string(), "admin".to_string());
        resolved.insert("$ENV{PASS}".to_string(), "secret".to_string());

        let result = apply_resolved_secrets(&config, &resolved);
        assert_eq!(result["dsn"], "postgres://admin:secret@localhost/db");
    }

    #[test]
    fn apply_nested_refs() {
        let config = json!({
            "models": {
                "providers": {
                    "anthropic": {
                        "apiKey": "$ENV{API_KEY}"
                    }
                }
            }
        });
        let mut resolved = HashMap::new();
        resolved.insert("$ENV{API_KEY}".to_string(), "sk-ant-xxx".to_string());

        let result = apply_resolved_secrets(&config, &resolved);
        assert_eq!(
            result["models"]["providers"]["anthropic"]["apiKey"],
            "sk-ant-xxx"
        );
    }

    #[test]
    fn apply_unresolved_left_unchanged() {
        let config = json!({"key": "$ENV{MISSING}"});
        let resolved = HashMap::new();

        let result = apply_resolved_secrets(&config, &resolved);
        assert_eq!(result["key"], "$ENV{MISSING}");
    }

    #[test]
    fn apply_non_string_unchanged() {
        let config = json!({"port": 8080, "enabled": true});
        let resolved = HashMap::new();

        let result = apply_resolved_secrets(&config, &resolved);
        assert_eq!(result["port"], 8080);
        assert_eq!(result["enabled"], true);
    }

    #[test]
    fn check_unresolved_required_empty_when_all_resolved() {
        let statuses = vec![SecretStatus {
            secret_ref: SecretRef {
                kind: SecretRefKind::Env,
                key: "KEY".into(),
                config_path: "models.apiKey".into(),
                raw: "$ENV{KEY}".into(),
            },
            resolved: true,
            redacted_value: Some("sk…xx".into()),
            error: None,
        }];
        let unresolved = check_unresolved_required(&statuses, &["models"]);
        assert!(unresolved.is_empty());
    }

    #[test]
    fn check_unresolved_required_finds_failures() {
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
        let unresolved = check_unresolved_required(&statuses, &["models"]);
        assert_eq!(unresolved.len(), 1);
    }

    #[test]
    fn check_unresolved_filters_by_path() {
        let statuses = vec![SecretStatus {
            secret_ref: SecretRef {
                kind: SecretRefKind::Env,
                key: "KEY".into(),
                config_path: "channels.telegram.token".into(),
                raw: "$ENV{KEY}".into(),
            },
            resolved: false,
            redacted_value: None,
            error: Some("not set".into()),
        }];
        // Only require "models" path — channels failure should not be reported.
        let unresolved = check_unresolved_required(&statuses, &["models"]);
        assert!(unresolved.is_empty());
    }
}
