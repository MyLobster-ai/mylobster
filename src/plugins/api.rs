//! Plugin API.
//!
//! The `PluginApi` is passed to plugins during registration. Plugins use it
//! to register tools, hooks, channels, providers, HTTP routes, and commands
//! with the host gateway.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::{debug, info};

// ============================================================================
// Plugin API
// ============================================================================

/// API handle passed to plugins during registration.
///
/// Plugins call methods on this struct to register their capabilities
/// with the host gateway. All registrations are collected and applied
/// after the plugin's `register()` function returns.
pub struct PluginApi {
    plugin_id: String,
    registrations: PluginRegistrations,
}

/// Collected registrations from a plugin.
#[derive(Debug, Default)]
pub struct PluginRegistrations {
    pub tools: Vec<ToolRegistration>,
    pub hooks: Vec<HookRegistration>,
    pub channels: Vec<ChannelRegistration>,
    pub providers: Vec<ProviderRegistration>,
    pub http_routes: Vec<HttpRouteRegistration>,
    pub commands: Vec<CommandRegistration>,
    pub config_defaults: HashMap<String, serde_json::Value>,
}

// ============================================================================
// Registration Types
// ============================================================================

/// A tool registered by a plugin.
#[derive(Debug, Clone)]
pub struct ToolRegistration {
    pub plugin_id: String,
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub handler: ToolHandler,
}

/// Handler function for a plugin tool.
#[derive(Debug, Clone)]
pub enum ToolHandler {
    /// Synchronous handler — function pointer.
    Sync(fn(&serde_json::Value) -> Result<serde_json::Value, String>),
    /// Async handler via callback channel.
    Callback {
        /// Unique handler ID for routing responses.
        handler_id: String,
    },
}

/// A hook registration by a plugin.
#[derive(Debug, Clone)]
pub struct HookRegistration {
    pub plugin_id: String,
    pub event_type: String,
    pub priority: i32,
    pub handler: HookHandler,
}

/// Handler for a plugin hook.
#[derive(Debug, Clone)]
pub enum HookHandler {
    /// Fire-and-forget (void return).
    FireAndForget(fn(&serde_json::Value)),
    /// Modifying hook that can cancel/override/transform.
    Modifying {
        handler_id: String,
    },
}

/// A channel registered by a plugin.
#[derive(Debug, Clone)]
pub struct ChannelRegistration {
    pub plugin_id: String,
    pub channel_type: String,
    pub display_name: String,
    pub capabilities: ChannelCapabilities,
    pub handler_id: String,
}

/// Capabilities of a plugin-registered channel.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelCapabilities {
    pub send_text: bool,
    pub send_media: bool,
    pub send_files: bool,
    pub reactions: bool,
    pub threads: bool,
    pub edit_messages: bool,
    pub delete_messages: bool,
}

/// A provider registered by a plugin.
#[derive(Debug, Clone)]
pub struct ProviderRegistration {
    pub plugin_id: String,
    pub provider_name: String,
    pub api_type: String,
    pub base_url: String,
    pub models: Vec<String>,
    pub handler_id: String,
}

/// An HTTP route registered by a plugin.
#[derive(Debug, Clone)]
pub struct HttpRouteRegistration {
    pub plugin_id: String,
    pub method: HttpMethod,
    pub path: String,
    pub handler_id: String,
}

/// HTTP method for plugin routes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

/// A CLI command registered by a plugin.
#[derive(Debug, Clone)]
pub struct CommandRegistration {
    pub plugin_id: String,
    pub name: String,
    pub description: String,
    pub usage: String,
    pub handler_id: String,
}

// ============================================================================
// PluginApi Implementation
// ============================================================================

impl PluginApi {
    /// Create a new PluginApi for a specific plugin.
    pub fn new(plugin_id: &str) -> Self {
        Self {
            plugin_id: plugin_id.to_string(),
            registrations: PluginRegistrations::default(),
        }
    }

    /// Get the plugin ID.
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// Consume and return all collected registrations.
    pub fn into_registrations(self) -> PluginRegistrations {
        self.registrations
    }

    // ---- Tool Registration ----

    /// Register a tool with a synchronous handler.
    pub fn register_tool(
        &mut self,
        name: &str,
        description: &str,
        parameters: serde_json::Value,
        handler: fn(&serde_json::Value) -> Result<serde_json::Value, String>,
    ) {
        debug!(
            plugin = %self.plugin_id,
            tool = name,
            "registering tool"
        );
        self.registrations.tools.push(ToolRegistration {
            plugin_id: self.plugin_id.clone(),
            name: name.to_string(),
            description: description.to_string(),
            parameters,
            handler: ToolHandler::Sync(handler),
        });
    }

    /// Register a tool with an async callback handler.
    pub fn register_tool_async(
        &mut self,
        name: &str,
        description: &str,
        parameters: serde_json::Value,
        handler_id: &str,
    ) {
        debug!(
            plugin = %self.plugin_id,
            tool = name,
            handler = handler_id,
            "registering async tool"
        );
        self.registrations.tools.push(ToolRegistration {
            plugin_id: self.plugin_id.clone(),
            name: name.to_string(),
            description: description.to_string(),
            parameters,
            handler: ToolHandler::Callback {
                handler_id: handler_id.to_string(),
            },
        });
    }

    // ---- Hook Registration ----

    /// Register a fire-and-forget hook handler.
    pub fn register_hook(
        &mut self,
        event_type: &str,
        handler: fn(&serde_json::Value),
    ) {
        self.register_hook_with_priority(event_type, 0, handler);
    }

    /// Register a fire-and-forget hook handler with priority.
    pub fn register_hook_with_priority(
        &mut self,
        event_type: &str,
        priority: i32,
        handler: fn(&serde_json::Value),
    ) {
        debug!(
            plugin = %self.plugin_id,
            event = event_type,
            priority,
            "registering hook"
        );
        self.registrations.hooks.push(HookRegistration {
            plugin_id: self.plugin_id.clone(),
            event_type: event_type.to_string(),
            priority,
            handler: HookHandler::FireAndForget(handler),
        });
    }

    /// Register a modifying hook handler.
    pub fn register_modifying_hook(
        &mut self,
        event_type: &str,
        priority: i32,
        handler_id: &str,
    ) {
        debug!(
            plugin = %self.plugin_id,
            event = event_type,
            priority,
            handler = handler_id,
            "registering modifying hook"
        );
        self.registrations.hooks.push(HookRegistration {
            plugin_id: self.plugin_id.clone(),
            event_type: event_type.to_string(),
            priority,
            handler: HookHandler::Modifying {
                handler_id: handler_id.to_string(),
            },
        });
    }

    // ---- Channel Registration ----

    /// Register a channel.
    pub fn register_channel(
        &mut self,
        channel_type: &str,
        display_name: &str,
        capabilities: ChannelCapabilities,
        handler_id: &str,
    ) {
        debug!(
            plugin = %self.plugin_id,
            channel = channel_type,
            "registering channel"
        );
        self.registrations.channels.push(ChannelRegistration {
            plugin_id: self.plugin_id.clone(),
            channel_type: channel_type.to_string(),
            display_name: display_name.to_string(),
            capabilities,
            handler_id: handler_id.to_string(),
        });
    }

    // ---- Provider Registration ----

    /// Register a model provider.
    pub fn register_provider(
        &mut self,
        provider_name: &str,
        api_type: &str,
        base_url: &str,
        models: &[&str],
        handler_id: &str,
    ) {
        debug!(
            plugin = %self.plugin_id,
            provider = provider_name,
            api_type,
            "registering provider"
        );
        self.registrations.providers.push(ProviderRegistration {
            plugin_id: self.plugin_id.clone(),
            provider_name: provider_name.to_string(),
            api_type: api_type.to_string(),
            base_url: base_url.to_string(),
            models: models.iter().map(|s| s.to_string()).collect(),
            handler_id: handler_id.to_string(),
        });
    }

    // ---- HTTP Route Registration ----

    /// Register an HTTP GET route.
    pub fn register_get_route(&mut self, path: &str, handler_id: &str) {
        self.register_route(HttpMethod::Get, path, handler_id);
    }

    /// Register an HTTP POST route.
    pub fn register_post_route(&mut self, path: &str, handler_id: &str) {
        self.register_route(HttpMethod::Post, path, handler_id);
    }

    /// Register an HTTP route with a specific method.
    pub fn register_route(
        &mut self,
        method: HttpMethod,
        path: &str,
        handler_id: &str,
    ) {
        debug!(
            plugin = %self.plugin_id,
            method = ?method,
            path,
            "registering HTTP route"
        );
        self.registrations.http_routes.push(HttpRouteRegistration {
            plugin_id: self.plugin_id.clone(),
            method,
            path: path.to_string(),
            handler_id: handler_id.to_string(),
        });
    }

    // ---- Command Registration ----

    /// Register a CLI command.
    pub fn register_command(
        &mut self,
        name: &str,
        description: &str,
        usage: &str,
        handler_id: &str,
    ) {
        debug!(
            plugin = %self.plugin_id,
            command = name,
            "registering command"
        );
        self.registrations.commands.push(CommandRegistration {
            plugin_id: self.plugin_id.clone(),
            name: name.to_string(),
            description: description.to_string(),
            usage: usage.to_string(),
            handler_id: handler_id.to_string(),
        });
    }

    // ---- Config Registration ----

    /// Register default configuration values.
    pub fn register_config_defaults(&mut self, defaults: HashMap<String, serde_json::Value>) {
        debug!(
            plugin = %self.plugin_id,
            count = defaults.len(),
            "registering config defaults"
        );
        self.registrations.config_defaults.extend(defaults);
    }

    // ---- Query Methods ----

    /// Get the gateway version string.
    pub fn gateway_version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }
}

// ============================================================================
// Plugin Coordinator
// ============================================================================

/// Coordinates all plugin registrations and makes them available
/// to the gateway subsystems.
pub struct PluginCoordinator {
    all_tools: Vec<ToolRegistration>,
    all_hooks: Vec<HookRegistration>,
    all_channels: Vec<ChannelRegistration>,
    all_providers: Vec<ProviderRegistration>,
    all_routes: Vec<HttpRouteRegistration>,
    all_commands: Vec<CommandRegistration>,
    all_config_defaults: HashMap<String, serde_json::Value>,
}

impl PluginCoordinator {
    /// Create an empty coordinator.
    pub fn new() -> Self {
        Self {
            all_tools: Vec::new(),
            all_hooks: Vec::new(),
            all_channels: Vec::new(),
            all_providers: Vec::new(),
            all_routes: Vec::new(),
            all_commands: Vec::new(),
            all_config_defaults: HashMap::new(),
        }
    }

    /// Merge registrations from a plugin.
    pub fn merge(&mut self, registrations: PluginRegistrations) {
        self.all_tools.extend(registrations.tools);
        self.all_hooks.extend(registrations.hooks);
        self.all_channels.extend(registrations.channels);
        self.all_providers.extend(registrations.providers);
        self.all_routes.extend(registrations.http_routes);
        self.all_commands.extend(registrations.commands);
        self.all_config_defaults.extend(registrations.config_defaults);
    }

    /// Get all registered tools.
    pub fn tools(&self) -> &[ToolRegistration] {
        &self.all_tools
    }

    /// Get all registered hooks.
    pub fn hooks(&self) -> &[HookRegistration] {
        &self.all_hooks
    }

    /// Get all registered channels.
    pub fn channels(&self) -> &[ChannelRegistration] {
        &self.all_channels
    }

    /// Get all registered providers.
    pub fn providers(&self) -> &[ProviderRegistration] {
        &self.all_providers
    }

    /// Get all registered HTTP routes.
    pub fn routes(&self) -> &[HttpRouteRegistration] {
        &self.all_routes
    }

    /// Get all registered commands.
    pub fn commands(&self) -> &[CommandRegistration] {
        &self.all_commands
    }

    /// Get merged config defaults.
    pub fn config_defaults(&self) -> &HashMap<String, serde_json::Value> {
        &self.all_config_defaults
    }

    /// Summary of all registrations.
    pub fn summary(&self) -> String {
        format!(
            "tools={}, hooks={}, channels={}, providers={}, routes={}, commands={}",
            self.all_tools.len(),
            self.all_hooks.len(),
            self.all_channels.len(),
            self.all_providers.len(),
            self.all_routes.len(),
            self.all_commands.len(),
        )
    }
}

impl Default for PluginCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_tool() {
        let mut api = PluginApi::new("test-plugin");
        api.register_tool(
            "my_tool",
            "A test tool",
            serde_json::json!({"type": "object"}),
            |_params| Ok(serde_json::json!({"result": "ok"})),
        );
        let regs = api.into_registrations();
        assert_eq!(regs.tools.len(), 1);
        assert_eq!(regs.tools[0].name, "my_tool");
        assert_eq!(regs.tools[0].plugin_id, "test-plugin");
    }

    #[test]
    fn register_hook() {
        let mut api = PluginApi::new("test-plugin");
        api.register_hook("MessageReceived", |_data| {});
        api.register_hook_with_priority("LlmOutput", 10, |_data| {});
        let regs = api.into_registrations();
        assert_eq!(regs.hooks.len(), 2);
        assert_eq!(regs.hooks[1].priority, 10);
    }

    #[test]
    fn register_channel() {
        let mut api = PluginApi::new("test-plugin");
        api.register_channel(
            "custom-chat",
            "Custom Chat",
            ChannelCapabilities {
                send_text: true,
                send_media: true,
                ..Default::default()
            },
            "handler-1",
        );
        let regs = api.into_registrations();
        assert_eq!(regs.channels.len(), 1);
        assert!(regs.channels[0].capabilities.send_text);
    }

    #[test]
    fn register_provider() {
        let mut api = PluginApi::new("test-plugin");
        api.register_provider(
            "custom-ai",
            "openai-compat",
            "https://api.custom.ai/v1",
            &["custom-model-1", "custom-model-2"],
            "handler-2",
        );
        let regs = api.into_registrations();
        assert_eq!(regs.providers.len(), 1);
        assert_eq!(regs.providers[0].models.len(), 2);
    }

    #[test]
    fn register_http_routes() {
        let mut api = PluginApi::new("test-plugin");
        api.register_get_route("/api/plugin/status", "status-handler");
        api.register_post_route("/api/plugin/action", "action-handler");
        let regs = api.into_registrations();
        assert_eq!(regs.http_routes.len(), 2);
        assert_eq!(regs.http_routes[0].method, HttpMethod::Get);
        assert_eq!(regs.http_routes[1].method, HttpMethod::Post);
    }

    #[test]
    fn register_command() {
        let mut api = PluginApi::new("test-plugin");
        api.register_command(
            "custom-cmd",
            "A custom CLI command",
            "mylobster custom-cmd [args]",
            "cmd-handler",
        );
        let regs = api.into_registrations();
        assert_eq!(regs.commands.len(), 1);
        assert_eq!(regs.commands[0].name, "custom-cmd");
    }

    #[test]
    fn coordinator_merge() {
        let mut coord = PluginCoordinator::new();

        let mut api1 = PluginApi::new("plugin-a");
        api1.register_tool("tool_a", "Tool A", serde_json::json!({}), |_| {
            Ok(serde_json::json!(null))
        });

        let mut api2 = PluginApi::new("plugin-b");
        api2.register_tool("tool_b", "Tool B", serde_json::json!({}), |_| {
            Ok(serde_json::json!(null))
        });
        api2.register_hook("MessageReceived", |_| {});

        coord.merge(api1.into_registrations());
        coord.merge(api2.into_registrations());

        assert_eq!(coord.tools().len(), 2);
        assert_eq!(coord.hooks().len(), 1);
        assert_eq!(coord.channels().len(), 0);
    }

    #[test]
    fn coordinator_summary() {
        let coord = PluginCoordinator::new();
        assert_eq!(
            coord.summary(),
            "tools=0, hooks=0, channels=0, providers=0, routes=0, commands=0"
        );
    }
}
