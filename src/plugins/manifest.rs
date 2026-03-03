//! Plugin manifest types.
//!
//! Parsed from `plugin.json` in each plugin directory.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Plugin manifest — the `plugin.json` file in each plugin directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginManifest {
    /// Unique plugin identifier (e.g., "com.example.my-plugin").
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Short description.
    #[serde(default)]
    pub description: String,
    /// Semantic version string.
    #[serde(default = "default_version")]
    pub version: String,
    /// Plugin author.
    #[serde(default)]
    pub author: String,
    /// Homepage/repository URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    /// License identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Minimum gateway version required.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_gateway_version: Option<String>,
    /// Plugin kind hint (overridden by auto-detection if binary exists).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Entry point path (relative to plugin directory).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_point: Option<String>,
    /// Configuration schema for this plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Default configuration values.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub default_config: HashMap<String, serde_json::Value>,
    /// Capabilities this plugin provides.
    #[serde(default)]
    pub capabilities: PluginCapabilities,
    /// Permissions this plugin requires.
    #[serde(default)]
    pub permissions: PluginPermissions,
    /// Dependencies on other plugins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<PluginDependency>,
}

fn default_version() -> String {
    "0.1.0".to_string()
}

/// What a plugin can provide.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginCapabilities {
    /// Tool names this plugin registers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// Channel types this plugin registers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<String>,
    /// Provider names this plugin registers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<String>,
    /// Hook event types this plugin listens to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<String>,
    /// HTTP route prefixes this plugin mounts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub http_routes: Vec<String>,
    /// CLI commands this plugin registers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<String>,
}

/// Permissions a plugin needs from the host.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginPermissions {
    /// Network access domains.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network: Vec<String>,
    /// Filesystem paths (read).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fs_read: Vec<String>,
    /// Filesystem paths (write).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fs_write: Vec<String>,
    /// Environment variable names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_vars: Vec<String>,
    /// Whether the plugin can spawn subprocesses.
    #[serde(default)]
    pub subprocess: bool,
}

/// Dependency on another plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginDependency {
    /// Plugin ID of the dependency.
    pub id: String,
    /// Required version range (semver).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Whether this dependency is optional.
    #[serde(default)]
    pub optional: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_manifest() {
        let json = r#"{
            "id": "com.example.test",
            "name": "Test Plugin"
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.id, "com.example.test");
        assert_eq!(manifest.name, "Test Plugin");
        assert_eq!(manifest.version, "0.1.0");
        assert!(manifest.capabilities.tools.is_empty());
    }

    #[test]
    fn parse_full_manifest() {
        let json = r#"{
            "id": "com.example.discord-ext",
            "name": "Discord Extension",
            "description": "Extended Discord integration",
            "version": "1.2.0",
            "author": "Example Corp",
            "homepage": "https://example.com",
            "license": "MIT",
            "minGatewayVersion": "2026.3.0",
            "kind": "dylib",
            "entryPoint": "plugin.dylib",
            "capabilities": {
                "tools": ["discord_poll", "discord_embed"],
                "channels": ["discord-ext"],
                "hooks": ["MessageReceived"]
            },
            "permissions": {
                "network": ["discord.com"],
                "subprocess": false
            },
            "dependencies": [
                { "id": "com.example.base", "version": ">=1.0.0" }
            ]
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.version, "1.2.0");
        assert_eq!(manifest.capabilities.tools.len(), 2);
        assert_eq!(manifest.capabilities.channels.len(), 1);
        assert_eq!(manifest.permissions.network.len(), 1);
        assert_eq!(manifest.dependencies.len(), 1);
        assert!(!manifest.dependencies[0].optional);
    }

    #[test]
    fn roundtrip_serialization() {
        let manifest = PluginManifest {
            id: "test".to_string(),
            name: "Test".to_string(),
            description: String::new(),
            version: "1.0.0".to_string(),
            author: String::new(),
            homepage: None,
            license: None,
            min_gateway_version: None,
            kind: None,
            entry_point: None,
            config_schema: None,
            default_config: HashMap::new(),
            capabilities: PluginCapabilities::default(),
            permissions: PluginPermissions::default(),
            dependencies: Vec::new(),
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: PluginManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "test");
    }
}
