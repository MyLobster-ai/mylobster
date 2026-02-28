//! Core types for the secrets management system.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ============================================================================
// Secret Reference Types
// ============================================================================

/// The kind of secret reference found in configuration values.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretRefKind {
    /// Environment variable: `$ENV{VAR_NAME}`
    Env,
    /// External command: `$EXEC{command args}`
    Exec,
    /// SOPS-encrypted file: `$SOPS{path#key}`
    Sops,
}

/// A reference to an external secret found in configuration.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretRef {
    /// The kind of secret reference.
    pub kind: SecretRefKind,
    /// The reference key (env var name, command, file#key).
    pub key: String,
    /// The config path where this ref was found (dotted notation).
    pub config_path: String,
    /// The original raw string containing the reference.
    pub raw: String,
}

/// Result of resolving a single secret reference.
#[derive(Debug, Clone)]
pub enum SecretResolution {
    /// Successfully resolved to a value.
    Resolved(String),
    /// Resolution failed with a reason.
    Failed(String),
    /// Reference was not found (env var not set, etc.).
    NotFound(String),
}

impl SecretResolution {
    pub fn is_resolved(&self) -> bool {
        matches!(self, SecretResolution::Resolved(_))
    }

    pub fn value(&self) -> Option<&str> {
        match self {
            SecretResolution::Resolved(v) => Some(v),
            _ => None,
        }
    }

    pub fn error_message(&self) -> Option<&str> {
        match self {
            SecretResolution::Failed(msg) | SecretResolution::NotFound(msg) => Some(msg),
            SecretResolution::Resolved(_) => None,
        }
    }
}

/// Status of a secret in the snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretStatus {
    /// The secret reference.
    pub secret_ref: SecretRef,
    /// Whether the secret was successfully resolved.
    pub resolved: bool,
    /// The resolved value (redacted for display).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redacted_value: Option<String>,
    /// Error message if resolution failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ============================================================================
// Provider Trait
// ============================================================================

/// Trait for secret resolution providers.
///
/// Each provider handles one `SecretRefKind` and knows how to resolve
/// references of that type to actual values.
#[async_trait]
pub trait SecretProvider: Send + Sync {
    /// The kind of references this provider handles.
    fn kind(&self) -> SecretRefKind;

    /// Display name for logging.
    fn name(&self) -> &str;

    /// Resolve a secret reference to its value.
    async fn resolve(&self, key: &str) -> SecretResolution;

    /// Check if the provider is available (e.g., SOPS binary exists).
    fn is_available(&self) -> bool {
        true
    }
}

/// A resolved set of secrets, keyed by config path.
pub type ResolvedSecrets = HashMap<String, String>;

// ============================================================================
// Ref parsing
// ============================================================================

/// Parse all secret references from a JSON config value.
///
/// Walks the entire config tree looking for string values containing
/// `$ENV{...}`, `$EXEC{...}`, or `$SOPS{...}` patterns.
pub fn parse_secret_refs(
    value: &serde_json::Value,
    prefix: &str,
) -> Vec<SecretRef> {
    let mut refs = Vec::new();
    parse_refs_recursive(value, prefix, &mut refs);
    refs
}

fn parse_refs_recursive(
    value: &serde_json::Value,
    path: &str,
    refs: &mut Vec<SecretRef>,
) {
    match value {
        serde_json::Value::String(s) => {
            // Check for secret references in the string.
            for secret_ref in extract_refs_from_string(s, path) {
                refs.push(secret_ref);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, val) in map {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                parse_refs_recursive(val, &child_path, refs);
            }
        }
        serde_json::Value::Array(arr) => {
            for (idx, val) in arr.iter().enumerate() {
                let child_path = format!("{path}[{idx}]");
                parse_refs_recursive(val, &child_path, refs);
            }
        }
        _ => {}
    }
}

/// Extract secret references from a single string value.
fn extract_refs_from_string(s: &str, config_path: &str) -> Vec<SecretRef> {
    let mut refs = Vec::new();
    let patterns: &[(&str, SecretRefKind)] = &[
        ("$ENV{", SecretRefKind::Env),
        ("$EXEC{", SecretRefKind::Exec),
        ("$SOPS{", SecretRefKind::Sops),
    ];

    for (prefix, kind) in patterns {
        let mut search_from = 0;
        while let Some(start) = s[search_from..].find(prefix) {
            let abs_start = search_from + start;
            let key_start = abs_start + prefix.len();
            if let Some(end) = s[key_start..].find('}') {
                let key = &s[key_start..key_start + end];
                if !key.is_empty() {
                    refs.push(SecretRef {
                        kind: kind.clone(),
                        key: key.to_string(),
                        config_path: config_path.to_string(),
                        raw: s[abs_start..key_start + end + 1].to_string(),
                    });
                }
                search_from = key_start + end + 1;
            } else {
                break;
            }
        }
    }

    refs
}

/// Redact a secret value for display (show first 2 and last 2 chars).
pub fn redact_secret(value: &str) -> String {
    if value.len() <= 6 {
        return "***".to_string();
    }
    format!("{}…{}", &value[..2], &value[value.len() - 2..])
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_env_ref() {
        let config = json!({
            "models": {
                "providers": {
                    "anthropic": {
                        "apiKey": "$ENV{ANTHROPIC_API_KEY}"
                    }
                }
            }
        });
        let refs = parse_secret_refs(&config, "");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, SecretRefKind::Env);
        assert_eq!(refs[0].key, "ANTHROPIC_API_KEY");
        assert_eq!(
            refs[0].config_path,
            "models.providers.anthropic.apiKey"
        );
    }

    #[test]
    fn parse_exec_ref() {
        let config = json!({
            "secret": "$EXEC{vault read -field=key secret/myapp}"
        });
        let refs = parse_secret_refs(&config, "");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, SecretRefKind::Exec);
        assert_eq!(refs[0].key, "vault read -field=key secret/myapp");
    }

    #[test]
    fn parse_sops_ref() {
        let config = json!({
            "db": {
                "password": "$SOPS{secrets.enc.yaml#db.password}"
            }
        });
        let refs = parse_secret_refs(&config, "");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, SecretRefKind::Sops);
        assert_eq!(refs[0].key, "secrets.enc.yaml#db.password");
    }

    #[test]
    fn parse_multiple_refs_in_one_string() {
        let config = json!({
            "dsn": "postgres://$ENV{DB_USER}:$ENV{DB_PASS}@localhost/db"
        });
        let refs = parse_secret_refs(&config, "");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].key, "DB_USER");
        assert_eq!(refs[1].key, "DB_PASS");
    }

    #[test]
    fn parse_refs_in_arrays() {
        let config = json!({
            "keys": ["$ENV{KEY_1}", "$ENV{KEY_2}"]
        });
        let refs = parse_secret_refs(&config, "");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].config_path, "keys[0]");
        assert_eq!(refs[1].config_path, "keys[1]");
    }

    #[test]
    fn parse_no_refs() {
        let config = json!({"plain": "value", "number": 42});
        let refs = parse_secret_refs(&config, "");
        assert!(refs.is_empty());
    }

    #[test]
    fn parse_unclosed_ref_ignored() {
        let config = json!({"broken": "$ENV{UNCLOSED"});
        let refs = parse_secret_refs(&config, "");
        assert!(refs.is_empty());
    }

    #[test]
    fn parse_empty_ref_ignored() {
        let config = json!({"empty": "$ENV{}"});
        let refs = parse_secret_refs(&config, "");
        assert!(refs.is_empty());
    }

    #[test]
    fn redact_short_value() {
        assert_eq!(redact_secret("abc"), "***");
    }

    #[test]
    fn redact_long_value() {
        let redacted = redact_secret("sk-ant-api03-1234567890");
        assert!(redacted.starts_with("sk"));
        assert!(redacted.ends_with("90"));
        assert!(redacted.contains('…'));
    }

    #[test]
    fn secret_resolution_helpers() {
        let ok = SecretResolution::Resolved("value".into());
        assert!(ok.is_resolved());
        assert_eq!(ok.value(), Some("value"));
        assert!(ok.error_message().is_none());

        let fail = SecretResolution::Failed("timeout".into());
        assert!(!fail.is_resolved());
        assert!(fail.value().is_none());
        assert_eq!(fail.error_message(), Some("timeout"));
    }
}
