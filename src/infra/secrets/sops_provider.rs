//! SOPS-encrypted file secret provider.
//!
//! Resolves `$SOPS{path#key}` references by decrypting the file with
//! Mozilla SOPS and extracting the specified key.

use super::types::{SecretProvider, SecretRefKind, SecretResolution};
use async_trait::async_trait;
use std::time::Duration;
use tokio::process::Command;
use tracing::{debug, warn};

/// Timeout for SOPS decryption.
const SOPS_TIMEOUT_SECS: u64 = 30;

/// Resolves secrets from SOPS-encrypted files.
pub struct SopsSecretProvider {
    /// Base directory for resolving relative SOPS file paths.
    base_dir: Option<String>,
}

impl SopsSecretProvider {
    pub fn new(base_dir: Option<String>) -> Self {
        Self { base_dir }
    }

    /// Parse a SOPS reference key into (file_path, json_key).
    ///
    /// Format: `path/to/file.enc.yaml#dotted.key.path`
    fn parse_ref(key: &str) -> Option<(&str, &str)> {
        let hash_pos = key.find('#')?;
        let file_path = &key[..hash_pos];
        let json_key = &key[hash_pos + 1..];

        if file_path.is_empty() || json_key.is_empty() {
            return None;
        }

        Some((file_path, json_key))
    }
}

#[async_trait]
impl SecretProvider for SopsSecretProvider {
    fn kind(&self) -> SecretRefKind {
        SecretRefKind::Sops
    }

    fn name(&self) -> &str {
        "sops"
    }

    fn is_available(&self) -> bool {
        // Check if the `sops` binary exists on PATH.
        which_sops().is_some()
    }

    async fn resolve(&self, key: &str) -> SecretResolution {
        let (file_path, json_key) = match Self::parse_ref(key) {
            Some(parsed) => parsed,
            None => {
                return SecretResolution::Failed(format!(
                    "Invalid SOPS reference '{key}': expected 'path#key' format"
                ));
            }
        };

        // Resolve relative paths against base_dir.
        let full_path = if std::path::Path::new(file_path).is_absolute() {
            std::path::PathBuf::from(file_path)
        } else {
            match &self.base_dir {
                Some(base) => std::path::PathBuf::from(base).join(file_path),
                None => std::path::PathBuf::from(file_path),
            }
        };

        if !full_path.exists() {
            return SecretResolution::NotFound(format!(
                "SOPS file '{}' not found",
                full_path.display()
            ));
        }

        let sops_bin = match which_sops() {
            Some(path) => path,
            None => {
                return SecretResolution::Failed(
                    "SOPS binary not found on PATH".to_string()
                );
            }
        };

        debug!(
            "Decrypting SOPS file '{}' for key '{}'",
            full_path.display(),
            json_key
        );

        // Run sops decrypt.
        let mut cmd = Command::new(&sops_bin);
        cmd.args(["-d", &full_path.to_string_lossy()]);
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let result = tokio::time::timeout(
            Duration::from_secs(SOPS_TIMEOUT_SECS),
            cmd.output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return SecretResolution::Failed(format!(
                        "SOPS decrypt failed: {}",
                        stderr.trim()
                    ));
                }

                // Parse the decrypted output as JSON/YAML.
                let content = String::from_utf8_lossy(&output.stdout);
                extract_key_from_content(&content, json_key, file_path)
            }
            Ok(Err(e)) => SecretResolution::Failed(format!(
                "Failed to run SOPS: {e}"
            )),
            Err(_) => SecretResolution::Failed(format!(
                "SOPS decrypt timed out after {SOPS_TIMEOUT_SECS}s"
            )),
        }
    }
}

/// Find the `sops` binary on PATH.
fn which_sops() -> Option<String> {
    // Check common locations.
    for name in &["sops", "/usr/local/bin/sops", "/usr/bin/sops"] {
        if std::process::Command::new(name)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
        {
            return Some(name.to_string());
        }
    }
    None
}

/// Extract a dotted key path from decrypted SOPS content.
///
/// Supports both JSON and YAML formats. The key path uses dots as
/// separators: `db.password` extracts `{"db": {"password": "value"}}`.
fn extract_key_from_content(
    content: &str,
    dotted_key: &str,
    file_path: &str,
) -> SecretResolution {
    // Try JSON first, then YAML.
    let value: serde_json::Value = if let Ok(v) = serde_json::from_str(content) {
        v
    } else if let Ok(v) = serde_yaml::from_str(content) {
        v
    } else {
        return SecretResolution::Failed(format!(
            "Cannot parse decrypted SOPS file '{file_path}' as JSON or YAML"
        ));
    };

    // Walk the dotted key path.
    let mut current = &value;
    for segment in dotted_key.split('.') {
        match current {
            serde_json::Value::Object(map) => {
                current = match map.get(segment) {
                    Some(v) => v,
                    None => {
                        return SecretResolution::NotFound(format!(
                            "Key '{dotted_key}' not found in SOPS file '{file_path}' \
                             (missing segment '{segment}')"
                        ));
                    }
                };
            }
            _ => {
                return SecretResolution::Failed(format!(
                    "Key path '{dotted_key}' in SOPS file '{file_path}': \
                     segment '{segment}' is not an object"
                ));
            }
        }
    }

    // Extract the leaf value as a string.
    match current {
        serde_json::Value::String(s) => SecretResolution::Resolved(s.clone()),
        serde_json::Value::Number(n) => {
            SecretResolution::Resolved(n.to_string())
        }
        serde_json::Value::Bool(b) => {
            SecretResolution::Resolved(b.to_string())
        }
        serde_json::Value::Null => SecretResolution::NotFound(format!(
            "Key '{dotted_key}' in SOPS file '{file_path}' is null"
        )),
        _ => SecretResolution::Failed(format!(
            "Key '{dotted_key}' in SOPS file '{file_path}' is not a scalar value"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sops_ref_valid() {
        let (file, key) = SopsSecretProvider::parse_ref("secrets.enc.yaml#db.password").unwrap();
        assert_eq!(file, "secrets.enc.yaml");
        assert_eq!(key, "db.password");
    }

    #[test]
    fn parse_sops_ref_no_hash() {
        assert!(SopsSecretProvider::parse_ref("no-hash-here").is_none());
    }

    #[test]
    fn parse_sops_ref_empty_parts() {
        assert!(SopsSecretProvider::parse_ref("#key").is_none());
        assert!(SopsSecretProvider::parse_ref("file#").is_none());
    }

    #[test]
    fn extract_key_json() {
        let content = r#"{"db": {"password": "secret123"}}"#;
        let result = extract_key_from_content(content, "db.password", "test.json");
        assert!(result.is_resolved());
        assert_eq!(result.value(), Some("secret123"));
    }

    #[test]
    fn extract_key_yaml() {
        let content = "db:\n  password: yaml-secret\n";
        let result = extract_key_from_content(content, "db.password", "test.yaml");
        assert!(result.is_resolved());
        assert_eq!(result.value(), Some("yaml-secret"));
    }

    #[test]
    fn extract_key_missing() {
        let content = r#"{"db": {"host": "localhost"}}"#;
        let result = extract_key_from_content(content, "db.password", "test.json");
        assert!(!result.is_resolved());
    }

    #[test]
    fn extract_key_number() {
        let content = r#"{"config": {"port": 5432}}"#;
        let result = extract_key_from_content(content, "config.port", "test.json");
        assert!(result.is_resolved());
        assert_eq!(result.value(), Some("5432"));
    }

    #[test]
    fn extract_key_nested_not_object() {
        let content = r#"{"db": "not-an-object"}"#;
        let result = extract_key_from_content(content, "db.password", "test.json");
        assert!(!result.is_resolved());
    }

    #[test]
    fn extract_key_null_value() {
        let content = r#"{"db": {"password": null}}"#;
        let result = extract_key_from_content(content, "db.password", "test.json");
        assert!(!result.is_resolved());
    }
}
