use crate::config::Config;

/// Registry of loaded plugins.
///
/// This is a placeholder implementation. The full version will load, initialise,
/// and manage lifecycle of external plugins (memory backends, tool extensions,
/// channel extensions, etc.).
pub struct PluginRegistry {
    _config: Config,
}

impl PluginRegistry {
    /// Create a new plugin registry from configuration.
    pub fn new(config: &Config) -> Self {
        Self {
            _config: config.clone(),
        }
    }
}
