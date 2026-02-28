//! Environment variable secret provider.
//!
//! Resolves `$ENV{VAR_NAME}` references by reading from the process environment.

use super::types::{SecretProvider, SecretRefKind, SecretResolution};
use async_trait::async_trait;

/// Resolves secrets from environment variables.
pub struct EnvSecretProvider;

impl EnvSecretProvider {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SecretProvider for EnvSecretProvider {
    fn kind(&self) -> SecretRefKind {
        SecretRefKind::Env
    }

    fn name(&self) -> &str {
        "env"
    }

    async fn resolve(&self, key: &str) -> SecretResolution {
        match std::env::var(key) {
            Ok(value) if !value.is_empty() => SecretResolution::Resolved(value),
            Ok(_) => SecretResolution::NotFound(format!(
                "Environment variable '{key}' is set but empty"
            )),
            Err(_) => SecretResolution::NotFound(format!(
                "Environment variable '{key}' is not set"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_existing_env_var() {
        // PATH should always exist
        let provider = EnvSecretProvider::new();
        let result = provider.resolve("PATH").await;
        assert!(result.is_resolved());
    }

    #[tokio::test]
    async fn resolve_missing_env_var() {
        let provider = EnvSecretProvider::new();
        let result = provider
            .resolve("MYLOBSTER_DEFINITELY_DOES_NOT_EXIST_XYZ")
            .await;
        assert!(!result.is_resolved());
        assert!(result
            .error_message()
            .unwrap()
            .contains("not set"));
    }
}
