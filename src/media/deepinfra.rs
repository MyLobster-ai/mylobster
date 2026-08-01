//! DeepInfra multi-modal media paths (v2026.4.27).
//!
//! HTTP implementations for DeepInfra image generation/editing, audio
//! understanding (Whisper transcription), and TTS. All routes ride the
//! OpenAI-compatible surface at `https://api.deepinfra.com/v1/openai`.
//! Model/voice metadata lives provider-side in
//! `crate::providers::deepinfra`; text embeddings live there too
//! (`deepinfra_embed`) so the memory subsystem can call them directly.

use crate::providers::deepinfra::{
    normalize_deepinfra_base_url, normalize_deepinfra_model_ref, DEEPINFRA_BASE_URL,
    DEEPINFRA_IMAGE_SIZES, DEFAULT_DEEPINFRA_IMAGE_SIZE, DEFAULT_DEEPINFRA_TTS_VOICE,
};
use anyhow::{anyhow, Result};
use base64::Engine;
use reqwest::Client;

/// Maximum images per generation request.
pub const DEEPINFRA_IMAGE_MAX_COUNT: u32 = 4;

/// DeepInfra image editing supports one reference image.
pub const DEEPINFRA_IMAGE_EDIT_MAX_INPUT_IMAGES: usize = 1;

fn resolve_size(size: Option<&str>) -> String {
    size.map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_DEEPINFRA_IMAGE_SIZE)
        .to_string()
}

/// Whether a size string is one of the documented DeepInfra sizes.
pub fn is_supported_image_size(size: &str) -> bool {
    DEEPINFRA_IMAGE_SIZES.contains(&size)
}

/// Build the JSON body for a DeepInfra image *generation* request
/// (OpenAI-compatible `/images/generations`).
pub fn build_image_generate_body(
    model: &str,
    prompt: &str,
    count: u32,
    size: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "model": normalize_deepinfra_model_ref(model),
        "prompt": prompt,
        "n": count.clamp(1, DEEPINFRA_IMAGE_MAX_COUNT),
        "size": resolve_size(size),
        "response_format": "b64_json",
    })
}

fn parse_b64_images(payload: &serde_json::Value) -> Result<Vec<Vec<u8>>> {
    let rows = payload
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| anyhow!("DeepInfra image response did not include generated image data"))?;
    let mut images = Vec::new();
    for row in rows {
        if let Some(b64) = row.get("b64_json").and_then(|b| b.as_str()) {
            images.push(
                base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|e| anyhow!("DeepInfra image payload was not valid base64: {}", e))?,
            );
        }
    }
    if images.is_empty() {
        return Err(anyhow!(
            "DeepInfra image response did not include generated image data"
        ));
    }
    Ok(images)
}

/// Generate images. Returns raw image bytes per generated image.
pub async fn deepinfra_image_generate(
    client: &Client,
    base_url: Option<&str>,
    api_key: &str,
    model: &str,
    prompt: &str,
    count: u32,
    size: Option<&str>,
) -> Result<Vec<Vec<u8>>> {
    let base = normalize_deepinfra_base_url(base_url, DEEPINFRA_BASE_URL);
    let body = build_image_generate_body(model, prompt, count, size);
    let resp = client
        .post(format!("{}/images/generations", base))
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("DeepInfra image generation failed ({}): {}", status, text));
    }
    parse_b64_images(&resp.json().await?)
}

/// Edit an image with one reference image (multipart `/images/edits`).
pub async fn deepinfra_image_edit(
    client: &Client,
    base_url: Option<&str>,
    api_key: &str,
    model: &str,
    prompt: &str,
    reference_image: &[u8],
    reference_mime: Option<&str>,
    size: Option<&str>,
) -> Result<Vec<Vec<u8>>> {
    if reference_image.is_empty() {
        return Err(anyhow!("DeepInfra image edit missing reference image."));
    }
    let base = normalize_deepinfra_base_url(base_url, DEEPINFRA_BASE_URL);
    let mime = reference_mime.unwrap_or("image/png");
    let part = reqwest::multipart::Part::bytes(reference_image.to_vec())
        .file_name("image-0.png")
        .mime_str(mime)?;
    let form = reqwest::multipart::Form::new()
        .text("model", normalize_deepinfra_model_ref(model).to_string())
        .text("prompt", prompt.to_string())
        .text("n", "1")
        .text("size", resolve_size(size))
        .text("response_format", "b64_json")
        .part("image", part);
    let resp = client
        .post(format!("{}/images/edits", base))
        .header("Authorization", format!("Bearer {}", api_key))
        .multipart(form)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("DeepInfra image edit failed ({}): {}", status, text));
    }
    parse_b64_images(&resp.json().await?)
}

/// Transcribe audio through DeepInfra's Whisper models (OpenAI-compatible
/// `/audio/transcriptions` multipart route). Returns the transcript text.
pub async fn deepinfra_transcribe(
    client: &Client,
    base_url: Option<&str>,
    api_key: &str,
    model: &str,
    audio: Vec<u8>,
    file_name: &str,
) -> Result<String> {
    let base = normalize_deepinfra_base_url(base_url, DEEPINFRA_BASE_URL);
    let part = reqwest::multipart::Part::bytes(audio)
        .file_name(file_name.to_string())
        .mime_str("application/octet-stream")?;
    let form = reqwest::multipart::Form::new()
        .text("model", normalize_deepinfra_model_ref(model).to_string())
        .part("file", part);
    let resp = client
        .post(format!("{}/audio/transcriptions", base))
        .header("Authorization", format!("Bearer {}", api_key))
        .multipart(form)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "DeepInfra audio transcription failed ({}): {}",
            status,
            text
        ));
    }
    let payload: serde_json::Value = resp.json().await?;
    Ok(payload
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string())
}

/// Synthesize speech through DeepInfra TTS (OpenAI-compatible
/// `/audio/speech`, with `extraBody` passthrough). Returns audio bytes.
pub async fn deepinfra_speech(
    client: &Client,
    base_url: Option<&str>,
    api_key: &str,
    model: &str,
    input: &str,
    voice: Option<&str>,
    response_format: Option<&str>,
    extra_body: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Result<Vec<u8>> {
    let base = normalize_deepinfra_base_url(base_url, DEEPINFRA_BASE_URL);
    let body = crate::providers::openai_compat::build_speech_request_body(
        normalize_deepinfra_model_ref(model),
        input,
        voice.unwrap_or(DEFAULT_DEEPINFRA_TTS_VOICE),
        Some(response_format.unwrap_or("mp3")),
        None,
        None,
        extra_body,
    );
    crate::providers::openai_compat::openai_compat_speech(
        client, &base, api_key, &body, "DeepInfra",
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn generate_body_shape() {
        let body = build_image_generate_body("deepinfra/black-forest-labs/FLUX-1-schnell", "cat", 2, None);
        assert_eq!(body["model"], "black-forest-labs/FLUX-1-schnell");
        assert_eq!(body["prompt"], "cat");
        assert_eq!(body["n"], 2);
        assert_eq!(body["size"], DEFAULT_DEEPINFRA_IMAGE_SIZE);
        assert_eq!(body["response_format"], "b64_json");
    }

    #[test]
    fn generate_body_clamps_count() {
        assert_eq!(build_image_generate_body("m", "p", 0, None)["n"], 1);
        assert_eq!(build_image_generate_body("m", "p", 99, None)["n"], 4);
    }

    #[test]
    fn supported_sizes() {
        assert!(is_supported_image_size("1024x1024"));
        assert!(is_supported_image_size("1792x1024"));
        assert!(!is_supported_image_size("3000x3000"));
    }

    #[tokio::test]
    async fn image_generate_decodes_b64_payload() {
        let server = MockServer::start().await;
        let b64 = base64::engine::general_purpose::STANDARD.encode([9u8, 8, 7]);
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data": [{"b64_json": b64}]})),
            )
            .mount(&server)
            .await;
        let client = Client::new();
        let images = deepinfra_image_generate(
            &client,
            Some(&server.uri()),
            "k",
            "m",
            "cat",
            1,
            None,
        )
        .await
        .unwrap();
        assert_eq!(images, vec![vec![9u8, 8, 7]]);
    }

    #[tokio::test]
    async fn image_generate_errors_on_empty_data() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
            .mount(&server)
            .await;
        let client = Client::new();
        let err = deepinfra_image_generate(&client, Some(&server.uri()), "k", "m", "p", 1, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not include generated image data"));
    }

    #[tokio::test]
    async fn image_edit_requires_reference_image() {
        let client = Client::new();
        let err = deepinfra_image_edit(&client, None, "k", "m", "p", &[], None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing reference image"));
    }

    #[tokio::test]
    async fn transcription_returns_text_field() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/audio/transcriptions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": "halo dunia"})))
            .mount(&server)
            .await;
        let client = Client::new();
        let text = deepinfra_transcribe(
            &client,
            Some(&server.uri()),
            "k",
            "openai/whisper-large-v3-turbo",
            vec![1, 2, 3],
            "note.ogg",
        )
        .await
        .unwrap();
        assert_eq!(text, "halo dunia");
    }

    #[tokio::test]
    async fn speech_returns_audio_bytes_and_merges_extra_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/audio/speech"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![4u8, 5]))
            .mount(&server)
            .await;
        let client = Client::new();
        let extra = json!({"lang": "id"});
        let audio = deepinfra_speech(
            &client,
            Some(&server.uri()),
            "k",
            "hexgrad/Kokoro-82M",
            "halo",
            None,
            None,
            extra.as_object(),
        )
        .await
        .unwrap();
        assert_eq!(audio, vec![4, 5]);
        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["voice"], DEFAULT_DEEPINFRA_TTS_VOICE);
        assert_eq!(body["lang"], "id");
    }
}
