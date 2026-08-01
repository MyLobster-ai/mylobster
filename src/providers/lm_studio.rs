//! LM Studio provider (v2026.5.2).
//!
//! LM Studio exposes an OpenAI-compatible inference API plus a native model
//! management API. Parity items covered here:
//!
//! * **Binary reasoning normalization** — LM Studio `/api/v1/models` entries
//!   advertise reasoning through `capabilities.reasoning`, either as an
//!   `allowed_options` list (Gemma 4 and friends expose binary `off`/`on`
//!   options) or a bare `default`. `resolve_reasoning_capability` normalizes
//!   both shapes; anything other than `off` counts as enabled, and missing
//!   metadata means no reasoning.
//! * **`params.preload: false`** — skips OpenClaw's native model-load call so
//!   LM Studio JIT loading, idle TTL, and auto-evict own the model lifecycle
//!   (issue #75921). `should_preload` reads the flag from
//!   `models.providers.lmstudio.params`.

use super::openai_compat;
use super::{ModelProvider, ProviderRequest, ProviderResponse, StreamEvent};
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Stable provider id.
pub const LMSTUDIO_PROVIDER_ID: &str = "lmstudio";

/// Default LM Studio server base (model management API lives under /api/v1).
pub const LMSTUDIO_DEFAULT_BASE_URL: &str = "http://127.0.0.1:1234";

/// Default OpenAI-compatible inference base URL.
pub const LMSTUDIO_DEFAULT_INFERENCE_BASE_URL: &str = "http://127.0.0.1:1234/v1";

/// Env var checked for LM Studio API tokens.
pub const LMSTUDIO_DEFAULT_API_KEY_ENV_VAR: &str = "LM_API_TOKEN";

/// Placeholder token for local servers that accept any API key.
pub const LMSTUDIO_LOCAL_API_KEY_PLACEHOLDER: &str = "lmstudio-local";

/// Default context length requested when loading models.
pub const LMSTUDIO_DEFAULT_LOAD_CONTEXT_LENGTH: u64 = 64_000;

fn normalize_reasoning_option(value: &Value) -> Option<String> {
    let s = value.as_str()?;
    let normalized = s.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn is_reasoning_enabled_option(value: &Value) -> bool {
    match normalize_reasoning_option(value) {
        Some(option) => option != "off",
        None => false,
    }
}

/// Resolve whether an LM Studio wire entry advertises reasoning support.
///
/// `entry` is one row of `/api/v1/models`. Reasoning metadata lives at
/// `capabilities.reasoning` as `{ allowed_options?: [...], default?: ... }`.
/// Defaults to `false` when the server omits reasoning metadata.
pub fn resolve_reasoning_capability(entry: &Value) -> bool {
    let Some(reasoning) = entry.pointer("/capabilities/reasoning") else {
        return false;
    };
    if reasoning.is_null() {
        return false;
    }
    let allowed: Vec<&Value> = reasoning
        .get("allowed_options")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().collect())
        .unwrap_or_default();
    let normalized: Vec<String> = allowed
        .iter()
        .filter_map(|v| normalize_reasoning_option(v))
        .collect();
    if !normalized.is_empty() {
        return normalized.iter().any(|option| option != "off");
    }
    reasoning
        .get("default")
        .map(is_reasoning_enabled_option)
        .unwrap_or(false)
}

/// Read the largest valid loaded-instance context window from a wire entry.
/// Tolerates malformed entries (external JSON). Returns `None` when no usable
/// loaded context is present.
pub fn resolve_loaded_context_window(entry: &Value) -> Option<u64> {
    let instances = entry.get("loaded_instances")?.as_array()?;
    let mut best: Option<u64> = None;
    for instance in instances {
        let Some(len) = instance
            .pointer("/config/context_length")
            .and_then(|v| v.as_u64())
            .filter(|v| *v > 0)
        else {
            continue;
        };
        best = Some(best.map_or(len, |b| b.max(len)));
    }
    best
}

/// Whether OpenClaw should natively preload LM Studio models before
/// inference. `models.providers.lmstudio.params.preload: false` opts out so
/// JIT loading / idle TTL / auto-evict own the lifecycle (v2026.5.2 #75921).
pub fn should_preload(provider_params: Option<&Value>) -> bool {
    provider_params
        .and_then(|p| p.get("preload"))
        .and_then(|v| v.as_bool())
        != Some(false)
}

/// Reasoning efforts advertised when a model has enabled reasoning options
/// (v2026.6.x binary thinking on/off → `reasoning_effort` mapping).
pub const LMSTUDIO_ENABLED_REASONING_EFFORTS: &[&str] =
    &["minimal", "low", "medium", "high", "xhigh"];

/// Reasoning efforts including the disable option, used when the model's
/// allowed options include an `off`/`none` state.
pub const LMSTUDIO_REASONING_EFFORTS_WITH_NONE: &[&str] =
    &["none", "minimal", "low", "medium", "high", "xhigh"];

/// Map an LM Studio reasoning capability wire entry onto the supported
/// `reasoning_effort` set: models with a disable option (binary on/off like
/// Gemma 4) include `none`; always-on reasoning models only expose the
/// enabled efforts. Returns `None` when the entry advertises no reasoning.
pub fn resolve_supported_reasoning_efforts(entry: &Value) -> Option<&'static [&'static str]> {
    if !resolve_reasoning_capability(entry) {
        return None;
    }
    let has_disable_option = entry
        .pointer("/capabilities/reasoning/allowed_options")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| normalize_reasoning_option(v))
                .any(|o| o == "off" || o == "none")
        })
        .unwrap_or(false);
    Some(if has_disable_option {
        LMSTUDIO_REASONING_EFFORTS_WITH_NONE
    } else {
        LMSTUDIO_ENABLED_REASONING_EFFORTS
    })
}

/// Resolve an LM Studio *variant* id back to its loadable model key
/// (v2026.6.x: quantized multi-variant models expose variants separately
/// from the canonical `key`, but `/api/v1/models/load` expects the key; no
/// phantom suffixed catalog entries are synthesized). Exact key matches win
/// so unusual servers exposing a suffix as the real key are preserved.
pub fn resolve_variant_model_key(entry: &Value, requested: &str) -> Option<String> {
    let requested = normalize_model_key(requested);
    let key = entry.get("key").and_then(|k| k.as_str())?;
    if key == requested {
        return Some(key.to_string());
    }
    let variants: Vec<&str> = entry
        .get("variants")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    if variants.iter().any(|variant| *variant == requested) {
        return Some(key.to_string());
    }
    None
}

/// Strip an optional `lmstudio/` provider prefix from a model ref.
pub fn normalize_model_key(model_id: &str) -> &str {
    let trimmed = model_id.trim();
    if trimmed.to_ascii_lowercase().starts_with("lmstudio/") {
        trimmed["lmstudio/".len()..].trim()
    } else {
        trimmed
    }
}

/// Resolve the OpenAI-compatible inference base from the configured server
/// base (appends `/v1` when the base doesn't already carry it).
pub fn resolve_inference_base(server_base: &str) -> String {
    let trimmed = server_base.trim().trim_end_matches('/');
    let base = if trimmed.is_empty() {
        LMSTUDIO_DEFAULT_BASE_URL
    } else {
        trimmed
    };
    if base.ends_with("/v1") {
        base.to_string()
    } else {
        format!("{}/v1", base)
    }
}

/// LM Studio provider: OpenAI-compatible chat with optional native preload.
pub struct LmStudioProvider {
    api_key: String,
    server_base: String,
    inference_base: String,
    model: String,
    preload: bool,
    client: Client,
}

impl LmStudioProvider {
    pub fn new(
        api_key: Option<String>,
        base_url: String,
        model: String,
        provider_params: Option<&Value>,
    ) -> Self {
        let server_base = base_url
            .trim()
            .trim_end_matches('/')
            .trim_end_matches("/v1")
            .trim_end_matches('/')
            .to_string();
        let server_base = if server_base.is_empty() {
            LMSTUDIO_DEFAULT_BASE_URL.to_string()
        } else {
            server_base
        };
        Self {
            api_key: api_key.unwrap_or_else(|| LMSTUDIO_LOCAL_API_KEY_PLACEHOLDER.to_string()),
            inference_base: resolve_inference_base(&server_base),
            server_base,
            model,
            preload: should_preload(provider_params),
            client: Client::new(),
        }
    }

    /// Best-effort native model load; failures degrade to JIT loading.
    async fn ensure_model_loaded(&self) {
        if !self.preload {
            debug!("lmstudio preload disabled via params.preload=false; skipping native load");
            return;
        }
        let model_key = normalize_model_key(&self.model).to_string();
        let url = format!("{}/api/v1/models/load", self.server_base);
        let result = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&serde_json::json!({
                "model": model_key,
                "context_length": LMSTUDIO_DEFAULT_LOAD_CONTEXT_LENGTH,
            }))
            .send()
            .await;
        match result {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                warn!(
                    status = %resp.status(),
                    "LM Studio inference preload failed; continuing without preload"
                );
            }
            Err(e) => {
                warn!("LM Studio inference preload failed; continuing without preload: {}", e);
            }
        }
    }
}

#[async_trait]
impl ModelProvider for LmStudioProvider {
    async fn chat(&self, mut request: ProviderRequest) -> Result<ProviderResponse> {
        self.ensure_model_loaded().await;
        request.model = normalize_model_key(&request.model).to_string();
        openai_compat::openai_compat_chat(
            &self.client,
            &self.inference_base,
            &self.api_key,
            request,
            "LM Studio",
        )
        .await
    }

    async fn stream_chat(&self, mut request: ProviderRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        self.ensure_model_loaded().await;
        request.model = normalize_model_key(&request.model).to_string();
        openai_compat::openai_compat_stream_chat(
            &self.client,
            &self.inference_base,
            &self.api_key,
            request,
            "LM Studio",
        )
        .await
    }

    fn name(&self) -> &str {
        LMSTUDIO_PROVIDER_ID
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ------------------------------------------------------------------
    // Reasoning normalization
    // ------------------------------------------------------------------

    #[test]
    fn reasoning_missing_metadata_defaults_false() {
        assert!(!resolve_reasoning_capability(&json!({})));
        assert!(!resolve_reasoning_capability(&json!({"capabilities": {}})));
        assert!(!resolve_reasoning_capability(
            &json!({"capabilities": {"reasoning": null}})
        ));
    }

    #[test]
    fn reasoning_binary_allowed_options_gemma4_shape() {
        // Gemma 4-style binary reasoning: allowed options ["off", "on"].
        let entry = json!({"capabilities": {"reasoning": {"allowed_options": ["off", "on"]}}});
        assert!(resolve_reasoning_capability(&entry));
    }

    #[test]
    fn reasoning_all_off_options_disabled() {
        let entry = json!({"capabilities": {"reasoning": {"allowed_options": ["off", "OFF", " off "]}}});
        assert!(!resolve_reasoning_capability(&entry));
    }

    #[test]
    fn reasoning_default_used_when_no_allowed_options() {
        assert!(resolve_reasoning_capability(
            &json!({"capabilities": {"reasoning": {"default": "medium"}}})
        ));
        assert!(!resolve_reasoning_capability(
            &json!({"capabilities": {"reasoning": {"default": "off"}}})
        ));
        assert!(!resolve_reasoning_capability(
            &json!({"capabilities": {"reasoning": {"default": 3}}})
        ));
    }

    #[test]
    fn reasoning_malformed_options_ignored() {
        let entry = json!({"capabilities": {"reasoning": {"allowed_options": [3, null, ""],
            "default": "high"}}});
        // Malformed entries are filtered; falls through to default.
        assert!(resolve_reasoning_capability(&entry));
    }

    // ------------------------------------------------------------------
    // Loaded context window
    // ------------------------------------------------------------------

    #[test]
    fn loaded_context_window_takes_max() {
        let entry = json!({"loaded_instances": [
            {"config": {"context_length": 4096}},
            {"config": {"context_length": 32768}},
            null,
            {"config": null}
        ]});
        assert_eq!(resolve_loaded_context_window(&entry), Some(32_768));
    }

    #[test]
    fn loaded_context_window_none_when_absent() {
        assert_eq!(resolve_loaded_context_window(&json!({})), None);
        assert_eq!(
            resolve_loaded_context_window(&json!({"loaded_instances": []})),
            None
        );
    }

    // ------------------------------------------------------------------
    // preload flag
    // ------------------------------------------------------------------

    #[test]
    fn preload_defaults_true() {
        assert!(should_preload(None));
        assert!(should_preload(Some(&json!({}))));
        assert!(should_preload(Some(&json!({"preload": true}))));
        // Non-boolean values do not disable preload.
        assert!(should_preload(Some(&json!({"preload": "false"}))));
    }

    #[test]
    fn preload_false_disables_native_load() {
        assert!(!should_preload(Some(&json!({"preload": false}))));
    }

    // ------------------------------------------------------------------
    // Model key + base URL normalization
    // ------------------------------------------------------------------

    #[test]
    fn model_key_strips_provider_prefix() {
        assert_eq!(normalize_model_key("lmstudio/qwen/qwen3.5-9b"), "qwen/qwen3.5-9b");
        assert_eq!(normalize_model_key("qwen/qwen3.5-9b"), "qwen/qwen3.5-9b");
        assert_eq!(normalize_model_key(" LMSTUDIO/m "), "m");
    }

    #[test]
    fn inference_base_appends_v1_once() {
        assert_eq!(
            resolve_inference_base("http://127.0.0.1:1234"),
            "http://127.0.0.1:1234/v1"
        );
        assert_eq!(
            resolve_inference_base("http://127.0.0.1:1234/v1"),
            "http://127.0.0.1:1234/v1"
        );
        assert_eq!(resolve_inference_base(""), "http://127.0.0.1:1234/v1");
    }

    #[test]
    fn provider_uses_local_placeholder_key() {
        let p = LmStudioProvider::new(
            None,
            LMSTUDIO_DEFAULT_BASE_URL.to_string(),
            "m".into(),
            None,
        );
        assert_eq!(p.api_key, LMSTUDIO_LOCAL_API_KEY_PLACEHOLDER);
        assert_eq!(p.name(), "lmstudio");
        assert!(p.preload);
    }

    // ------------------------------------------------------------------
    // v2026.6.x–7.1: reasoning-effort mapping + variant resolution
    // ------------------------------------------------------------------

    #[test]
    fn binary_reasoning_maps_to_efforts_with_none() {
        let entry = json!({"capabilities": {"reasoning": {"allowed_options": ["off", "on"]}}});
        assert_eq!(
            resolve_supported_reasoning_efforts(&entry),
            Some(LMSTUDIO_REASONING_EFFORTS_WITH_NONE)
        );
    }

    #[test]
    fn always_on_reasoning_maps_to_enabled_efforts_only() {
        let entry = json!({"capabilities": {"reasoning": {"allowed_options": ["low", "high"]}}});
        assert_eq!(
            resolve_supported_reasoning_efforts(&entry),
            Some(LMSTUDIO_ENABLED_REASONING_EFFORTS)
        );
    }

    #[test]
    fn no_reasoning_metadata_maps_to_none() {
        assert!(resolve_supported_reasoning_efforts(&json!({})).is_none());
    }

    #[test]
    fn variant_ids_resolve_to_loadable_key() {
        let entry = json!({
            "key": "qwen/qwen3.5-9b",
            "variants": ["qwen/qwen3.5-9b@q4_k_m", "qwen/qwen3.5-9b@q8_0"]
        });
        assert_eq!(
            resolve_variant_model_key(&entry, "qwen/qwen3.5-9b@q4_k_m").as_deref(),
            Some("qwen/qwen3.5-9b")
        );
        // Exact key matches win.
        assert_eq!(
            resolve_variant_model_key(&entry, "qwen/qwen3.5-9b").as_deref(),
            Some("qwen/qwen3.5-9b")
        );
        // Unknown variants do not synthesize phantom entries.
        assert!(resolve_variant_model_key(&entry, "qwen/qwen3.5-9b@q2").is_none());
    }

    #[test]
    fn variant_resolution_handles_provider_prefix() {
        let entry = json!({"key": "m", "variants": ["m@q4"]});
        assert_eq!(
            resolve_variant_model_key(&entry, "lmstudio/m@q4").as_deref(),
            Some("m")
        );
    }

    #[test]
    fn provider_respects_preload_false_param() {
        let params = json!({"preload": false});
        let p = LmStudioProvider::new(
            None,
            "http://127.0.0.1:1234/v1".to_string(),
            "m".into(),
            Some(&params),
        );
        assert!(!p.preload);
        assert_eq!(p.server_base, "http://127.0.0.1:1234");
        assert_eq!(p.inference_base, "http://127.0.0.1:1234/v1");
    }
}
