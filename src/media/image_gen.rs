//! LiteLLM image generation (v2026.4.25).
//!
//! LiteLLM proxies expose an OpenAI-compatible `/images/generations` route.
//! The bundled provider defaults to a loopback proxy; private-network access
//! is auto-allowed only for loopback-style hosts — LAN/custom private
//! endpoints need an explicit `allowPrivateNetwork` opt-in.

use anyhow::{anyhow, Result};
use base64::Engine;
use reqwest::Client;

/// Default LiteLLM proxy base URL.
pub const LITELLM_BASE_URL: &str = "http://localhost:4000";

/// Default LiteLLM image model.
pub const DEFAULT_LITELLM_IMAGE_MODEL: &str = "gpt-image-2";

/// Default image size.
pub const DEFAULT_LITELLM_IMAGE_SIZE: &str = "1024x1024";

/// Sizes accepted by the LiteLLM image route.
pub const LITELLM_SUPPORTED_SIZES: &[&str] = &[
    "256x256",
    "512x512",
    "1024x1024",
    "1024x1536",
    "1024x1792",
    "1536x1024",
    "1792x1024",
    "2048x2048",
    "2048x1152",
    "3840x2160",
    "2160x3840",
];

/// Maximum reference images accepted for edits.
pub const LITELLM_MAX_INPUT_IMAGES: usize = 5;

/// LiteLLM's default proxy is loopback. Auto-enable private-network access
/// only for loopback-style hosts; LAN/custom private endpoints should use an
/// explicit opt-in (port of upstream `isAutoAllowedLitellmHostname`).
pub fn is_auto_allowed_litellm_hostname(hostname: &str) -> bool {
    if hostname.is_empty() {
        return false;
    }
    // Strip IPv6 brackets: "[::1]" -> "::1".
    let host = if hostname.starts_with('[') && hostname.ends_with(']') {
        &hostname[1..hostname.len() - 1]
    } else {
        hostname
    };
    let lowered = host.to_ascii_lowercase();
    if lowered == "localhost"
        || lowered == "host.docker.internal"
        || lowered.ends_with(".localhost")
    {
        return true;
    }
    if lowered == "127.0.0.1" || lowered.starts_with("127.") {
        return true;
    }
    lowered == "::1" || lowered == "0:0:0:0:0:0:0:1"
}

/// Build the JSON body for a LiteLLM image generation request.
pub fn build_litellm_image_body(
    model: Option<&str>,
    prompt: &str,
    count: u32,
    size: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "model": model.map(str::trim).filter(|m| !m.is_empty()).unwrap_or(DEFAULT_LITELLM_IMAGE_MODEL),
        "prompt": prompt,
        "n": count.max(1),
        "size": size.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(DEFAULT_LITELLM_IMAGE_SIZE),
        "response_format": "b64_json",
    })
}

/// Generate images through a LiteLLM proxy. Returns raw image bytes.
pub async fn litellm_image_generate(
    client: &Client,
    base_url: Option<&str>,
    api_key: Option<&str>,
    model: Option<&str>,
    prompt: &str,
    count: u32,
    size: Option<&str>,
) -> Result<Vec<Vec<u8>>> {
    let base = base_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(LITELLM_BASE_URL)
        .trim_end_matches('/')
        .to_string();
    let body = build_litellm_image_body(model, prompt, count, size);
    let mut req = client.post(format!("{}/images/generations", base)).json(&body);
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        req = req.header("Authorization", format!("Bearer {}", key));
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("LiteLLM image generation failed ({}): {}", status, text));
    }
    let payload: serde_json::Value = resp.json().await?;
    let rows = payload
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| anyhow!("LiteLLM image response did not include generated image data"))?;
    let mut images = Vec::new();
    for row in rows {
        if let Some(b64) = row.get("b64_json").and_then(|b| b.as_str()) {
            images.push(
                base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|e| anyhow!("LiteLLM image payload was not valid base64: {}", e))?,
            );
        } else if let Some(url) = row.get("url").and_then(|u| u.as_str()) {
            // Some proxied backends return URLs regardless of response_format.
            let bytes = client.get(url).send().await?.bytes().await?;
            images.push(bytes.to_vec());
        }
    }
    if images.is_empty() {
        return Err(anyhow!(
            "LiteLLM image response did not include generated image data"
        ));
    }
    Ok(images)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn loopback_hosts_are_auto_allowed() {
        assert!(is_auto_allowed_litellm_hostname("localhost"));
        assert!(is_auto_allowed_litellm_hostname("app.localhost"));
        assert!(is_auto_allowed_litellm_hostname("host.docker.internal"));
        assert!(is_auto_allowed_litellm_hostname("127.0.0.1"));
        assert!(is_auto_allowed_litellm_hostname("127.1.2.3"));
        assert!(is_auto_allowed_litellm_hostname("[::1]"));
        assert!(is_auto_allowed_litellm_hostname("::1"));
    }

    #[test]
    fn lan_and_public_hosts_are_not_auto_allowed() {
        assert!(!is_auto_allowed_litellm_hostname("192.168.1.5"));
        assert!(!is_auto_allowed_litellm_hostname("10.0.0.4"));
        assert!(!is_auto_allowed_litellm_hostname("litellm.example.com"));
        assert!(!is_auto_allowed_litellm_hostname(""));
    }

    #[test]
    fn body_defaults() {
        let body = build_litellm_image_body(None, "a cat", 0, None);
        assert_eq!(body["model"], DEFAULT_LITELLM_IMAGE_MODEL);
        assert_eq!(body["n"], 1);
        assert_eq!(body["size"], DEFAULT_LITELLM_IMAGE_SIZE);
        assert_eq!(body["response_format"], "b64_json");
    }

    #[test]
    fn supported_sizes_include_high_res() {
        assert!(LITELLM_SUPPORTED_SIZES.contains(&"3840x2160"));
        assert!(LITELLM_SUPPORTED_SIZES.contains(&"1024x1024"));
    }

    #[tokio::test]
    async fn generate_decodes_b64_rows() {
        let server = MockServer::start().await;
        let b64 = base64::engine::general_purpose::STANDARD.encode([1u8, 2]);
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": [{"b64_json": b64}]})),
            )
            .mount(&server)
            .await;
        let client = Client::new();
        let images = litellm_image_generate(
            &client,
            Some(&server.uri()),
            Some("k"),
            None,
            "cat",
            1,
            None,
        )
        .await
        .unwrap();
        assert_eq!(images, vec![vec![1u8, 2]]);
    }

    #[tokio::test]
    async fn generate_error_includes_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(500).set_body_string("proxy down"))
            .mount(&server)
            .await;
        let client = Client::new();
        let err = litellm_image_generate(&client, Some(&server.uri()), None, None, "p", 1, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("500"));
        assert!(err.contains("LiteLLM"));
    }
}
