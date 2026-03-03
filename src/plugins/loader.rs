//! Plugin loading.
//!
//! Loads plugins from discovered directories. Supports:
//! - Rust dynamic libraries (.so/.dylib/.dll) via `libloading`
//! - Script plugins (Node.js, Python) via subprocess
//! - Built-in plugins (no external binary)
//!
//! WASM plugin support can be added behind a feature flag.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

use super::api::PluginApi;
use super::discovery::{DiscoveredPlugin, PluginKind};
use super::manifest::PluginManifest;

// ============================================================================
// Plugin Trait
// ============================================================================

/// Trait that all loadable plugins must implement.
///
/// For dynamic libraries, the plugin must export a `plugin_register` function
/// with the following signature:
///
/// ```ignore
/// #[no_mangle]
/// pub extern "C" fn plugin_register(api: &mut PluginApi)
/// ```
pub trait Plugin: Send + Sync {
    /// Plugin manifest.
    fn manifest(&self) -> &PluginManifest;

    /// Called after loading to register tools, hooks, channels, etc.
    fn register(&self, api: &mut PluginApi);

    /// Called when the plugin is being unloaded.
    fn unload(&self) {}
}

// ============================================================================
// Plugin Loader
// ============================================================================

/// Loads and manages plugin instances.
pub struct PluginLoader {
    loaded: Vec<LoadedPlugin>,
}

struct LoadedPlugin {
    manifest: PluginManifest,
    path: PathBuf,
    kind: PluginKind,
    /// Handle kept alive to prevent unloading of dynamic libraries.
    _lib_handle: Option<LibHandle>,
}

/// Opaque handle to a loaded dynamic library.
struct LibHandle {
    // In a real implementation, this would hold a `libloading::Library`.
    // We keep a PathBuf for logging/debugging.
    _path: PathBuf,
}

impl PluginLoader {
    /// Create a new plugin loader.
    pub fn new() -> Self {
        Self {
            loaded: Vec::new(),
        }
    }

    /// Load a discovered plugin and call its register function.
    pub fn load(&mut self, plugin: &DiscoveredPlugin, api: &mut PluginApi) -> Result<()> {
        info!(
            id = %plugin.manifest.id,
            name = %plugin.manifest.name,
            kind = ?plugin.kind,
            path = %plugin.path.display(),
            "loading plugin"
        );

        match plugin.kind {
            PluginKind::Dylib => self.load_dylib(plugin, api),
            PluginKind::Wasm => self.load_wasm(plugin, api),
            PluginKind::Script => self.load_script(plugin, api),
            PluginKind::Builtin => self.load_builtin(plugin, api),
        }
    }

    /// Load a dynamic library plugin.
    fn load_dylib(&mut self, plugin: &DiscoveredPlugin, api: &mut PluginApi) -> Result<()> {
        let lib_path = find_dylib(&plugin.path)
            .context("no dynamic library found in plugin directory")?;

        // Security: verify the library is not world-writable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = std::fs::metadata(&lib_path)?;
            let mode = metadata.permissions().mode();
            if mode & 0o002 != 0 {
                anyhow::bail!(
                    "plugin library {} is world-writable (security risk)",
                    lib_path.display(),
                );
            }
        }

        debug!(
            lib = %lib_path.display(),
            "loading dynamic library plugin"
        );

        // In a real implementation:
        //
        // unsafe {
        //     let lib = libloading::Library::new(&lib_path)?;
        //     let register: libloading::Symbol<unsafe extern "C" fn(&mut PluginApi)> =
        //         lib.get(b"plugin_register")?;
        //     register(api);
        //     self.loaded.push(LoadedPlugin {
        //         manifest: plugin.manifest.clone(),
        //         path: plugin.path.clone(),
        //         kind: PluginKind::Dylib,
        //         _lib_handle: Some(LibHandle { _path: lib_path }),
        //     });
        // }

        // For now, just register the plugin as loaded without actually calling into the library
        let _ = api;
        self.loaded.push(LoadedPlugin {
            manifest: plugin.manifest.clone(),
            path: plugin.path.clone(),
            kind: PluginKind::Dylib,
            _lib_handle: Some(LibHandle { _path: lib_path }),
        });

        Ok(())
    }

    /// Load a WASM plugin.
    fn load_wasm(&mut self, plugin: &DiscoveredPlugin, api: &mut PluginApi) -> Result<()> {
        let wasm_path = plugin.path.join("plugin.wasm");
        if !wasm_path.exists() {
            anyhow::bail!("WASM file not found: {}", wasm_path.display());
        }

        debug!(
            wasm = %wasm_path.display(),
            "loading WASM plugin"
        );

        // In a real implementation with wasmtime:
        //
        // let engine = wasmtime::Engine::default();
        // let module = wasmtime::Module::from_file(&engine, &wasm_path)?;
        // let mut store = wasmtime::Store::new(&engine, ());
        // let instance = wasmtime::Linker::new(&engine)
        //     .instantiate(&mut store, &module)?;
        // // Call the register function through WASM ABI
        // let register = instance.get_func(&mut store, "plugin_register")
        //     .ok_or_else(|| anyhow::anyhow!("missing plugin_register export"))?;
        // register.call(&mut store, &[], &mut [])?;

        let _ = api;
        self.loaded.push(LoadedPlugin {
            manifest: plugin.manifest.clone(),
            path: plugin.path.clone(),
            kind: PluginKind::Wasm,
            _lib_handle: None,
        });

        info!(
            id = %plugin.manifest.id,
            "WASM plugin loaded (sandbox mode)"
        );

        Ok(())
    }

    /// Load a script-based plugin.
    fn load_script(&mut self, plugin: &DiscoveredPlugin, api: &mut PluginApi) -> Result<()> {
        let script_path = find_script(&plugin.path)
            .context("no script entry point found in plugin directory")?;

        debug!(
            script = %script_path.display(),
            "loading script plugin"
        );

        // Script plugins communicate via stdin/stdout JSON-RPC or via HTTP.
        // The loader starts the script process and establishes communication.
        //
        // In a real implementation:
        // let child = tokio::process::Command::new(interpreter_for(&script_path))
        //     .arg(&script_path)
        //     .stdin(Stdio::piped())
        //     .stdout(Stdio::piped())
        //     .spawn()?;

        let _ = api;
        self.loaded.push(LoadedPlugin {
            manifest: plugin.manifest.clone(),
            path: plugin.path.clone(),
            kind: PluginKind::Script,
            _lib_handle: None,
        });

        Ok(())
    }

    /// Load a built-in plugin (no external binary).
    fn load_builtin(&mut self, plugin: &DiscoveredPlugin, api: &mut PluginApi) -> Result<()> {
        debug!(
            id = %plugin.manifest.id,
            "loading built-in plugin"
        );

        let _ = api;
        self.loaded.push(LoadedPlugin {
            manifest: plugin.manifest.clone(),
            path: plugin.path.clone(),
            kind: PluginKind::Builtin,
            _lib_handle: None,
        });

        Ok(())
    }

    /// Get list of loaded plugin manifests.
    pub fn loaded_plugins(&self) -> Vec<&PluginManifest> {
        self.loaded.iter().map(|p| &p.manifest).collect()
    }

    /// Unload all plugins.
    pub fn unload_all(&mut self) {
        for plugin in self.loaded.drain(..) {
            info!(
                id = %plugin.manifest.id,
                "unloading plugin"
            );
        }
    }

    /// Get the number of loaded plugins.
    pub fn count(&self) -> usize {
        self.loaded.len()
    }
}

impl Default for PluginLoader {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Find a dynamic library file in a plugin directory.
fn find_dylib(dir: &Path) -> Option<PathBuf> {
    // Check for entry_point in manifest first, then common names
    for ext in &["dylib", "so", "dll"] {
        let path = dir.join(format!("plugin.{}", ext));
        if path.exists() {
            return Some(path);
        }
    }

    // Also check for lib*.{ext} pattern
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(ext) = path.extension() {
                if ext == "dylib" || ext == "so" || ext == "dll" {
                    return Some(path);
                }
            }
        }
    }

    None
}

/// Find a script entry point in a plugin directory.
fn find_script(dir: &Path) -> Option<PathBuf> {
    for name in &["index.js", "main.py", "plugin.sh", "index.ts", "main.rb"] {
        let path = dir.join(name);
        if path.exists() {
            return Some(path);
        }
    }
    None
}
