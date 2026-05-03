//! Static model catalogs for bundled providers (v2026.4.25-29).
//!
//! OpenClaw v2026.4.x moved provider-specific model metadata out of inline
//! resolution code into reusable catalog builders. This module is the
//! mylobster equivalent: literal model references that the model picker
//! and onboarding flows can enumerate without making HTTP calls.
//!
//! Seeded for the bundled providers added in v2026.4.25-29:
//! - NVIDIA (v2026.4.29 — static catalog metadata)
//! - Cerebras (v2026.4.26 — static catalog)
//! - DeepInfra (v2026.4.27 — chat models; image/audio/TTS/embedding tracked
//!   separately when those subsystems land)

/// A single entry in a provider's static model catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCatalogEntry {
    /// Canonical model identifier as accepted by the provider.
    pub name: &'static str,
    /// Provider identifier matching `OPENAI_COMPAT_PROVIDERS` / detect_provider.
    pub provider: &'static str,
    /// Maximum context window in tokens (None when unspecified).
    pub context_length: Option<u32>,
    /// Whether the model supports OpenAI-style tool/function calling.
    pub supports_tools: bool,
    /// Whether the model accepts image inputs (vision).
    pub supports_vision: bool,
    /// Whether the model exposes a thinking/reasoning channel.
    pub supports_thinking: bool,
}

/// NVIDIA NIM static catalog (v2026.4.29).
///
/// Names are accepted via the provider prefix `nvidia/...` and resolve through
/// `https://integrate.api.nvidia.com/v1` as an OpenAI-compatible endpoint.
pub const NVIDIA_CATALOG: &[ModelCatalogEntry] = &[
    ModelCatalogEntry {
        name: "nvidia/nemotron-4-340b-instruct",
        provider: "nvidia",
        context_length: Some(4096),
        supports_tools: true,
        supports_vision: false,
        supports_thinking: false,
    },
    ModelCatalogEntry {
        name: "nvidia/llama-3.3-nemotron-super-49b-v1",
        provider: "nvidia",
        context_length: Some(128_000),
        supports_tools: true,
        supports_vision: false,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "nvidia/llama-3.1-nemotron-70b-instruct",
        provider: "nvidia",
        context_length: Some(128_000),
        supports_tools: true,
        supports_vision: false,
        supports_thinking: false,
    },
    ModelCatalogEntry {
        name: "meta/llama-3.3-70b-instruct",
        provider: "nvidia",
        context_length: Some(128_000),
        supports_tools: true,
        supports_vision: false,
        supports_thinking: false,
    },
    ModelCatalogEntry {
        name: "meta/llama-3.1-405b-instruct",
        provider: "nvidia",
        context_length: Some(128_000),
        supports_tools: true,
        supports_vision: false,
        supports_thinking: false,
    },
];

/// Cerebras static catalog (v2026.4.26).
///
/// Models hosted on `https://api.cerebras.ai/v1`. Cerebras runs Llama family
/// models on its inference cluster; tool support landed across 2025.
pub const CEREBRAS_CATALOG: &[ModelCatalogEntry] = &[
    ModelCatalogEntry {
        name: "llama3.1-8b",
        provider: "cerebras",
        context_length: Some(128_000),
        supports_tools: true,
        supports_vision: false,
        supports_thinking: false,
    },
    ModelCatalogEntry {
        name: "llama-3.3-70b",
        provider: "cerebras",
        context_length: Some(128_000),
        supports_tools: true,
        supports_vision: false,
        supports_thinking: false,
    },
    ModelCatalogEntry {
        name: "llama-4-scout-17b-16e-instruct",
        provider: "cerebras",
        context_length: Some(128_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: false,
    },
];

/// DeepInfra static catalog (v2026.4.27 — chat models only).
///
/// DeepInfra hosts a wide range of OAI-compatible models at
/// `https://api.deepinfra.com/v1/openai`. Image generation/editing, audio
/// understanding, TTS, and embeddings are tracked separately when those
/// subsystems land in mylobster.
pub const DEEPINFRA_CATALOG: &[ModelCatalogEntry] = &[
    ModelCatalogEntry {
        name: "meta-llama/Llama-3.3-70B-Instruct",
        provider: "deepinfra",
        context_length: Some(128_000),
        supports_tools: true,
        supports_vision: false,
        supports_thinking: false,
    },
    ModelCatalogEntry {
        name: "meta-llama/Meta-Llama-3.1-405B-Instruct",
        provider: "deepinfra",
        context_length: Some(128_000),
        supports_tools: true,
        supports_vision: false,
        supports_thinking: false,
    },
    ModelCatalogEntry {
        name: "Qwen/Qwen2.5-72B-Instruct",
        provider: "deepinfra",
        context_length: Some(32_768),
        supports_tools: true,
        supports_vision: false,
        supports_thinking: false,
    },
    ModelCatalogEntry {
        name: "deepseek-ai/DeepSeek-V3",
        provider: "deepinfra",
        context_length: Some(64_000),
        supports_tools: true,
        supports_vision: false,
        supports_thinking: false,
    },
];

/// xAI Grok static catalog (v2026.5.2).
///
/// Hosted at `https://api.x.ai/v1` (OpenAI-compatible). v5.2 introduced
/// Grok 4.3 as the default chat model alongside the existing Grok 2/3/4
/// family. Tool use and vision are supported on the recent series.
pub const XAI_CATALOG: &[ModelCatalogEntry] = &[
    ModelCatalogEntry {
        name: "grok-4.3",
        provider: "xai",
        context_length: Some(256_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "grok-4",
        provider: "xai",
        context_length: Some(256_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "grok-3",
        provider: "xai",
        context_length: Some(131_072),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: false,
    },
    ModelCatalogEntry {
        name: "grok-2",
        provider: "xai",
        context_length: Some(131_072),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: false,
    },
];

/// Default xAI chat model (v2026.5.2 promoted Grok 4.3 to default).
pub const XAI_DEFAULT_MODEL: &str = "grok-4.3";

/// Concatenation of every bundled provider catalog. Use for global lookup by
/// canonical model name, e.g. via `find_model`.
pub const ALL_CATALOGS: &[&[ModelCatalogEntry]] =
    &[NVIDIA_CATALOG, CEREBRAS_CATALOG, DEEPINFRA_CATALOG, XAI_CATALOG];

/// Look up a model by its canonical name across every bundled catalog.
///
/// Returns the first matching entry. Catalog order is NVIDIA → Cerebras →
/// DeepInfra; no provider currently has a name collision with another, but
/// callers needing specificity should filter by provider after lookup.
pub fn find_model(name: &str) -> Option<&'static ModelCatalogEntry> {
    ALL_CATALOGS
        .iter()
        .flat_map(|c| c.iter())
        .find(|entry| entry.name == name)
}

/// Look up models for a given provider name across every bundled catalog.
pub fn models_for_provider(provider: &str) -> Vec<&'static ModelCatalogEntry> {
    ALL_CATALOGS
        .iter()
        .flat_map(|c| c.iter())
        .filter(|entry| entry.provider == provider)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_catalog_non_empty() {
        assert!(!NVIDIA_CATALOG.is_empty());
    }

    #[test]
    fn nvidia_catalog_all_have_nvidia_provider() {
        for entry in NVIDIA_CATALOG {
            assert_eq!(entry.provider, "nvidia", "entry {:?} has wrong provider", entry);
        }
    }

    #[test]
    fn cerebras_catalog_non_empty() {
        assert!(!CEREBRAS_CATALOG.is_empty());
    }

    #[test]
    fn cerebras_catalog_all_have_cerebras_provider() {
        for entry in CEREBRAS_CATALOG {
            assert_eq!(entry.provider, "cerebras", "entry {:?} has wrong provider", entry);
        }
    }

    #[test]
    fn deepinfra_catalog_non_empty() {
        assert!(!DEEPINFRA_CATALOG.is_empty());
    }

    #[test]
    fn deepinfra_catalog_all_have_deepinfra_provider() {
        for entry in DEEPINFRA_CATALOG {
            assert_eq!(entry.provider, "deepinfra", "entry {:?} has wrong provider", entry);
        }
    }

    #[test]
    fn nvidia_includes_nemotron_and_llama() {
        let names: Vec<&str> = NVIDIA_CATALOG.iter().map(|e| e.name).collect();
        assert!(names.iter().any(|n| n.contains("nemotron")));
        assert!(names.iter().any(|n| n.contains("llama")));
    }

    #[test]
    fn find_model_locates_known_nvidia() {
        let entry = find_model("nvidia/nemotron-4-340b-instruct");
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().provider, "nvidia");
    }

    #[test]
    fn find_model_locates_known_cerebras() {
        let entry = find_model("llama-3.3-70b");
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().provider, "cerebras");
    }

    #[test]
    fn find_model_locates_known_deepinfra() {
        let entry = find_model("Qwen/Qwen2.5-72B-Instruct");
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().provider, "deepinfra");
    }

    #[test]
    fn find_model_unknown_returns_none() {
        assert!(find_model("nonexistent-model-xyz").is_none());
    }

    #[test]
    fn models_for_nvidia_returns_only_nvidia_entries() {
        let entries = models_for_provider("nvidia");
        assert!(!entries.is_empty());
        for e in &entries {
            assert_eq!(e.provider, "nvidia");
        }
    }

    #[test]
    fn models_for_unknown_provider_returns_empty() {
        let entries = models_for_provider("nonexistent");
        assert!(entries.is_empty());
    }

    #[test]
    fn nvidia_thinking_capability_present() {
        // v2026.4.29 catalog notes that nemotron-super-49b-v1 has reasoning.
        let entry = find_model("nvidia/llama-3.3-nemotron-super-49b-v1").unwrap();
        assert!(entry.supports_thinking);
    }

    #[test]
    fn cerebras_vision_capability_present() {
        let entry = find_model("llama-4-scout-17b-16e-instruct").unwrap();
        assert!(entry.supports_vision);
    }

    #[test]
    fn no_catalog_entries_have_empty_name() {
        for catalog in ALL_CATALOGS {
            for entry in catalog.iter() {
                assert!(!entry.name.is_empty(), "empty name in {:?}", entry);
                assert!(!entry.provider.is_empty(), "empty provider in {:?}", entry);
            }
        }
    }

    #[test]
    fn xai_catalog_includes_grok_4_3_default() {
        let entry = find_model(XAI_DEFAULT_MODEL).expect("default xAI model present");
        assert_eq!(entry.provider, "xai");
        assert!(entry.supports_tools);
        assert!(entry.supports_vision);
    }

    #[test]
    fn xai_catalog_default_is_grok_4_3() {
        assert_eq!(XAI_DEFAULT_MODEL, "grok-4.3");
    }

    #[test]
    fn xai_catalog_all_have_xai_provider() {
        for entry in XAI_CATALOG {
            assert_eq!(entry.provider, "xai");
        }
    }
}
