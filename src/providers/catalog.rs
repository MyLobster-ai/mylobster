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

/// xAI Grok static catalog (v2026.5.2, pruned at v2026.7.1).
///
/// Hosted at `https://api.x.ai/v1` (OpenAI-compatible). v5.2 introduced
/// Grok 4.3 as the default chat model; the v2026.7.1 refresh pruned the
/// retired Grok 2/3 and Grok 4 Fast rows (doctor migrates old refs — see
/// `upgrade_retired_model_ref`).
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
        name: "grok-build-0.1",
        provider: "xai",
        context_length: Some(256_000),
        supports_tools: true,
        supports_vision: false,
        supports_thinking: true,
    },
];

/// Anthropic static catalog (v2026.7.1 refresh: Fable 5, Sonnet 5, Mythos 5,
/// Opus 4.8, Haiku 4.5 join the 4.6/4.7 family; 5-series and Opus 4.8 carry
/// ~1M contexts after the 1M-context GA).
pub const ANTHROPIC_CATALOG: &[ModelCatalogEntry] = &[
    ModelCatalogEntry {
        name: "claude-fable-5",
        provider: "anthropic",
        context_length: Some(1_000_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "claude-sonnet-5",
        provider: "anthropic",
        context_length: Some(1_000_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "claude-mythos-5",
        provider: "anthropic",
        context_length: Some(1_000_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "claude-opus-4-8",
        provider: "anthropic",
        context_length: Some(1_048_576),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "claude-opus-4-7",
        provider: "anthropic",
        context_length: Some(200_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "claude-haiku-4-5",
        provider: "anthropic",
        context_length: Some(200_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "claude-sonnet-4-6",
        provider: "anthropic",
        context_length: Some(200_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "claude-opus-4-6",
        provider: "anthropic",
        context_length: Some(200_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
];

/// OpenAI static catalog (v2026.7.1 refresh: GPT-5.6 series is the
/// new-setup default and supports `/think ultra`/`max`; GPT-5.5, GPT-5.4
/// tiers, and GPT-5.3 Codex Spark round out the family).
pub const OPENAI_CATALOG: &[ModelCatalogEntry] = &[
    ModelCatalogEntry {
        name: "gpt-5.6",
        provider: "openai",
        context_length: Some(400_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "gpt-5.6-sol",
        provider: "openai",
        context_length: Some(400_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "gpt-5.6-terra",
        provider: "openai",
        context_length: Some(400_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "gpt-5.6-luna",
        provider: "openai",
        context_length: Some(400_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "gpt-5.5",
        provider: "openai",
        context_length: Some(400_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "gpt-5.5-pro",
        provider: "openai",
        context_length: Some(400_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "gpt-5.4",
        provider: "openai",
        context_length: Some(400_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "gpt-5.4-mini",
        provider: "openai",
        context_length: Some(400_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "gpt-5.4-nano",
        provider: "openai",
        context_length: Some(400_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "gpt-5.3-codex",
        provider: "openai",
        context_length: Some(400_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "gpt-5.3-codex-spark",
        provider: "openai",
        context_length: Some(400_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
];

/// Default OpenAI chat model for new setups (v2026.7.1: GPT-5.6 series).
pub const OPENAI_DEFAULT_MODEL: &str = "gpt-5.6";

/// Google static catalog rows (v2026.7.1 refresh: Gemini 3.5 Flash at 1M
/// context plus `gemini-3.1-flash-lite`). Catalog data only — Gemini
/// transport behavior lives in `gemini.rs` (other cluster).
pub const GOOGLE_CATALOG: &[ModelCatalogEntry] = &[
    ModelCatalogEntry {
        name: "gemini-3.5-flash",
        provider: "google",
        context_length: Some(1_000_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "gemini-3.1-flash-lite",
        provider: "google",
        context_length: Some(1_000_000),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "gemini-3.1-pro-preview",
        provider: "google",
        context_length: Some(1_048_576),
        supports_tools: true,
        supports_vision: true,
        supports_thinking: true,
    },
];

/// MiniMax static catalog (v2026.7.1: MiniMax M3).
pub const MINIMAX_CATALOG: &[ModelCatalogEntry] = &[
    ModelCatalogEntry {
        name: "minimax-m3",
        provider: "minimax",
        context_length: Some(196_608),
        supports_tools: true,
        supports_vision: false,
        supports_thinking: true,
    },
    ModelCatalogEntry {
        name: "minimax-m2.5",
        provider: "minimax",
        context_length: Some(196_608),
        supports_tools: true,
        supports_vision: false,
        supports_thinking: true,
    },
];

/// Default xAI chat model (v2026.5.2 promoted Grok 4.3 to default).
pub const XAI_DEFAULT_MODEL: &str = "grok-4.3";

/// Concatenation of every bundled provider catalog. Use for global lookup by
/// canonical model name, e.g. via `find_model`.
pub const ALL_CATALOGS: &[&[ModelCatalogEntry]] = &[
    NVIDIA_CATALOG,
    CEREBRAS_CATALOG,
    DEEPINFRA_CATALOG,
    XAI_CATALOG,
    ANTHROPIC_CATALOG,
    OPENAI_CATALOG,
    GOOGLE_CATALOG,
    MINIMAX_CATALOG,
];

// ============================================================================
// Retired-model migration (v2026.7.1 doctor migrations)
// ============================================================================

fn normalize_ref(model: &str) -> String {
    model.trim().to_ascii_lowercase()
}

/// Upgrade a retired model id to its current replacement (port of the
/// upstream doctor `legacy-config-migrations.runtime.models` maps at
/// v2026.7.1). Returns `None` when the model is current.
///
/// Covers: retired Grok Code Fast / Grok 4 Fast rows, retired Groq-hosted
/// rows, retired old-Claude generations (Opus/Sonnet ≤4.5 → 4.6; Haiku 4.5
/// is current and never migrated), and the retired GPT-4.x/5.0–5.2 family.
pub fn upgrade_retired_model_ref(provider: &str, model: &str) -> Option<&'static str> {
    let provider = provider.trim().to_ascii_lowercase();
    let normalized = normalize_ref(model);
    match provider.as_str() {
        "xai" | "grok" => match normalized.as_str() {
            "grok-code-fast" | "grok-code-fast-1" | "grok-code-fast-1-0825" => {
                Some("grok-build-0.1")
            }
            "grok-4-fast-reasoning" | "grok-4-1-fast-reasoning" => Some("grok-4.3"),
            _ => None,
        },
        "groq" => match normalized.as_str() {
            "deepseek-r1-distill-llama-70b" | "llama3-70b-8192" => {
                Some("llama-3.3-70b-versatile")
            }
            "gemma2-9b-it" | "llama3-8b-8192" => Some("llama-3.1-8b-instant"),
            "meta-llama/llama-4-maverick-17b-128e-instruct"
            | "moonshotai/kimi-k2-instruct"
            | "moonshotai/kimi-k2-instruct-0905" => Some("openai/gpt-oss-120b"),
            "mistral-saba-24b" | "qwen-qwq-32b" => Some("qwen/qwen3-32b"),
            _ => None,
        },
        "openai" | "openai-codex" => {
            let codex = provider == "openai-codex";
            if codex && normalized == "gpt-5.2" {
                return Some("gpt-5.5");
            }
            match normalized.as_str() {
                "gpt-5.2-codex" | "gpt-5.1-codex" | "gpt-5-codex" => {
                    Some(if codex { "gpt-5.5" } else { "gpt-5.3-codex" })
                }
                "gpt-5-pro" | "gpt-5.2-pro" => Some("gpt-5.5-pro"),
                "gpt-4.1-nano" | "gpt-5-nano" => {
                    Some(if codex { "gpt-5.4-mini" } else { "gpt-5.4-nano" })
                }
                "gpt-4.1-mini" | "gpt-4o-mini" | "gpt-5.1-codex-mini" | "gpt-5-mini" => {
                    Some("gpt-5.4-mini")
                }
                "gpt-4" | "gpt-4-turbo" | "gpt-4.1" | "gpt-4o" | "gpt-4o-2024-05-13"
                | "gpt-4o-2024-08-06" | "gpt-4o-2024-11-20" | "gpt-5" | "gpt-5-chat-latest"
                | "gpt-5.1" | "gpt-5.1-chat-latest" | "gpt-5.1-codex-max" | "gpt-5.2"
                | "gpt-5.2-chat-latest" => Some("gpt-5.5"),
                _ => None,
            }
        }
        "anthropic" => {
            // Haiku 4.5 is a current production model and must not migrate.
            if normalized.starts_with("claude-haiku-4-5")
                || normalized.starts_with("claude-haiku-4.5")
            {
                return None;
            }
            // Current 4.6+/5 generations are never migrated.
            for current in [
                "claude-opus-4-8",
                "claude-opus-4.8",
                "claude-opus-4-7",
                "claude-opus-4.7",
                "claude-opus-4-6",
                "claude-opus-4.6",
                "claude-sonnet-4-6",
                "claude-sonnet-4.6",
                "claude-sonnet-5",
                "claude-fable-5",
                "claude-mythos-5",
            ] {
                if normalized.starts_with(current) {
                    return None;
                }
            }
            let is_retired_opus = normalized == "claude-opus-4"
                || normalized.starts_with("claude-opus-4-5")
                || normalized.starts_with("claude-opus-4.5")
                || normalized.starts_with("claude-opus-4-1")
                || normalized.starts_with("claude-opus-4.1")
                || normalized.starts_with("claude-opus-4-0")
                || normalized.starts_with("claude-opus-4.0")
                || normalized.starts_with("claude-opus-4-20");
            if is_retired_opus {
                return Some("claude-opus-4-6");
            }
            let is_retired_sonnet = normalized == "claude-sonnet-4"
                || normalized.starts_with("claude-sonnet-4-5")
                || normalized.starts_with("claude-sonnet-4.5")
                || normalized.starts_with("claude-sonnet-4-0")
                || normalized.starts_with("claude-sonnet-4.0")
                || normalized.starts_with("claude-3-");
            if is_retired_sonnet {
                return Some("claude-sonnet-4-6");
            }
            None
        }
        _ => None,
    }
}

/// Whether a `provider/model` pair refers to a retired model that should be
/// suppressed from catalogs (doctor migrates configs via
/// `upgrade_retired_model_ref`).
pub fn is_retired_model_ref(provider: &str, model: &str) -> bool {
    upgrade_retired_model_ref(provider, model).is_some()
}

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

/// Literal model-ref picker (v2026.4.29 NVIDIA item).
///
/// NVIDIA (and other vendor-namespaced hosts) publish model ids that
/// themselves contain `/` (e.g. `meta/llama-3.1-405b-instruct`), so a
/// user-facing ref like `nvidia/meta/llama-3.1-405b-instruct` must be
/// resolved by peeling the *provider* prefix and keeping the literal
/// remainder intact. Accepts:
///
/// * `<provider>/<catalog-id>` — provider prefix + literal catalog id
/// * a bare catalog id (matched across all bundled catalogs)
///
/// Returns the matching catalog entry.
pub fn pick_literal_model_ref(model_ref: &str) -> Option<&'static ModelCatalogEntry> {
    let trimmed = model_ref.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Bare catalog id (may itself contain a vendor namespace).
    if let Some(entry) = find_model(trimmed) {
        return Some(entry);
    }
    // Provider-prefixed ref: peel the first segment as the provider and keep
    // the literal remainder as the catalog id.
    let (provider, remainder) = trimmed.split_once('/')?;
    ALL_CATALOGS
        .iter()
        .flat_map(|c| c.iter())
        .find(|entry| entry.provider == provider && entry.name == remainder)
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

    // ====================================================================
    // v2026.4.29 — literal model-ref picker (NVIDIA)
    // ====================================================================

    #[test]
    fn literal_ref_matches_bare_catalog_id() {
        let entry = pick_literal_model_ref("nvidia/nemotron-4-340b-instruct").unwrap();
        assert_eq!(entry.provider, "nvidia");
    }

    #[test]
    fn literal_ref_peels_provider_prefix_keeping_vendor_namespace() {
        // nvidia/<meta/llama...>: provider prefix peeled, literal id intact.
        let entry = pick_literal_model_ref("nvidia/meta/llama-3.1-405b-instruct").unwrap();
        assert_eq!(entry.provider, "nvidia");
        assert_eq!(entry.name, "meta/llama-3.1-405b-instruct");
    }

    #[test]
    fn literal_ref_provider_prefixed_vendor_id() {
        // Full form: nvidia/nvidia/nemotron... also resolves.
        let entry = pick_literal_model_ref("nvidia/nvidia/nemotron-4-340b-instruct").unwrap();
        assert_eq!(entry.name, "nvidia/nemotron-4-340b-instruct");
    }

    #[test]
    fn literal_ref_unknown_returns_none() {
        assert!(pick_literal_model_ref("nvidia/unknown/model").is_none());
        assert!(pick_literal_model_ref("").is_none());
        assert!(pick_literal_model_ref("no-such-model").is_none());
    }

    // ====================================================================
    // v2026.7.1 — catalog refresh + retired-model migration
    // ====================================================================

    #[test]
    fn anthropic_catalog_has_v7_1_family() {
        for id in [
            "claude-fable-5",
            "claude-sonnet-5",
            "claude-mythos-5",
            "claude-opus-4-8",
            "claude-haiku-4-5",
        ] {
            let entry = find_model(id).unwrap_or_else(|| panic!("{} missing", id));
            assert_eq!(entry.provider, "anthropic");
            assert!(entry.supports_thinking);
        }
    }

    #[test]
    fn five_series_carries_1m_context() {
        assert_eq!(find_model("claude-sonnet-5").unwrap().context_length, Some(1_000_000));
        assert_eq!(find_model("claude-opus-4-8").unwrap().context_length, Some(1_048_576));
    }

    #[test]
    fn openai_catalog_has_gpt_5_6_series_and_default() {
        assert_eq!(OPENAI_DEFAULT_MODEL, "gpt-5.6");
        for id in ["gpt-5.6", "gpt-5.6-sol", "gpt-5.5", "gpt-5.3-codex-spark"] {
            assert_eq!(find_model(id).unwrap().provider, "openai", "{}", id);
        }
    }

    #[test]
    fn google_and_minimax_rows_present() {
        assert_eq!(find_model("gemini-3.5-flash").unwrap().context_length, Some(1_000_000));
        assert!(find_model("gemini-3.1-flash-lite").is_some());
        assert_eq!(find_model("minimax-m3").unwrap().provider, "minimax");
    }

    #[test]
    fn retired_grok_rows_pruned_from_catalog() {
        assert!(find_model("grok-3").is_none());
        assert!(find_model("grok-2").is_none());
        assert!(find_model("grok-build-0.1").is_some());
    }

    #[test]
    fn retired_xai_refs_migrate() {
        assert_eq!(upgrade_retired_model_ref("xai", "grok-code-fast-1"), Some("grok-build-0.1"));
        assert_eq!(
            upgrade_retired_model_ref("xai", "grok-4-fast-reasoning"),
            Some("grok-4.3")
        );
        assert_eq!(upgrade_retired_model_ref("xai", "grok-4.3"), None);
    }

    #[test]
    fn retired_groq_refs_migrate() {
        assert_eq!(
            upgrade_retired_model_ref("groq", "llama3-70b-8192"),
            Some("llama-3.3-70b-versatile")
        );
        assert_eq!(
            upgrade_retired_model_ref("groq", "moonshotai/kimi-k2-instruct"),
            Some("openai/gpt-oss-120b")
        );
        assert_eq!(upgrade_retired_model_ref("groq", "llama-3.3-70b-versatile"), None);
    }

    #[test]
    fn retired_openai_refs_migrate_with_codex_provider_awareness() {
        assert_eq!(upgrade_retired_model_ref("openai", "gpt-4o"), Some("gpt-5.5"));
        assert_eq!(upgrade_retired_model_ref("openai", "gpt-5.2-codex"), Some("gpt-5.3-codex"));
        assert_eq!(
            upgrade_retired_model_ref("openai-codex", "gpt-5.2-codex"),
            Some("gpt-5.5")
        );
        assert_eq!(
            upgrade_retired_model_ref("openai-codex", "gpt-4.1-nano"),
            Some("gpt-5.4-mini")
        );
        assert_eq!(upgrade_retired_model_ref("openai", "gpt-4.1-nano"), Some("gpt-5.4-nano"));
        assert_eq!(upgrade_retired_model_ref("openai", "gpt-5.6"), None);
    }

    #[test]
    fn retired_claude_refs_migrate_but_current_families_do_not() {
        assert_eq!(
            upgrade_retired_model_ref("anthropic", "claude-opus-4-5"),
            Some("claude-opus-4-6")
        );
        assert_eq!(
            upgrade_retired_model_ref("anthropic", "claude-sonnet-4.5"),
            Some("claude-sonnet-4-6")
        );
        // Haiku 4.5 is current production — never migrated.
        assert_eq!(upgrade_retired_model_ref("anthropic", "claude-haiku-4-5"), None);
        assert_eq!(upgrade_retired_model_ref("anthropic", "claude-opus-4-8"), None);
        assert_eq!(upgrade_retired_model_ref("anthropic", "claude-sonnet-5"), None);
    }

    #[test]
    fn is_retired_model_ref_matches_upgrade_map() {
        assert!(is_retired_model_ref("openai", "gpt-4o"));
        assert!(!is_retired_model_ref("openai", "gpt-5.6"));
        assert!(!is_retired_model_ref("unknown-provider", "gpt-4o"));
    }
}
