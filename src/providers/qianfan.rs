//! Baidu Qianfan provider metadata (v2026.5.2).
//!
//! Chat routing already flows through the shared OpenAI-compatible path
//! (`OPENAI_COMPAT_PROVIDERS` in `providers/mod.rs`). This module exposes the
//! manifest-declared setup auth metadata and model catalog so onboarding and
//! `models list` surfaces resolve the expected env var / catalog rows without
//! legacy runtime seed data.

use super::manifest::{self, ManifestModel};

/// Default Qianfan OpenAI-compatible endpoint.
pub const QIANFAN_DEFAULT_BASE_URL: &str = "https://qianfan.baidubce.com/v2";

/// Manifest-driven Qianfan model catalog.
pub fn qianfan_catalog() -> &'static [ManifestModel] {
    manifest::manifest_models("qianfan")
}

/// Auth methods declared in the Qianfan manifest (`["api-key"]`).
pub fn qianfan_auth_methods() -> &'static [&'static str] {
    manifest::manifest_for("qianfan")
        .map(|m| m.auth.methods)
        .unwrap_or(&[])
}

/// Auth env vars declared in the Qianfan manifest (`["QIANFAN_API_KEY"]`).
pub fn qianfan_auth_env_vars() -> &'static [&'static str] {
    manifest::manifest_auth_env_vars("qianfan")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qianfan_auth_metadata_from_manifest() {
        assert_eq!(qianfan_auth_methods(), &["api-key"]);
        assert_eq!(qianfan_auth_env_vars(), &["QIANFAN_API_KEY"]);
    }

    #[test]
    fn qianfan_catalog_has_current_models() {
        let catalog = qianfan_catalog();
        assert!(catalog.iter().any(|m| m.id == "deepseek-v3.2"));
        assert!(catalog.iter().any(|m| m.id == "ernie-5.0-thinking-preview"));
    }

    #[test]
    fn qianfan_base_url_matches_manifest() {
        assert_eq!(
            manifest::manifest_for("qianfan").unwrap().base_url,
            QIANFAN_DEFAULT_BASE_URL
        );
    }
}
