//! Plugin discovery.
//!
//! Scans directories for plugin manifests, validates them, and
//! returns a list of discovered plugins.

use super::manifest::PluginManifest;
use anyhow::Result;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Discovered plugin with its manifest and location.
#[derive(Debug, Clone)]
pub struct DiscoveredPlugin {
    pub manifest: PluginManifest,
    pub path: PathBuf,
    pub kind: PluginKind,
}

/// The type of plugin binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginKind {
    /// Rust dynamic library (.so/.dylib/.dll)
    Dylib,
    /// WebAssembly module (.wasm)
    Wasm,
    /// Script-based (Node.js, Python, etc.)
    Script,
    /// Built-in (no external binary)
    Builtin,
}

/// Default plugin search paths.
pub fn default_search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // User-level plugins
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".mylobster").join("extensions"));
    }

    // Project-level plugins
    paths.push(PathBuf::from(".mylobster").join("extensions"));

    // System-level plugins
    #[cfg(unix)]
    paths.push(PathBuf::from("/usr/local/lib/mylobster/plugins"));

    paths
}

/// Scan directories for plugin manifests.
pub fn discover_plugins(search_paths: &[PathBuf]) -> Vec<DiscoveredPlugin> {
    let mut plugins = Vec::new();

    for path in search_paths {
        if !path.exists() || !path.is_dir() {
            debug!(path = %path.display(), "plugin search path does not exist");
            continue;
        }

        // Check directory permissions (security)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(metadata) = std::fs::metadata(path) {
                let mode = metadata.permissions().mode();
                if mode & 0o002 != 0 {
                    warn!(
                        path = %path.display(),
                        "skipping world-writable plugin directory (security risk)"
                    );
                    continue;
                }
            }
        }

        match scan_directory(path) {
            Ok(found) => {
                info!(
                    path = %path.display(),
                    count = found.len(),
                    "discovered plugins"
                );
                plugins.extend(found);
            }
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to scan plugin directory"
                );
            }
        }
    }

    plugins
}

fn scan_directory(dir: &Path) -> Result<Vec<DiscoveredPlugin>> {
    let mut plugins = Vec::new();

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if !path.is_dir() {
            continue;
        }

        // Look for plugin.json manifest
        let manifest_path = path.join("plugin.json");
        if !manifest_path.exists() {
            continue;
        }

        match load_manifest(&manifest_path) {
            Ok(manifest) => {
                let kind = detect_plugin_kind(&path);
                debug!(
                    id = %manifest.id,
                    name = %manifest.name,
                    kind = ?kind,
                    "discovered plugin"
                );
                plugins.push(DiscoveredPlugin {
                    manifest,
                    path,
                    kind,
                });
            }
            Err(e) => {
                warn!(
                    path = %manifest_path.display(),
                    error = %e,
                    "failed to load plugin manifest"
                );
            }
        }
    }

    Ok(plugins)
}

fn load_manifest(path: &Path) -> Result<PluginManifest> {
    let content = std::fs::read_to_string(path)?;
    let manifest: PluginManifest = serde_json::from_str(&content)?;
    Ok(manifest)
}

fn detect_plugin_kind(dir: &Path) -> PluginKind {
    // Check for various plugin binary types
    if dir.join("plugin.wasm").exists() {
        return PluginKind::Wasm;
    }

    // Check for dynamic libraries
    for ext in &["so", "dylib", "dll"] {
        let lib_path = dir.join(format!("plugin.{}", ext));
        if lib_path.exists() {
            return PluginKind::Dylib;
        }
    }

    // Check for scripts
    for file in &["index.js", "main.py", "plugin.sh"] {
        if dir.join(file).exists() {
            return PluginKind::Script;
        }
    }

    PluginKind::Builtin
}
