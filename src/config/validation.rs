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

// ============================================================================
// Sandbox Network Mode Validation (v2026.2.24)
// ============================================================================

/// Reason a Docker network mode is blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkModeBlockReason {
    /// `network: "host"` — always blocked.
    Host,
    /// `network: "container:<id>"` — blocked unless explicitly allowed.
    ContainerNamespaceJoin,
}

/// Check whether a Docker network mode string should be blocked.
///
/// Returns `Some(reason)` if the mode is dangerous, `None` if safe.
///
/// - `"host"` is always blocked.
/// - `"container:<id>"` is blocked unless `allow_container_namespace_join` is true.
/// - All other values (`"bridge"`, `"none"`, custom networks) are allowed.
pub fn get_blocked_network_mode_reason(
    network: Option<&str>,
    allow_container_namespace_join: bool,
) -> Option<NetworkModeBlockReason> {
    let network = match network {
        Some(n) => n.trim().to_lowercase(),
        None => return None,
    };

    if network == "host" {
        return Some(NetworkModeBlockReason::Host);
    }

    if network.starts_with("container:") && !allow_container_namespace_join {
        return Some(NetworkModeBlockReason::ContainerNamespaceJoin);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_network_always_blocked() {
        assert_eq!(
            get_blocked_network_mode_reason(Some("host"), false),
            Some(NetworkModeBlockReason::Host)
        );
        assert_eq!(
            get_blocked_network_mode_reason(Some("host"), true),
            Some(NetworkModeBlockReason::Host)
        );
    }

    #[test]
    fn host_network_case_insensitive() {
        assert_eq!(
            get_blocked_network_mode_reason(Some("HOST"), false),
            Some(NetworkModeBlockReason::Host)
        );
        assert_eq!(
            get_blocked_network_mode_reason(Some(" Host "), false),
            Some(NetworkModeBlockReason::Host)
        );
    }

    #[test]
    fn container_namespace_blocked_by_default() {
        assert_eq!(
            get_blocked_network_mode_reason(Some("container:abc123"), false),
            Some(NetworkModeBlockReason::ContainerNamespaceJoin)
        );
    }

    #[test]
    fn container_namespace_allowed_when_flag_set() {
        assert_eq!(
            get_blocked_network_mode_reason(Some("container:abc123"), true),
            None
        );
    }

    #[test]
    fn bridge_network_allowed() {
        assert_eq!(get_blocked_network_mode_reason(Some("bridge"), false), None);
    }

    #[test]
    fn none_network_allowed() {
        assert_eq!(get_blocked_network_mode_reason(Some("none"), false), None);
    }

    #[test]
    fn custom_network_allowed() {
        assert_eq!(
            get_blocked_network_mode_reason(Some("my-custom-net"), false),
            None
        );
    }

    #[test]
    fn no_network_allowed() {
        assert_eq!(get_blocked_network_mode_reason(None, false), None);
    }

    // ====================================================================
    // validate_config (v2026.3.11 parity)
    // ====================================================================

    #[test]
    fn valid_default_config_has_no_errors() {
        let config = Config::default();
        let errors = validate_config(&config);
        assert!(errors.is_empty());
    }

    #[test]
    fn zero_port_is_invalid() {
        let mut config = Config::default();
        config.gateway.port = 0;
        let errors = validate_config(&config);
        assert!(errors.iter().any(|e| e.path == "gateway.port"));
    }

    #[test]
    fn password_auth_without_password_is_invalid() {
        let mut config = Config::default();
        config.gateway.auth.mode = GatewayAuthMode::Password;
        config.gateway.auth.password = None;
        let errors = validate_config(&config);
        assert!(errors.iter().any(|e| e.path == "gateway.auth.password"));
    }

    #[test]
    fn password_auth_with_password_is_valid() {
        let mut config = Config::default();
        config.gateway.auth.mode = GatewayAuthMode::Password;
        config.gateway.auth.password = Some("my-pass".to_string());
        let errors = validate_config(&config);
        assert!(!errors.iter().any(|e| e.path == "gateway.auth.password"));
    }

    #[test]
    fn empty_provider_base_url_is_invalid() {
        let mut config = Config::default();
        config.models.providers.insert(
            "test".to_string(),
            crate::config::types::ModelProviderConfig {
                base_url: "".to_string(),
                api_key: Some("key".to_string()),
                auth: None,
                api: None,
                headers: None,
                auth_header: None,
                models: vec![],
            },
        );
        let errors = validate_config(&config);
        assert!(errors
            .iter()
            .any(|e| e.path == "models.providers.test.baseUrl"));
    }

    #[test]
    fn multiple_errors_accumulate() {
        let mut config = Config::default();
        config.gateway.port = 0;
        config.gateway.auth.mode = GatewayAuthMode::Password;
        config.gateway.auth.password = None;
        let errors = validate_config(&config);
        assert!(errors.len() >= 2);
    }

    #[test]
    fn config_validation_error_display() {
        let err = ConfigValidationError {
            path: "gateway.port".to_string(),
            message: "Port must be greater than 0".to_string(),
        };
        assert_eq!(err.to_string(), "gateway.port: Port must be greater than 0");
    }

    #[test]
    fn validate_config_object_ok() {
        let config = Config::default();
        assert!(validate_config_object(&config).is_ok());
    }

    #[test]
    fn validate_config_object_err() {
        let mut config = Config::default();
        config.gateway.port = 0;
        assert!(validate_config_object(&config).is_err());
    }
}
