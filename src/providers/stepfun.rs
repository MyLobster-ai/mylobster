//! Stepfun provider metadata (v2026.5.2).
//!
//! Chat routing flows through the shared OpenAI-compatible path; this module
//! exposes the manifest-declared setup auth metadata (`api-key` method,
//! `STEPFUN_API_KEY`) and the model catalog, mirroring the upstream move away
//! from legacy `providerAuthEnvVars` runtime seed data.

use super::manifest::{self, ManifestModel};

/// Default Stepfun OpenAI-compatible endpoint.
pub const STEPFUN_DEFAULT_BASE_URL: &str = "https://api.stepfun.ai/v1";

/// Stepfun Coding Plan endpoint (the `stepfun-plan` manifest provider).
pub const STEPFUN_PLAN_BASE_URL: &str = "https://api.stepfun.ai/step_plan/v1";

/// Manifest-driven Stepfun model catalog.
pub fn stepfun_catalog() -> &'static [ManifestModel] {
    manifest::manifest_models("stepfun")
}

/// Auth methods declared in the Stepfun manifest (`["api-key"]`).
pub fn stepfun_auth_methods() -> &'static [&'static str] {
    manifest::manifest_for("stepfun")
        .map(|m| m.auth.methods)
        .unwrap_or(&[])
}

/// Auth env vars declared in the Stepfun manifest (`["STEPFUN_API_KEY"]`).
pub fn stepfun_auth_env_vars() -> &'static [&'static str] {
    manifest::manifest_auth_env_vars("stepfun")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stepfun_auth_metadata_from_manifest() {
        assert_eq!(stepfun_auth_methods(), &["api-key"]);
        assert_eq!(stepfun_auth_env_vars(), &["STEPFUN_API_KEY"]);
    }

    #[test]
    fn stepfun_catalog_has_step_3_5_flash() {
        assert!(stepfun_catalog().iter().any(|m| m.id == "step-3.5-flash"));
    }

    #[test]
    fn stepfun_base_urls() {
        assert_eq!(
            manifest::manifest_for("stepfun").unwrap().base_url,
            STEPFUN_DEFAULT_BASE_URL
        );
        assert!(STEPFUN_PLAN_BASE_URL.contains("step_plan"));
    }
}
