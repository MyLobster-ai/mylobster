use super::Config;
use anyhow::Result;
use tracing::warn;

/// Validation errors for configuration.
#[derive(Debug, Clone)]
pub struct ConfigValidationError {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for ConfigValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Validate a configuration object.
pub fn validate_config(config: &Config) -> Vec<ConfigValidationError> {
    let mut errors = Vec::new();

    // Validate gateway port
    if config.gateway.port == 0 {
        errors.push(ConfigValidationError {
            path: "gateway.port".to_string(),
            message: "Port must be greater than 0".to_string(),
        });
    }

    // Validate auth configuration
    if let GatewayAuthMode::Token = config.gateway.auth.mode {
        if config.gateway.auth.token.is_none() {
            warn!("Gateway auth mode is 'token' but no token is configured");
        }
    }

    if let GatewayAuthMode::Password = config.gateway.auth.mode {
        if config.gateway.auth.password.is_none() {
            errors.push(ConfigValidationError {
                path: "gateway.auth.password".to_string(),
                message: "Password auth mode requires a password".to_string(),
            });
        }
    }

    // Validate model providers
    for (name, provider) in &config.models.providers {
        if provider.base_url.is_empty() {
            errors.push(ConfigValidationError {
                path: format!("models.providers.{name}.baseUrl"),
                message: "Provider base URL is required".to_string(),
            });
        }
    }

    errors
}

use super::types::GatewayAuthMode;

/// Validate configuration and return Result.
pub fn validate_config_object(config: &Config) -> Result<()> {
    let errors = validate_config(config);
    if errors.is_empty() {
        Ok(())
    } else {
        let messages: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
        anyhow::bail!("Configuration validation failed:\n{}", messages.join("\n"));
    }
}
