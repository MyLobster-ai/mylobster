use crate::config::Config;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::info;

// ============================================================================
// Plugin Interactive Onboarding (v2026.2.26)
// ============================================================================

/// Describes a single onboarding step for interactive plugin configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnboardingStep {
    /// Unique identifier for this step.
    pub id: String,
    /// Human-readable title.
    pub title: String,
    /// Description of what the user needs to do.
    pub description: String,
    /// The type of input expected.
    pub input_type: OnboardingInputType,
    /// Whether this step is required.
    #[serde(default = "default_true")]
    pub required: bool,
    /// Default value (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_value: Option<String>,
    /// Validation pattern (regex string).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation_pattern: Option<String>,
}

fn default_true() -> bool {
    true
}

/// The type of input for an onboarding step.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OnboardingInputType {
    /// A text input field.
    Text,
    /// A password/secret input (masked).
    Secret,
    /// A URL input.
    Url,
    /// A boolean toggle.
    Toggle,
    /// A selection from predefined options.
    Select(Vec<String>),
    /// An OAuth flow (returns token).
    OAuthFlow,
}

/// The result of completing an onboarding step.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnboardingStepResult {
    /// The step ID this result corresponds to.
    pub step_id: String,
    /// The value provided by the user.
    pub value: serde_json::Value,
    /// Whether the step was completed successfully.
    pub completed: bool,
    /// Error message if the step failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Overall onboarding status for a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnboardingStatus {
    /// Plugin identifier.
    pub plugin_id: String,
    /// Whether onboarding is complete.
    pub complete: bool,
    /// Steps that still need to be completed.
    pub pending_steps: Vec<String>,
    /// Steps that have been completed.
    pub completed_steps: Vec<String>,
    /// Config patches to apply once onboarding is complete.
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub config_patches: HashMap<String, serde_json::Value>,
}

/// Trait for plugins that support interactive onboarding.
///
/// Plugins that need user input during setup (API keys, OAuth tokens,
/// configuration options) implement this trait to provide a guided
/// configuration experience.
#[async_trait]
pub trait PluginOnboarding: Send + Sync {
    /// Return the list of onboarding steps for this plugin.
    fn onboarding_steps(&self) -> Vec<OnboardingStep>;

    /// Process a completed onboarding step and return the result.
    ///
    /// The implementation should validate the input and return a result
    /// indicating whether the step was completed successfully.
    async fn process_step(
        &self,
        step_id: &str,
        value: &serde_json::Value,
    ) -> Result<OnboardingStepResult>;

    /// Return the current onboarding status.
    fn status(&self) -> OnboardingStatus;

    /// Finalize onboarding and return config patches to apply.
    ///
    /// Called when all required steps are complete. Returns a map
    /// of config paths to values that should be applied.
    async fn finalize(&self) -> Result<HashMap<String, serde_json::Value>>;
}

/// Registry of loaded plugins.
///
/// Manages plugin lifecycle, discovery, and interactive onboarding.
pub struct PluginRegistry {
    _config: Config,
    /// Plugins that support interactive onboarding.
    onboarding: HashMap<String, Box<dyn PluginOnboarding>>,
}

impl PluginRegistry {
    /// Create a new plugin registry from configuration.
    pub fn new(config: &Config) -> Self {
        Self {
            _config: config.clone(),
            onboarding: HashMap::new(),
        }
    }

    /// Register a plugin that supports interactive onboarding.
    pub fn register_onboarding(&mut self, id: String, plugin: Box<dyn PluginOnboarding>) {
        info!(plugin_id = %id, "Registered plugin for interactive onboarding");
        self.onboarding.insert(id, plugin);
    }

    /// Get onboarding steps for a specific plugin.
    pub fn get_onboarding_steps(&self, plugin_id: &str) -> Option<Vec<OnboardingStep>> {
        self.onboarding.get(plugin_id).map(|p| p.onboarding_steps())
    }

    /// Process an onboarding step for a plugin.
    pub async fn process_onboarding_step(
        &self,
        plugin_id: &str,
        step_id: &str,
        value: &serde_json::Value,
    ) -> Result<OnboardingStepResult> {
        let plugin = self
            .onboarding
            .get(plugin_id)
            .ok_or_else(|| anyhow::anyhow!("Plugin '{}' not found", plugin_id))?;
        plugin.process_step(step_id, value).await
    }

    /// Get onboarding status for a plugin.
    pub fn get_onboarding_status(&self, plugin_id: &str) -> Option<OnboardingStatus> {
        self.onboarding.get(plugin_id).map(|p| p.status())
    }

    /// Finalize onboarding for a plugin and return config patches.
    pub async fn finalize_onboarding(
        &self,
        plugin_id: &str,
    ) -> Result<HashMap<String, serde_json::Value>> {
        let plugin = self
            .onboarding
            .get(plugin_id)
            .ok_or_else(|| anyhow::anyhow!("Plugin '{}' not found", plugin_id))?;
        plugin.finalize().await
    }

    /// List all plugins that support onboarding.
    pub fn list_onboarding_plugins(&self) -> Vec<OnboardingStatus> {
        self.onboarding.values().map(|p| p.status()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onboarding_step_serialization() {
        let step = OnboardingStep {
            id: "api_key".to_string(),
            title: "API Key".to_string(),
            description: "Enter your API key".to_string(),
            input_type: OnboardingInputType::Secret,
            required: true,
            default_value: None,
            validation_pattern: Some(r"^sk-[a-zA-Z0-9]+$".to_string()),
        };

        let json = serde_json::to_value(&step).unwrap();
        assert_eq!(json["id"], "api_key");
        assert_eq!(json["inputType"], "secret");
        assert!(json["required"].as_bool().unwrap());
    }

    #[test]
    fn onboarding_status_serialization() {
        let status = OnboardingStatus {
            plugin_id: "test-plugin".to_string(),
            complete: false,
            pending_steps: vec!["step1".to_string()],
            completed_steps: vec![],
            config_patches: HashMap::new(),
        };

        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["pluginId"], "test-plugin");
        assert!(!json["complete"].as_bool().unwrap());
        assert_eq!(json["pendingSteps"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn select_input_type_serialization() {
        let step = OnboardingStep {
            id: "provider".to_string(),
            title: "Provider".to_string(),
            description: "Choose a provider".to_string(),
            input_type: OnboardingInputType::Select(vec![
                "openai".to_string(),
                "anthropic".to_string(),
            ]),
            required: true,
            default_value: Some("anthropic".to_string()),
            validation_pattern: None,
        };

        let json = serde_json::to_value(&step).unwrap();
        assert_eq!(json["defaultValue"], "anthropic");

        // Deserialize back
        let deserialized: OnboardingStep = serde_json::from_value(json).unwrap();
        assert_eq!(deserialized.id, "provider");
        if let OnboardingInputType::Select(options) = &deserialized.input_type {
            assert_eq!(options.len(), 2);
        } else {
            panic!("Expected Select input type");
        }
    }

    #[test]
    fn plugin_registry_creation() {
        let config = Config::default();
        let registry = PluginRegistry::new(&config);
        assert!(registry.list_onboarding_plugins().is_empty());
    }

    #[test]
    fn plugin_registry_onboarding_not_found() {
        let config = Config::default();
        let registry = PluginRegistry::new(&config);
        assert!(registry.get_onboarding_steps("nonexistent").is_none());
        assert!(registry.get_onboarding_status("nonexistent").is_none());
    }
}
