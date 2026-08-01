use anyhow::{bail, Context, Result};
use serde_json;
use std::path::Path;
use tracing::warn;

/// Maximum size for a config file (10 MB).
pub const MAX_CONFIG_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// Maximum recursion depth for config includes.
pub const MAX_INCLUDE_DEPTH: usize = 16;

/// Parse a JSON5 configuration string.
pub fn parse_config_json5(content: &str) -> Result<serde_json::Value> {
    let value: serde_json::Value = json5::from_str(content)?;
    Ok(value)
}

/// Read a configuration file with security hardening (v2026.2.26).
///
/// Security checks:
/// - File size guardrail (`MAX_CONFIG_FILE_BYTES`)
/// - Hardlink detection (rejects files with nlink > 1)
/// - Symlink following on final component (O_NOFOLLOW semantics on Unix)
pub fn read_config_file_snapshot(path: &Path) -> Result<serde_json::Value> {
    // 1. Check file metadata before reading.
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("Cannot stat config file '{}'", path.display()))?;

    // 2. Reject symlinks at the final path component (O_NOFOLLOW equivalent).
    #[cfg(unix)]
    if metadata.file_type().is_symlink() {
        bail!(
            "Config file '{}' is a symlink — refusing to follow for security",
            path.display()
        );
    }

    // 3. Resolve to canonical path and re-stat.
    let canonical = path
        .canonicalize()
        .with_context(|| format!("Cannot canonicalize config path '{}'", path.display()))?;
    let real_metadata = std::fs::metadata(&canonical)
        .with_context(|| format!("Cannot stat canonical config path '{}'", canonical.display()))?;

    // 4. Size guardrail.
    if real_metadata.len() > MAX_CONFIG_FILE_BYTES {
        bail!(
            "Config file '{}' is {} bytes, exceeds limit of {} bytes",
            path.display(),
            real_metadata.len(),
            MAX_CONFIG_FILE_BYTES,
        );
    }

    // 5. Reject hardlinked files (nlink > 1).
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if real_metadata.is_file() && real_metadata.nlink() > 1 {
            bail!(
                "Config file '{}' has {} hard links — refusing to read \
                 (hardlinks can alias files outside workspace)",
                path.display(),
                real_metadata.nlink(),
            );
        }
    }

    // 6. Read content from canonical path.
    let content = std::fs::read_to_string(&canonical)
        .with_context(|| format!("Failed to read config file '{}'", canonical.display()))?;

    let ext = canonical
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("json");

    match ext {
        "yaml" | "yml" => {
            let value: serde_json::Value = serde_yaml::from_str(&content)?;
            Ok(value)
        }
        "toml" => {
            let value: serde_json::Value = toml::from_str(&content)?;
            Ok(value)
        }
        _ => parse_config_json5(&content),
    }
}

/// Environment variables holding operator-approved `$include` roots
/// (path-separator-delimited, v2026.5.2). When set, `$include` directives
/// may only reference files inside these directories (plus the including
/// config's own directory).
pub const INCLUDE_ROOTS_ENV_VARS: &[&str] = &["OPENCLAW_INCLUDE_ROOTS", "MYLOBSTER_INCLUDE_ROOTS"];

/// Parse the operator-approved include roots from the environment.
/// Returns `None` when no roots are configured (legacy behavior: any path).
pub fn approved_include_roots() -> Option<Vec<std::path::PathBuf>> {
    for var in INCLUDE_ROOTS_ENV_VARS {
        if let Ok(raw) = std::env::var(var) {
            let roots = parse_include_roots(&raw);
            if !roots.is_empty() {
                return Some(roots);
            }
        }
    }
    None
}

/// Parse a path-separator-delimited include-roots string.
pub fn parse_include_roots(raw: &str) -> Vec<std::path::PathBuf> {
    std::env::split_paths(raw)
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
}

/// Whether an include path is permitted under the approved roots policy
/// (v2026.5.2). `config_dir` (the including file's directory) is always
/// allowed so sibling includes keep working.
pub fn include_path_allowed(
    include_path: &Path,
    config_dir: Option<&Path>,
    roots: Option<&[std::path::PathBuf]>,
) -> bool {
    let Some(roots) = roots else {
        return true; // no policy configured → legacy behavior
    };
    // Compare on canonical paths when possible (falls back to lexical).
    let canonical = include_path
        .canonicalize()
        .unwrap_or_else(|_| include_path.to_path_buf());
    if let Some(dir) = config_dir {
        let dir_canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        if canonical.starts_with(&dir_canonical) {
            return true;
        }
    }
    roots.iter().any(|root| {
        let root_canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
        canonical.starts_with(&root_canonical)
    })
}

/// Read a config file with include depth tracking (v2026.2.26).
///
/// Prevents infinite include loops by tracking recursion depth.
/// v2026.5.2: when `OPENCLAW_INCLUDE_ROOTS` (or `MYLOBSTER_INCLUDE_ROOTS`)
/// is set, `$include` targets outside the approved roots (and outside the
/// including file's directory) are rejected.
pub fn read_config_with_includes(
    path: &Path,
    depth: usize,
) -> Result<serde_json::Value> {
    if depth > MAX_INCLUDE_DEPTH {
        bail!(
            "Config include depth exceeded {} at '{}'",
            MAX_INCLUDE_DEPTH,
            path.display(),
        );
    }

    let mut config = read_config_file_snapshot(path)?;

    // Process $include directives if present.
    if let Some(includes) = config
        .as_object()
        .and_then(|obj| obj.get("$include"))
        .cloned()
    {
        let include_paths = match includes {
            serde_json::Value::String(s) => vec![s],
            serde_json::Value::Array(arr) => arr
                .into_iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            _ => {
                warn!("Invalid $include value in '{}', skipping", path.display());
                vec![]
            }
        };

        let approved_roots = approved_include_roots();
        for include_path_str in &include_paths {
            let include_path = if Path::new(include_path_str).is_absolute() {
                std::path::PathBuf::from(include_path_str)
            } else {
                path.parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(include_path_str)
            };

            // v2026.5.2: enforce operator-approved include roots.
            if !include_path_allowed(
                &include_path,
                path.parent(),
                approved_roots.as_deref(),
            ) {
                bail!(
                    "$include '{}' is outside the operator-approved include roots \
                     (OPENCLAW_INCLUDE_ROOTS)",
                    include_path.display(),
                );
            }

            match read_config_with_includes(&include_path, depth + 1) {
                Ok(included) => {
                    merge_config_values(&mut config, &included);
                }
                Err(e) => {
                    let msg = e.to_string();
                    // Depth exceeded and security errors are fatal — propagate up.
                    if msg.contains("depth exceeded")
                        || msg.contains("symlink")
                        || msg.contains("hardlink")
                    {
                        return Err(e);
                    }
                    warn!(
                        "Failed to include config '{}': {}",
                        include_path.display(),
                        e
                    );
                }
            }
        }

        // Remove the $include key from the final config.
        if let Some(obj) = config.as_object_mut() {
            obj.remove("$include");
        }
    }

    Ok(config)
}

/// Deep merge two JSON config values (source into target).
fn merge_config_values(target: &mut serde_json::Value, source: &serde_json::Value) {
    match (target, source) {
        (serde_json::Value::Object(ref mut target_map), serde_json::Value::Object(source_map)) => {
            for (key, value) in source_map {
                let entry = target_map
                    .entry(key.clone())
                    .or_insert(serde_json::Value::Null);
                merge_config_values(entry, value);
            }
        }
        (target, source) => {
            *target = source.clone();
        }
    }
}

/// Compute a hash of a configuration snapshot for change detection.
pub fn resolve_config_snapshot_hash(value: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};
    let canonical = serde_json::to_string(value).unwrap_or_default();
    let hash = Sha256::digest(canonical.as_bytes());
    hex::encode(hash)
}

/// Write configuration to a JSON file.
pub fn write_config_file(path: &Path, config: &serde_json::Value) -> Result<()> {
    let content = serde_json::to_string_pretty(config)?;
    std::fs::write(path, content)?;
    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn read_json_config() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("config.json");
        fs::write(&file, r#"{"gateway": {"port": 18789}}"#).unwrap();

        let config = read_config_file_snapshot(&file).unwrap();
        assert_eq!(config["gateway"]["port"], 18789);
    }

    #[test]
    fn read_yaml_config() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("config.yaml");
        fs::write(&file, "gateway:\n  port: 18789\n").unwrap();

        let config = read_config_file_snapshot(&file).unwrap();
        assert_eq!(config["gateway"]["port"], 18789);
    }

    #[test]
    fn reject_oversized_config() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("huge.json");
        // Create a file just over the limit
        let content = "x".repeat((MAX_CONFIG_FILE_BYTES + 1) as usize);
        fs::write(&file, content).unwrap();

        let result = read_config_file_snapshot(&file);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("exceeds limit"));
    }

    #[cfg(unix)]
    #[test]
    fn reject_hardlinked_config() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("config.json");
        let link = dir.path().join("alias.json");
        fs::write(&file, "{}").unwrap();
        fs::hard_link(&file, &link).unwrap();

        let result = read_config_file_snapshot(&file);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("hard links"));
    }

    #[cfg(unix)]
    #[test]
    fn reject_symlinked_config() {
        let dir = TempDir::new().unwrap();
        let real_file = dir.path().join("real.json");
        let symlink = dir.path().join("link.json");
        fs::write(&real_file, "{}").unwrap();
        std::os::unix::fs::symlink(&real_file, &symlink).unwrap();

        let result = read_config_file_snapshot(&symlink);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("symlink"));
    }

    #[test]
    fn include_depth_limit() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("config.json");
        // Self-referencing include
        fs::write(
            &file,
            format!(r#"{{"$include": "{}"}}"#, file.display()),
        )
        .unwrap();

        let result = read_config_with_includes(&file, 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("depth exceeded"));
    }

    #[test]
    fn config_with_includes_merges() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("base.json");
        let extra = dir.path().join("extra.json");

        fs::write(&base, r#"{"$include": "extra.json", "a": 1}"#).unwrap();
        fs::write(&extra, r#"{"b": 2}"#).unwrap();

        let config = read_config_with_includes(&base, 0).unwrap();
        assert_eq!(config["a"], 1);
        assert_eq!(config["b"], 2);
        assert!(config.get("$include").is_none());
    }

    #[test]
    fn merge_values_deep() {
        let mut target = serde_json::json!({"a": {"x": 1}, "b": 2});
        let source = serde_json::json!({"a": {"y": 3}, "c": 4});
        merge_config_values(&mut target, &source);

        assert_eq!(target["a"]["x"], 1);
        assert_eq!(target["a"]["y"], 3);
        assert_eq!(target["b"], 2);
        assert_eq!(target["c"], 4);
    }

    #[test]
    fn hash_deterministic() {
        let val = serde_json::json!({"key": "value"});
        let h1 = resolve_config_snapshot_hash(&val);
        let h2 = resolve_config_snapshot_hash(&val);
        assert_eq!(h1, h2);
    }

    // ====================================================================
    // $include approved roots (v2026.5.2)
    // ====================================================================

    #[test]
    fn parse_include_roots_splits_path_list() {
        let joined = std::env::join_paths(["/opt/conf", "/etc/mylobster"])
            .unwrap()
            .into_string()
            .unwrap();
        let roots = parse_include_roots(&joined);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], std::path::PathBuf::from("/opt/conf"));
        assert!(parse_include_roots("").is_empty());
    }

    #[test]
    fn include_allowed_without_policy() {
        assert!(include_path_allowed(
            Path::new("/anywhere/at/all.json"),
            None,
            None
        ));
    }

    #[test]
    fn include_policy_allows_roots_and_config_dir_only() {
        let root_dir = TempDir::new().unwrap();
        let conf_dir = TempDir::new().unwrap();
        let other_dir = TempDir::new().unwrap();
        fs::write(root_dir.path().join("inc.json"), "{}").unwrap();
        fs::write(conf_dir.path().join("sibling.json"), "{}").unwrap();
        fs::write(other_dir.path().join("evil.json"), "{}").unwrap();

        let roots = vec![root_dir.path().to_path_buf()];

        // Inside an approved root → allowed
        assert!(include_path_allowed(
            &root_dir.path().join("inc.json"),
            Some(conf_dir.path()),
            Some(&roots)
        ));
        // Sibling of the including config → allowed
        assert!(include_path_allowed(
            &conf_dir.path().join("sibling.json"),
            Some(conf_dir.path()),
            Some(&roots)
        ));
        // Outside both → rejected
        assert!(!include_path_allowed(
            &other_dir.path().join("evil.json"),
            Some(conf_dir.path()),
            Some(&roots)
        ));
    }

    #[test]
    fn include_outside_roots_fails_load() {
        let conf_dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let root_dir = TempDir::new().unwrap();
        fs::write(outside.path().join("secret.json"), r#"{"x": 1}"#).unwrap();
        let base = conf_dir.path().join("base.json");
        fs::write(
            &base,
            format!(
                r#"{{"$include": "{}"}}"#,
                outside.path().join("secret.json").display()
            ),
        )
        .unwrap();

        // Policy env vars are process-global; guard with a unique var set
        // just for this call path via direct helper testing instead.
        let roots = vec![root_dir.path().to_path_buf()];
        assert!(!include_path_allowed(
            &outside.path().join("secret.json"),
            base.parent(),
            Some(&roots)
        ));
    }
}
