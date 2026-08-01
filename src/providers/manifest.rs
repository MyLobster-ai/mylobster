//! Manifest-backed provider catalog layer (v2026.4.27 → v2026.5.2).
//!
//! OpenClaw moved bundled provider metadata (model catalogs, setup auth
//! metadata, model aliases, and model suppressions) out of inline runtime
//! seed data into per-plugin manifests (`extensions/*/openclaw.plugin.json`).
//! This module is the mylobster equivalent: a static, data-driven registry of
//! bundled provider manifests that the model picker, onboarding flows, and
//! provider resolution consult without HTTP calls or duplicated runtime seeds.
//!
//! Bundled manifests mirror the upstream v2026.7.1 state for:
//! Z.AI (GLM catalog), Qianfan, Stepfun (+ stepfun-plan), DeepInfra, NVIDIA,
//! Xiaomi (+ token plan), Cerebras, Mistral, Moonshot, and DeepSeek.

/// One model row in a provider manifest catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct ManifestModel {
    pub id: &'static str,
    pub name: &'static str,
    /// Whether the model exposes a reasoning/thinking channel.
    pub reasoning: bool,
    /// Whether the model accepts image input.
    pub vision: bool,
    pub context_window: u64,
    pub max_tokens: u64,
    /// Per-model wire-API override (v2026.6.x manifest hygiene); falls back
    /// to the provider manifest's `api` when `None`.
    pub api: Option<&'static str>,
    /// Per-model base-URL override; falls back to the provider `base_url`.
    pub base_url: Option<&'static str>,
}

/// Setup/auth metadata declared by a provider manifest (v2026.5.2:
/// "declare setup auth metadata (`api-key` method, env vars) in the plugin
/// manifest so onboarding surfaces the expected env var without legacy
/// `providerAuthEnvVars` runtime seed data").
#[derive(Debug, Clone, PartialEq)]
pub struct ManifestAuth {
    /// Auth methods, e.g. `["api-key"]`.
    pub methods: &'static [&'static str],
    /// Environment variables accepted for the API key, in priority order.
    pub env_vars: &'static [&'static str],
}

/// A bundled provider manifest.
#[derive(Debug, Clone)]
pub struct ProviderManifest {
    pub id: &'static str,
    pub base_url: &'static str,
    /// Wire API family (all bundled catalogs are OpenAI-completions today).
    pub api: &'static str,
    pub auth: ManifestAuth,
    pub models: &'static [ManifestModel],
}

macro_rules! mm {
    ($id:expr, $name:expr, $reasoning:expr, $vision:expr, $ctx:expr, $max:expr) => {
        ManifestModel {
            id: $id,
            name: $name,
            reasoning: $reasoning,
            vision: $vision,
            context_window: $ctx,
            max_tokens: $max,
            api: None,
            base_url: None,
        }
    };
}

/// Z.AI manifest catalog (v2026.5.2 — moved runtime seed into manifest).
pub const ZAI_MANIFEST_MODELS: &[ManifestModel] = &[
    mm!("glm-5.2", "GLM-5.2", true, false, 1_000_000, 131_072),
    mm!("glm-5.1", "GLM-5.1", true, false, 202_800, 131_100),
    mm!("glm-5", "GLM-5", true, false, 202_800, 131_100),
    mm!("glm-5-turbo", "GLM-5 Turbo", true, false, 202_800, 131_100),
    mm!("glm-5v-turbo", "GLM-5V Turbo", true, true, 202_800, 131_100),
    mm!("glm-4.7", "GLM-4.7", true, false, 204_800, 131_072),
    mm!("glm-4.7-flash", "GLM-4.7 Flash", true, false, 200_000, 131_072),
    mm!("glm-4.7-flashx", "GLM-4.7 FlashX", true, false, 200_000, 128_000),
    mm!("glm-4.6", "GLM-4.6", true, false, 204_800, 131_072),
    mm!("glm-4.6v", "GLM-4.6V", true, true, 128_000, 32_768),
    mm!("glm-4.5", "GLM-4.5", true, false, 131_072, 98_304),
    mm!("glm-4.5-air", "GLM-4.5 Air", true, false, 131_072, 98_304),
    mm!("glm-4.5-flash", "GLM-4.5 Flash", true, false, 131_072, 98_304),
    mm!("glm-4.5v", "GLM-4.5V", true, true, 64_000, 16_384),
];

/// Qianfan manifest catalog (v2026.4.27 manifest-backed catalog).
pub const QIANFAN_MANIFEST_MODELS: &[ManifestModel] = &[
    mm!("deepseek-v3.2", "DeepSeek V3.2", true, false, 98_304, 32_768),
    mm!(
        "ernie-5.0-thinking-preview",
        "ERNIE 5.0 Thinking Preview",
        true,
        true,
        119_000,
        64_000
    ),
];

/// Stepfun manifest catalog.
pub const STEPFUN_MANIFEST_MODELS: &[ManifestModel] = &[mm!(
    "step-3.5-flash",
    "Step 3.5 Flash",
    true,
    false,
    262_144,
    65_536
)];

/// DeepInfra manifest catalog (v2026.5.2 — runtime fallback catalog derives
/// from the manifest instead of duplicated static model data).
pub const DEEPINFRA_MANIFEST_MODELS: &[ManifestModel] = &[
    mm!(
        "deepseek-ai/DeepSeek-V4-Flash",
        "DeepSeek V4 Flash",
        true,
        false,
        1_048_576,
        1_048_576
    ),
    mm!(
        "deepseek-ai/DeepSeek-V3.2",
        "DeepSeek V3.2",
        false,
        false,
        163_840,
        163_840
    ),
    mm!("zai-org/GLM-5.1", "GLM-5.1", true, false, 202_752, 202_752),
    mm!(
        "stepfun-ai/Step-3.5-Flash",
        "Step 3.5 Flash",
        false,
        false,
        262_144,
        262_144
    ),
    mm!(
        "MiniMaxAI/MiniMax-M2.5",
        "MiniMax M2.5",
        true,
        false,
        196_608,
        196_608
    ),
    mm!(
        "moonshotai/Kimi-K2.5",
        "Kimi K2.5",
        true,
        true,
        262_144,
        262_144
    ),
    mm!(
        "nvidia/NVIDIA-Nemotron-3-Super-120B-A12B",
        "Nemotron 3 Super 120B",
        true,
        false,
        262_144,
        262_144
    ),
    mm!(
        "meta-llama/Llama-3.3-70B-Instruct-Turbo",
        "Llama 3.3 70B Turbo",
        false,
        false,
        131_072,
        131_072
    ),
];

/// NVIDIA manifest catalog (v2026.4.29 static catalog metadata, refreshed to
/// the current upstream rows).
pub const NVIDIA_MANIFEST_MODELS: &[ManifestModel] = &[
    mm!(
        "nvidia/nemotron-3-ultra-550b-a55b",
        "Nemotron 3 Ultra 550B",
        false,
        false,
        1_000_000,
        16_384
    ),
    mm!(
        "nvidia/nemotron-3-super-120b-a12b",
        "Nemotron 3 Super 120B",
        false,
        false,
        1_048_576,
        8_192
    ),
    mm!(
        "moonshotai/kimi-k2.5",
        "Kimi K2.5",
        false,
        false,
        262_144,
        8_192
    ),
    mm!(
        "minimaxai/minimax-m2.7",
        "MiniMax M2.7",
        false,
        false,
        196_608,
        8_192
    ),
    mm!("z-ai/glm-5.1", "GLM-5.1", false, false, 202_752, 8_192),
    mm!(
        "minimaxai/minimax-m2.5",
        "MiniMax M2.5",
        false,
        false,
        196_608,
        8_192
    ),
    mm!("z-ai/glm5", "GLM-5", false, false, 202_752, 8_192),
];

/// Xiaomi MiMo manifest catalog.
pub const XIAOMI_MANIFEST_MODELS: &[ManifestModel] = &[
    mm!("mimo-v2-flash", "MiMo V2 Flash", false, false, 262_144, 8_192),
    mm!("mimo-v2-pro", "MiMo V2 Pro", true, false, 1_048_576, 32_000),
    mm!("mimo-v2-omni", "MiMo V2 Omni", true, true, 262_144, 32_000),
];

/// Cerebras manifest catalog.
pub const CEREBRAS_MANIFEST_MODELS: &[ManifestModel] = &[
    mm!("zai-glm-4.7", "GLM-4.7", true, false, 128_000, 8_192),
    mm!("gpt-oss-120b", "GPT-OSS 120B", true, false, 128_000, 8_192),
    mm!(
        "qwen-3-235b-a22b-instruct-2507",
        "Qwen3 235B Instruct",
        false,
        false,
        128_000,
        8_192
    ),
    mm!("llama3.1-8b", "Llama 3.1 8B", false, false, 128_000, 8_192),
];

/// Mistral manifest catalog.
pub const MISTRAL_MANIFEST_MODELS: &[ManifestModel] = &[
    mm!("codestral-latest", "Codestral", false, false, 256_000, 4_096),
    mm!(
        "devstral-medium-latest",
        "Devstral Medium",
        false,
        false,
        262_144,
        32_768
    ),
    mm!("magistral-small", "Magistral Small", true, false, 128_000, 40_000),
    mm!(
        "mistral-large-latest",
        "Mistral Large",
        false,
        true,
        262_144,
        16_384
    ),
    mm!(
        "mistral-medium-2508",
        "Mistral Medium 2508",
        false,
        true,
        262_144,
        8_192
    ),
    mm!(
        "mistral-medium-3-5",
        "Mistral Medium 3.5",
        true,
        true,
        262_144,
        8_192
    ),
    mm!(
        "mistral-small-latest",
        "Mistral Small",
        true,
        true,
        128_000,
        16_384
    ),
    mm!(
        "pixtral-large-latest",
        "Pixtral Large",
        false,
        true,
        128_000,
        32_768
    ),
];

/// Moonshot manifest catalog.
pub const MOONSHOT_MANIFEST_MODELS: &[ManifestModel] = &[
    mm!("kimi-k2.6", "Kimi K2.6", false, true, 262_144, 262_144),
    mm!("kimi-k2.7-code", "Kimi K2.7 Code", true, true, 262_144, 262_144),
    mm!("kimi-k2.5", "Kimi K2.5", false, true, 262_144, 262_144),
    mm!(
        "kimi-k2-thinking",
        "Kimi K2 Thinking",
        true,
        false,
        262_144,
        262_144
    ),
    mm!(
        "kimi-k2-thinking-turbo",
        "Kimi K2 Thinking Turbo",
        true,
        false,
        262_144,
        262_144
    ),
    mm!("kimi-k2-turbo", "Kimi K2 Turbo", false, false, 256_000, 16_384),
];

/// Featherless manifest catalog (v2026.6.x new provider; bundled-native in
/// mylobster — upstream ships it as an npm plugin).
pub const FEATHERLESS_MANIFEST_MODELS: &[ManifestModel] = &[mm!(
    "Qwen/Qwen3-32B",
    "Qwen3 32B",
    true,
    false,
    32_768,
    4_096
)];

/// Tencent Hy3 manifest catalog (TokenHub route; bundled-native).
pub const TENCENT_MANIFEST_MODELS: &[ManifestModel] = &[
    mm!("hy3-preview", "Hunyuan 3 Preview", true, false, 256_000, 64_000),
    mm!("hy3", "Hunyuan 3", true, false, 256_000, 64_000),
];

/// LongCat manifest catalog (bundled-native).
pub const LONGCAT_MANIFEST_MODELS: &[ManifestModel] = &[mm!(
    "LongCat-2.0",
    "LongCat 2.0",
    true,
    false,
    1_048_576,
    131_072
)];

/// Cohere manifest catalog (OpenAI-compatibility route; bundled-native).
pub const COHERE_MANIFEST_MODELS: &[ManifestModel] = &[mm!(
    "command-a-03-2025",
    "Command A",
    false,
    false,
    256_000,
    8_000
)];

/// Meta manifest catalog (`muse-spark-1.1` on the Responses API;
/// bundled-native).
pub const META_MANIFEST_MODELS: &[ManifestModel] = &[mm!(
    "muse-spark-1.1",
    "Muse Spark 1.1",
    true,
    false,
    1_048_576,
    131_072
)];

/// DeepSeek manifest catalog.
pub const DEEPSEEK_MANIFEST_MODELS: &[ManifestModel] = &[
    mm!(
        "deepseek-v4-flash",
        "DeepSeek V4 Flash",
        true,
        false,
        1_000_000,
        384_000
    ),
    mm!(
        "deepseek-v4-pro",
        "DeepSeek V4 Pro",
        true,
        false,
        1_000_000,
        384_000
    ),
    mm!("deepseek-chat", "DeepSeek Chat", false, false, 131_072, 8_192),
    mm!(
        "deepseek-reasoner",
        "DeepSeek Reasoner",
        true,
        false,
        131_072,
        65_536
    ),
];

/// Every bundled provider manifest.
pub fn bundled_manifests() -> &'static [ProviderManifest] {
    static MANIFESTS: once_cell::sync::Lazy<Vec<ProviderManifest>> =
        once_cell::sync::Lazy::new(|| {
            vec![
                ProviderManifest {
                    id: "zai",
                    base_url: "https://api.z.ai/api/paas/v4",
                    api: "openai-completions",
                    auth: ManifestAuth {
                        methods: &["api-key"],
                        env_vars: &["ZAI_API_KEY", "Z_AI_API_KEY"],
                    },
                    models: ZAI_MANIFEST_MODELS,
                },
                ProviderManifest {
                    id: "qianfan",
                    base_url: "https://qianfan.baidubce.com/v2",
                    api: "openai-completions",
                    auth: ManifestAuth {
                        methods: &["api-key"],
                        env_vars: &["QIANFAN_API_KEY"],
                    },
                    models: QIANFAN_MANIFEST_MODELS,
                },
                ProviderManifest {
                    id: "stepfun",
                    base_url: "https://api.stepfun.ai/v1",
                    api: "openai-completions",
                    auth: ManifestAuth {
                        methods: &["api-key"],
                        env_vars: &["STEPFUN_API_KEY"],
                    },
                    models: STEPFUN_MANIFEST_MODELS,
                },
                ProviderManifest {
                    id: "deepinfra",
                    base_url: "https://api.deepinfra.com/v1/openai",
                    api: "openai-completions",
                    auth: ManifestAuth {
                        methods: &["api-key"],
                        env_vars: &["DEEPINFRA_API_KEY"],
                    },
                    models: DEEPINFRA_MANIFEST_MODELS,
                },
                ProviderManifest {
                    id: "nvidia",
                    base_url: "https://integrate.api.nvidia.com/v1",
                    api: "openai-completions",
                    auth: ManifestAuth {
                        methods: &["api-key"],
                        env_vars: &["NVIDIA_API_KEY"],
                    },
                    models: NVIDIA_MANIFEST_MODELS,
                },
                ProviderManifest {
                    id: "xiaomi",
                    base_url: "https://api.xiaomimimo.com/v1",
                    api: "openai-completions",
                    auth: ManifestAuth {
                        methods: &["api-key"],
                        env_vars: &["XIAOMI_API_KEY"],
                    },
                    models: XIAOMI_MANIFEST_MODELS,
                },
                ProviderManifest {
                    id: "cerebras",
                    base_url: "https://api.cerebras.ai/v1",
                    api: "openai-completions",
                    auth: ManifestAuth {
                        methods: &["api-key"],
                        env_vars: &["CEREBRAS_API_KEY"],
                    },
                    models: CEREBRAS_MANIFEST_MODELS,
                },
                ProviderManifest {
                    id: "mistral",
                    base_url: "https://api.mistral.ai/v1",
                    api: "openai-completions",
                    auth: ManifestAuth {
                        methods: &["api-key"],
                        env_vars: &["MISTRAL_API_KEY"],
                    },
                    models: MISTRAL_MANIFEST_MODELS,
                },
                ProviderManifest {
                    id: "moonshot",
                    base_url: "https://api.moonshot.ai/v1",
                    api: "openai-completions",
                    auth: ManifestAuth {
                        methods: &["api-key"],
                        env_vars: &["MOONSHOT_API_KEY", "KIMI_API_KEY"],
                    },
                    models: MOONSHOT_MANIFEST_MODELS,
                },
                ProviderManifest {
                    id: "deepseek",
                    base_url: "https://api.deepseek.com",
                    api: "openai-completions",
                    auth: ManifestAuth {
                        methods: &["api-key"],
                        env_vars: &["DEEPSEEK_API_KEY"],
                    },
                    models: DEEPSEEK_MANIFEST_MODELS,
                },
                // ---- v2026.6.x–7.1 new providers (bundled-native) ----
                ProviderManifest {
                    id: "featherless",
                    base_url: "https://api.featherless.ai/v1",
                    api: "openai-completions",
                    auth: ManifestAuth {
                        methods: &["api-key"],
                        env_vars: &["FEATHERLESS_API_KEY"],
                    },
                    models: FEATHERLESS_MANIFEST_MODELS,
                },
                ProviderManifest {
                    id: "tencent",
                    base_url: "https://tokenhub.tencentmaas.com/v1",
                    api: "openai-completions",
                    auth: ManifestAuth {
                        methods: &["api-key"],
                        env_vars: &["TOKENHUB_API_KEY"],
                    },
                    models: TENCENT_MANIFEST_MODELS,
                },
                ProviderManifest {
                    id: "longcat",
                    base_url: "https://api.longcat.chat/openai",
                    api: "openai-completions",
                    auth: ManifestAuth {
                        methods: &["api-key"],
                        env_vars: &["LONGCAT_API_KEY"],
                    },
                    models: LONGCAT_MANIFEST_MODELS,
                },
                ProviderManifest {
                    id: "cohere",
                    base_url: "https://api.cohere.ai/compatibility/v1",
                    api: "openai-completions",
                    auth: ManifestAuth {
                        methods: &["api-key"],
                        env_vars: &["COHERE_API_KEY"],
                    },
                    models: COHERE_MANIFEST_MODELS,
                },
                ProviderManifest {
                    id: "meta",
                    base_url: "https://api.meta.ai/v1",
                    api: "openai-responses",
                    auth: ManifestAuth {
                        methods: &["api-key"],
                        env_vars: &["MODEL_API_KEY"],
                    },
                    models: META_MANIFEST_MODELS,
                },
            ]
        });
    &MANIFESTS
}

// ============================================================================
// Manifest hygiene helpers (v2026.6.x)
// ============================================================================

/// Strip declared model-id prefixes tolerantly: prefix comparison ignores
/// surrounding whitespace and ASCII case, and the provider's own id followed
/// by `/` is always stripped (self-prefix strip).
pub fn strip_model_prefixes<'a>(
    model_id: &'a str,
    provider_id: &str,
    strip_prefixes: &[&str],
) -> &'a str {
    let trimmed = model_id.trim();
    let lowered = trimmed.to_ascii_lowercase();
    let self_prefix = format!("{}/", provider_id.trim().to_ascii_lowercase());
    if lowered.starts_with(&self_prefix) {
        return &trimmed[self_prefix.len()..];
    }
    for prefix in strip_prefixes {
        let normalized_prefix = prefix.trim().to_ascii_lowercase();
        if normalized_prefix.is_empty() {
            continue;
        }
        if lowered.starts_with(&normalized_prefix) {
            return &trimmed[normalized_prefix.len()..];
        }
    }
    trimmed
}

/// Resolve the effective wire API for a manifest model (per-model override
/// wins over the provider manifest default).
pub fn effective_model_api(manifest: &ProviderManifest, model: &ManifestModel) -> &'static str {
    model.api.unwrap_or(manifest.api)
}

/// Resolve the effective base URL for a manifest model (per-model override
/// wins over the provider manifest default).
pub fn effective_model_base_url(
    manifest: &ProviderManifest,
    model: &ManifestModel,
) -> &'static str {
    model.base_url.unwrap_or(manifest.base_url)
}

/// Look up a bundled provider manifest by provider id (case-insensitive).
pub fn manifest_for(provider: &str) -> Option<&'static ProviderManifest> {
    let normalized = provider.trim().to_ascii_lowercase();
    bundled_manifests().iter().find(|m| m.id == normalized)
}

/// Manifest catalog rows for a provider (empty when no manifest exists).
pub fn manifest_models(provider: &str) -> &'static [ManifestModel] {
    manifest_for(provider).map(|m| m.models).unwrap_or(&[])
}

/// Auth env vars declared in a provider's manifest (v2026.5.2 Qianfan/Stepfun
/// item — onboarding surfaces these without legacy runtime seed data).
pub fn manifest_auth_env_vars(provider: &str) -> &'static [&'static str] {
    manifest_for(provider).map(|m| m.auth.env_vars).unwrap_or(&[])
}

// ============================================================================
// modelCatalog.aliases / modelCatalog.suppressions (v2026.4.27 / v2026.4.29)
// ============================================================================

/// A model-catalog alias: an alternate provider id that resolves to a
/// canonical provider (optionally forcing a wire API).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCatalogAlias {
    pub alias: &'static str,
    pub provider: &'static str,
    pub api: Option<&'static str>,
}

/// Bundled catalog aliases (mirrors the upstream `modelCatalog.aliases`
/// declarations for bundled plugins).
pub const MODEL_CATALOG_ALIASES: &[ModelCatalogAlias] = &[
    ModelCatalogAlias {
        alias: "azure-openai-responses",
        provider: "openai",
        api: Some("azure-openai-responses"),
    },
    ModelCatalogAlias {
        alias: "kimi",
        provider: "moonshot",
        api: None,
    },
    ModelCatalogAlias {
        alias: "z-ai",
        provider: "zai",
        api: None,
    },
];

/// Resolve a provider alias to its canonical provider id. Returns the input
/// unchanged when no alias matches.
pub fn resolve_catalog_alias(provider: &str) -> &str {
    let normalized = provider.trim().to_ascii_lowercase();
    MODEL_CATALOG_ALIASES
        .iter()
        .find(|a| a.alias == normalized)
        .map(|a| a.provider)
        .unwrap_or(provider)
}

/// Conditions gating a suppression entry. When present, the suppression only
/// applies if the conditions match — and (v2026.4.29 #74451 semantics) it
/// remains bypassable by explicit user model configuration.
#[derive(Debug, Clone, Default)]
pub struct SuppressionWhen {
    /// Restrict to these base-URL hostnames (normalized lowercase).
    pub base_url_hosts: &'static [&'static str],
}

/// One manifest model-suppression entry.
#[derive(Debug, Clone)]
pub struct ModelSuppression {
    pub provider: &'static str,
    pub model: &'static str,
    pub reason: &'static str,
    pub when: Option<SuppressionWhen>,
}

/// Bundled suppressions (upstream `modelCatalog.suppressions`).
pub fn bundled_suppressions() -> &'static [ModelSuppression] {
    static SUPPRESSIONS: once_cell::sync::Lazy<Vec<ModelSuppression>> =
        once_cell::sync::Lazy::new(|| {
            vec![
                ModelSuppression {
                    provider: "openai",
                    model: "gpt-5.3-codex-spark",
                    reason: "gpt-5.3-codex-spark is available only through ChatGPT/Codex OAuth. \
                             OpenAI API-key auth cannot use this model.",
                    when: Some(SuppressionWhen {
                        base_url_hosts: &["api.openai.com"],
                    }),
                },
                ModelSuppression {
                    provider: "azure-openai-responses",
                    model: "gpt-5.3-codex-spark",
                    reason: "gpt-5.3-codex-spark is available only through ChatGPT/Codex OAuth. \
                             Azure/OpenAI API-key auth cannot use this model.",
                    when: None,
                },
            ]
        });
    &SUPPRESSIONS
}

/// Verdict returned when a model is suppressed by a manifest declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppressionVerdict {
    pub error_message: String,
}

fn normalize_host(base_url: &str) -> Option<String> {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return None;
    }
    url::Url::parse(trimmed)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase().trim_end_matches('.').to_string()))
}

/// Resolve whether a `provider/model` pair is suppressed by manifest
/// declarations (v2026.4.29 model suppression; #74451 semantics).
///
/// * Unconditional suppressions (no `when`) apply even when the model was
///   explicitly configured by the user — a stale inline entry cannot bypass
///   the manifest capability block.
/// * Conditional suppressions (with `when`) are bypassable by explicit user
///   configuration (`explicitly_configured = true`) and only match when
///   their conditions hold.
pub fn resolve_model_suppression(
    provider: &str,
    model: &str,
    base_url: Option<&str>,
    explicitly_configured: bool,
) -> Option<SuppressionVerdict> {
    let provider = provider.trim().to_ascii_lowercase();
    let model = model.trim().to_ascii_lowercase();
    if provider.is_empty() || model.is_empty() {
        return None;
    }
    for entry in bundled_suppressions() {
        if entry.provider != provider || entry.model != model {
            continue;
        }
        match &entry.when {
            None => {
                // Unconditional: not bypassable by explicit configuration.
                return Some(SuppressionVerdict {
                    error_message: format!(
                        "Unknown model: {}/{}. {}",
                        provider, model, entry.reason
                    ),
                });
            }
            Some(when) => {
                if explicitly_configured {
                    continue; // conditional suppressions are user-bypassable
                }
                let matches = if when.base_url_hosts.is_empty() {
                    true
                } else {
                    match base_url.and_then(normalize_host) {
                        // No base URL configured: default endpoint — matches.
                        None => true,
                        Some(host) => when
                            .base_url_hosts
                            .iter()
                            .any(|allowed| allowed.eq_ignore_ascii_case(&host)),
                    }
                };
                if matches {
                    return Some(SuppressionVerdict {
                        error_message: format!(
                            "Unknown model: {}/{}. {}",
                            provider, model, entry.reason
                        ),
                    });
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_manifests_have_models_and_env_vars() {
        for m in bundled_manifests() {
            assert!(!m.models.is_empty(), "{} manifest has no models", m.id);
            assert!(!m.auth.env_vars.is_empty(), "{} has no env vars", m.id);
            assert!(m.base_url.starts_with("https://"), "{}", m.id);
        }
    }

    #[test]
    fn zai_manifest_catalog_has_glm_family() {
        let models = manifest_models("zai");
        assert!(models.iter().any(|m| m.id == "glm-5.2"));
        assert!(models.iter().any(|m| m.id == "glm-4.7-flash"));
        assert!(models.iter().all(|m| m.id.starts_with("glm-")));
    }

    #[test]
    fn zai_manifest_env_vars_include_both_spellings() {
        let vars = manifest_auth_env_vars("zai");
        assert!(vars.contains(&"ZAI_API_KEY"));
        assert!(vars.contains(&"Z_AI_API_KEY"));
    }

    #[test]
    fn qianfan_stepfun_auth_metadata() {
        assert_eq!(manifest_auth_env_vars("qianfan"), &["QIANFAN_API_KEY"]);
        assert_eq!(manifest_auth_env_vars("stepfun"), &["STEPFUN_API_KEY"]);
        assert_eq!(
            manifest_for("qianfan").unwrap().auth.methods,
            &["api-key"]
        );
        assert_eq!(
            manifest_for("stepfun").unwrap().auth.methods,
            &["api-key"]
        );
    }

    #[test]
    fn deepinfra_manifest_catalog_discovery() {
        let models = manifest_models("deepinfra");
        assert!(models.iter().any(|m| m.id == "deepseek-ai/DeepSeek-V4-Flash"));
        assert!(models.iter().any(|m| m.id == "moonshotai/Kimi-K2.5"));
        let kimi = models.iter().find(|m| m.id == "moonshotai/Kimi-K2.5").unwrap();
        assert!(kimi.vision);
        assert!(kimi.reasoning);
    }

    #[test]
    fn nvidia_manifest_has_literal_model_refs() {
        let models = manifest_models("nvidia");
        // Literal refs keep the vendor namespace intact.
        assert!(models.iter().any(|m| m.id == "nvidia/nemotron-3-ultra-550b-a55b"));
        assert!(models.iter().any(|m| m.id == "z-ai/glm-5.1"));
    }

    #[test]
    fn manifest_lookup_case_insensitive() {
        assert!(manifest_for("ZAI").is_some());
        assert!(manifest_for("  DeepInfra  ").is_some());
        assert!(manifest_for("nonexistent").is_none());
    }

    #[test]
    fn unknown_provider_has_empty_models_and_env_vars() {
        assert!(manifest_models("nope").is_empty());
        assert!(manifest_auth_env_vars("nope").is_empty());
    }

    // ------------------------------------------------------------------
    // Aliases
    // ------------------------------------------------------------------

    #[test]
    fn alias_resolves_azure_openai_responses() {
        assert_eq!(resolve_catalog_alias("azure-openai-responses"), "openai");
    }

    #[test]
    fn alias_resolves_kimi_and_z_ai() {
        assert_eq!(resolve_catalog_alias("kimi"), "moonshot");
        assert_eq!(resolve_catalog_alias("z-ai"), "zai");
    }

    #[test]
    fn alias_passthrough_for_unknown() {
        assert_eq!(resolve_catalog_alias("anthropic"), "anthropic");
    }

    // ------------------------------------------------------------------
    // Suppressions (v2026.4.29 #74451 semantics)
    // ------------------------------------------------------------------

    #[test]
    fn unconditional_suppression_blocks_even_when_explicitly_configured() {
        let verdict = resolve_model_suppression(
            "azure-openai-responses",
            "gpt-5.3-codex-spark",
            None,
            true,
        );
        assert!(verdict.is_some());
        assert!(verdict.unwrap().error_message.contains("Unknown model"));
    }

    #[test]
    fn conditional_suppression_bypassable_by_explicit_config() {
        let verdict =
            resolve_model_suppression("openai", "gpt-5.3-codex-spark", None, true);
        assert!(verdict.is_none());
    }

    #[test]
    fn conditional_suppression_matches_default_endpoint() {
        let verdict =
            resolve_model_suppression("openai", "gpt-5.3-codex-spark", None, false);
        assert!(verdict.is_some());
    }

    #[test]
    fn conditional_suppression_matches_declared_host() {
        let verdict = resolve_model_suppression(
            "openai",
            "gpt-5.3-codex-spark",
            Some("https://api.openai.com/v1"),
            false,
        );
        assert!(verdict.is_some());
    }

    #[test]
    fn conditional_suppression_skips_custom_compat_endpoint() {
        let verdict = resolve_model_suppression(
            "openai",
            "gpt-5.3-codex-spark",
            Some("https://my-proxy.example.com/v1"),
            false,
        );
        assert!(verdict.is_none());
    }

    #[test]
    fn unrelated_models_never_suppressed() {
        assert!(resolve_model_suppression("openai", "gpt-4o", None, false).is_none());
        assert!(resolve_model_suppression("", "", None, false).is_none());
    }

    // ------------------------------------------------------------------
    // v2026.6.x–7.1 new providers (bundled-native)
    // ------------------------------------------------------------------

    #[test]
    fn new_provider_manifests_present() {
        for (id, env) in [
            ("featherless", "FEATHERLESS_API_KEY"),
            ("tencent", "TOKENHUB_API_KEY"),
            ("longcat", "LONGCAT_API_KEY"),
            ("cohere", "COHERE_API_KEY"),
            ("meta", "MODEL_API_KEY"),
        ] {
            let m = manifest_for(id).unwrap_or_else(|| panic!("{} missing", id));
            assert!(m.auth.env_vars.contains(&env), "{}", id);
            assert!(!m.models.is_empty(), "{}", id);
        }
    }

    #[test]
    fn tencent_hy3_and_longcat_models() {
        assert!(manifest_models("tencent").iter().any(|m| m.id == "hy3"));
        assert!(manifest_models("longcat").iter().any(|m| m.id == "LongCat-2.0"));
    }

    #[test]
    fn meta_muse_spark_uses_responses_api() {
        let m = manifest_for("meta").unwrap();
        assert_eq!(m.api, "openai-responses");
        let model = &m.models[0];
        assert_eq!(model.id, "muse-spark-1.1");
        assert_eq!(effective_model_api(m, model), "openai-responses");
    }

    // ------------------------------------------------------------------
    // Manifest hygiene (v2026.6.x)
    // ------------------------------------------------------------------

    #[test]
    fn strip_prefixes_tolerates_spaces_and_casing() {
        assert_eq!(
            strip_model_prefixes("  MoonshotAI/kimi-k2.5 ", "moonshot", &["moonshotai/"]),
            "kimi-k2.5"
        );
        assert_eq!(
            strip_model_prefixes("kimi-k2.5", "moonshot", &[" MOONSHOTAI/ "]),
            "kimi-k2.5"
        );
    }

    #[test]
    fn self_prefix_always_stripped() {
        assert_eq!(strip_model_prefixes("zai/glm-5.2", "zai", &[]), "glm-5.2");
        assert_eq!(strip_model_prefixes("ZAI/glm-5.2", "zai", &[]), "glm-5.2");
    }

    #[test]
    fn per_model_overrides_fall_back_to_provider_defaults() {
        let m = manifest_for("zai").unwrap();
        let model = &m.models[0];
        assert_eq!(effective_model_api(m, model), "openai-completions");
        assert_eq!(effective_model_base_url(m, model), m.base_url);
    }
}
