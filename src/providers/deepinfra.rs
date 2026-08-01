//! DeepInfra provider (v2026.4.27 multi-modal + v2026.5.2 manifest catalog).
//!
//! Chat completions route through the shared OpenAI-compatible path
//! (`OPENAI_COMPAT_PROVIDERS`). This module adds:
//!
//! * **Manifest catalog discovery** (v2026.5.2) — the runtime fallback chat
//!   catalog derives from the provider manifest instead of duplicated static
//!   model data (`deepinfra_catalog`).
//! * **Media capability metadata** (v2026.4.27) — fallback model lists for
//!   image generation/editing, TTS, audio understanding (Whisper), and text
//!   embeddings. The HTTP media paths live in `crate::media::deepinfra`.
//! * **Text embeddings** (v2026.4.27) — `deepinfra_embed` is the provider-side
//!   entry point other subsystems (memory) can call; it posts to the
//!   OpenAI-compatible `/embeddings` endpoint.

use super::manifest::{self, ManifestModel};
use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;

/// DeepInfra OpenAI-compatible base URL (chat, images, audio, embeddings).
pub const DEEPINFRA_BASE_URL: &str = "https://api.deepinfra.com/v1/openai";

/// DeepInfra native inference base URL (non-OpenAI-compatible model routes).
pub const DEEPINFRA_NATIVE_BASE_URL: &str = "https://api.deepinfra.com/v1/inference";

/// Env var carrying the DeepInfra API key.
pub const DEEPINFRA_API_KEY_ENV_VAR: &str = "DEEPINFRA_API_KEY";

/// Fallback image generation/editing models (first entry is the default).
pub const DEEPINFRA_IMAGE_FALLBACK_MODELS: &[&str] = &[
    "black-forest-labs/FLUX-1-schnell",
    "run-diffusion/Juggernaut-Lightning-Flux",
    "black-forest-labs/FLUX-1-dev",
    "Qwen/Qwen-Image-Max",
    "stabilityai/sdxl-turbo",
];

/// Fallback TTS models (first entry is the default).
pub const DEEPINFRA_TTS_FALLBACK_MODELS: &[&str] = &[
    "hexgrad/Kokoro-82M",
    "Qwen/Qwen3-TTS",
    "ResembleAI/chatterbox-turbo",
    "sesame/csm-1b",
];

/// Default TTS voice.
pub const DEFAULT_DEEPINFRA_TTS_VOICE: &str = "af_bella";

/// Fallback audio-understanding (ASR) models.
pub const DEEPINFRA_ASR_FALLBACK_MODELS: &[&str] =
    &["openai/whisper-large-v3-turbo", "openai/whisper-large-v3"];

/// Fallback text-embedding models.
pub const DEEPINFRA_EMBED_FALLBACK_MODELS: &[&str] = &["BAAI/bge-m3"];

/// Default image size for generation/editing.
pub const DEFAULT_DEEPINFRA_IMAGE_SIZE: &str = "1024x1024";

/// Supported image sizes.
pub const DEEPINFRA_IMAGE_SIZES: &[&str] =
    &["512x512", "1024x1024", "1024x1792", "1792x1024"];

/// Manifest-driven chat catalog (v2026.5.2: no duplicated static rows).
pub fn deepinfra_catalog() -> &'static [ManifestModel] {
    manifest::manifest_models("deepinfra")
}

/// Strip an optional `deepinfra/` provider prefix from a model ref.
pub fn normalize_deepinfra_model_ref(model: &str) -> &str {
    model.strip_prefix("deepinfra/").unwrap_or(model)
}

/// Normalize a configured base URL, trimming trailing slashes and falling
/// back to the bundled default.
pub fn normalize_deepinfra_base_url(configured: Option<&str>, fallback: &str) -> String {
    let base = configured
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback);
    base.trim_end_matches('/').to_string()
}

// ============================================================================
// Text embeddings (v2026.4.27)
// ============================================================================

#[derive(Debug, Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingRow>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingRow {
    embedding: Vec<f32>,
    #[serde(default)]
    index: Option<usize>,
}

/// Embed a batch of texts through DeepInfra's OpenAI-compatible `/embeddings`
/// endpoint. Returns one vector per input text, in input order.
///
/// Exposed provider-side so the memory subsystem can plug DeepInfra in as an
/// embedding backend without owning the HTTP path.
pub async fn deepinfra_embed(
    client: &Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    texts: &[String],
) -> Result<Vec<Vec<f32>>> {
    let base = normalize_deepinfra_base_url(Some(base_url), DEEPINFRA_BASE_URL);
    let model = normalize_deepinfra_model_ref(model);
    let resp = client
        .post(format!("{}/embeddings", base))
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": model,
            "input": texts,
            "encoding_format": "float",
        }))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("DeepInfra embeddings API error ({}): {}", status, text);
    }

    let parsed: EmbeddingsResponse = resp.json().await?;
    let mut rows: Vec<EmbeddingRow> = parsed.data;
    // The API documents input-order rows; honor explicit indices when present.
    rows.sort_by_key(|r| r.index.unwrap_or(0));
    Ok(rows.into_iter().map(|r| r.embedding).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn catalog_is_manifest_driven() {
        let catalog = deepinfra_catalog();
        assert!(!catalog.is_empty());
        assert!(catalog.iter().any(|m| m.id == "deepseek-ai/DeepSeek-V4-Flash"));
    }

    #[test]
    fn normalize_model_ref_strips_provider_prefix() {
        assert_eq!(
            normalize_deepinfra_model_ref("deepinfra/BAAI/bge-m3"),
            "BAAI/bge-m3"
        );
        assert_eq!(normalize_deepinfra_model_ref("BAAI/bge-m3"), "BAAI/bge-m3");
    }

    #[test]
    fn normalize_base_url_trims_and_falls_back() {
        assert_eq!(
            normalize_deepinfra_base_url(None, DEEPINFRA_BASE_URL),
            DEEPINFRA_BASE_URL
        );
        assert_eq!(
            normalize_deepinfra_base_url(Some("https://x.example/v1///"), DEEPINFRA_BASE_URL),
            "https://x.example/v1"
        );
        assert_eq!(
            normalize_deepinfra_base_url(Some("   "), DEEPINFRA_BASE_URL),
            DEEPINFRA_BASE_URL
        );
    }

    #[test]
    fn media_fallback_models_have_defaults() {
        assert_eq!(DEEPINFRA_IMAGE_FALLBACK_MODELS[0], "black-forest-labs/FLUX-1-schnell");
        assert_eq!(DEEPINFRA_TTS_FALLBACK_MODELS[0], "hexgrad/Kokoro-82M");
        assert_eq!(DEEPINFRA_ASR_FALLBACK_MODELS[0], "openai/whisper-large-v3-turbo");
        assert_eq!(DEEPINFRA_EMBED_FALLBACK_MODELS[0], "BAAI/bge-m3");
    }

    #[tokio::test]
    async fn embed_posts_to_embeddings_and_orders_rows() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {"embedding": [0.5, 0.6], "index": 1},
                    {"embedding": [0.1, 0.2], "index": 0}
                ]
            })))
            .mount(&server)
            .await;
        let client = Client::new();
        let out = deepinfra_embed(
            &client,
            &server.uri(),
            "k",
            "deepinfra/BAAI/bge-m3",
            &["a".to_string(), "b".to_string()],
        )
        .await
        .unwrap();
        assert_eq!(out, vec![vec![0.1, 0.2], vec![0.5, 0.6]]);

        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["model"], "BAAI/bge-m3");
        assert_eq!(body["input"][0], "a");
    }

    #[tokio::test]
    async fn embed_error_includes_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(402).set_body_string("no credit"))
            .mount(&server)
            .await;
        let client = Client::new();
        let err = deepinfra_embed(&client, &server.uri(), "k", "m", &["a".to_string()])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("402"));
        assert!(err.contains("DeepInfra embeddings"));
    }
}
