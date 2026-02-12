use anyhow::Result;
use serde_json;
use std::path::Path;

/// Parse a JSON5 configuration string.
pub fn parse_config_json5(content: &str) -> Result<serde_json::Value> {
    let value: serde_json::Value = json5::from_str(content)?;
    Ok(value)
}

/// Read a configuration file and return its snapshot as JSON value.
pub fn read_config_file_snapshot(path: &Path) -> Result<serde_json::Value> {
    let content = std::fs::read_to_string(path)?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("json");

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
