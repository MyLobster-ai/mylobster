//! Seedance reference(image)-to-video request shaping (v2026.4.25, refreshed
//! to the current upstream state).
//!
//! Seedance models are hosted on BytePlus/Volcengine ARK endpoints. The
//! generation request is a `content` array: a text prompt plus, for
//! reference-to-video, an `image_url` block with `role: "first_frame"`.
//! Seedance 1.0 has separate T2V and I2V model ids; when a reference image is
//! provided with a T2V model the corresponding I2V variant is substituted so
//! the API does not reject with a task_type mismatch. Resolution values must
//! be lowercase (`480p`, `720p`); uppercase variants are rejected.

use base64::Engine;

/// Default Seedance video model.
pub const DEFAULT_SEEDANCE_VIDEO_MODEL: &str = "seedance-1-0-lite-t2v-250428";

/// Bundled Seedance model ids.
pub const SEEDANCE_VIDEO_MODELS: &[&str] = &[
    "seedance-1-0-lite-t2v-250428",
    "seedance-1-0-lite-i2v-250428",
    "seedance-1-0-pro-250528",
    "seedance-1-5-pro-251215",
];

/// Supported aspect ratios.
pub const SEEDANCE_VIDEO_ASPECT_RATIOS: &[&str] = &["16:9", "4:3", "1:1", "3:4", "9:16"];

/// Pixverse video models on DeepInfra (v2026.5.x Pixverse video plugin;
/// bundled-native here).
pub const PIXVERSE_VIDEO_MODELS: &[&str] = &["Pixverse/Pixverse-T2V", "Pixverse/Pixverse-T2V-HD"];

/// Maximum clip duration in seconds.
pub const SEEDANCE_MAX_DURATION_SECONDS: u32 = 12;

/// Optional inputs for a Seedance generation request.
#[derive(Debug, Clone, Default)]
pub struct SeedanceRequestOptions {
    pub aspect_ratio: Option<String>,
    /// Lowercased before serialization (`480p`, `720p`, `1080p`).
    pub resolution: Option<String>,
    pub duration_seconds: Option<u32>,
    pub audio: Option<bool>,
    pub watermark: Option<bool>,
    pub seed: Option<i64>,
    /// draft=true forces 480p for faster generation.
    pub draft: bool,
    pub camera_fixed: Option<bool>,
}

/// Reference image for image-to-video.
#[derive(Debug, Clone)]
pub enum SeedanceReferenceImage {
    Url(String),
    Bytes { data: Vec<u8>, mime_type: String },
}

impl SeedanceReferenceImage {
    fn to_image_url(&self) -> String {
        match self {
            SeedanceReferenceImage::Url(url) => url.clone(),
            SeedanceReferenceImage::Bytes { data, mime_type } => format!(
                "data:{};base64,{}",
                mime_type,
                base64::engine::general_purpose::STANDARD.encode(data)
            ),
        }
    }
}

/// Resolve the effective model id: when a reference image is present and a
/// Seedance 1.0 T2V id was requested, substitute the matching I2V variant.
/// Seedance 1.5 Pro uses one id for both modes and passes through unchanged.
pub fn resolve_seedance_model(requested: Option<&str>, has_reference_image: bool) -> String {
    let requested = requested
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or(DEFAULT_SEEDANCE_VIDEO_MODEL);
    if has_reference_image && requested.contains("-t2v-") {
        requested.replace("-t2v-", "-i2v-")
    } else {
        requested.to_string()
    }
}

/// Build the Seedance generation request body (`content` array shape).
pub fn build_seedance_request_body(
    prompt: &str,
    model: Option<&str>,
    reference_image: Option<&SeedanceReferenceImage>,
    options: &SeedanceRequestOptions,
) -> serde_json::Value {
    let resolved_model = resolve_seedance_model(model, reference_image.is_some());
    let mut content = vec![serde_json::json!({"type": "text", "text": prompt})];
    if let Some(image) = reference_image {
        content.push(serde_json::json!({
            "type": "image_url",
            "image_url": {"url": image.to_image_url()},
            "role": "first_frame",
        }));
    }
    let mut body = serde_json::json!({
        "model": resolved_model,
        "content": content,
    });
    let map = body.as_object_mut().unwrap();
    if let Some(ratio) = options
        .aspect_ratio
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        map.insert("ratio".to_string(), serde_json::Value::String(ratio.to_string()));
    }
    // Seedance requires lowercase resolution values; uppercase variants like
    // "480P" are rejected with InvalidParameter.
    if let Some(resolution) = options
        .resolution
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        map.insert(
            "resolution".to_string(),
            serde_json::Value::String(resolution.to_ascii_lowercase()),
        );
    }
    if let Some(duration) = options.duration_seconds {
        map.insert(
            "duration".to_string(),
            serde_json::Value::Number(duration.min(SEEDANCE_MAX_DURATION_SECONDS).into()),
        );
    }
    if let Some(audio) = options.audio {
        map.insert("generate_audio".to_string(), serde_json::Value::Bool(audio));
    }
    if let Some(watermark) = options.watermark {
        map.insert("watermark".to_string(), serde_json::Value::Bool(watermark));
    }
    if let Some(seed) = options.seed {
        map.insert("seed".to_string(), serde_json::Value::Number(seed.into()));
    }
    if options.draft && !map.contains_key("resolution") {
        map.insert(
            "resolution".to_string(),
            serde_json::Value::String("480p".to_string()),
        );
    }
    if let Some(camera_fixed) = options.camera_fixed {
        map.insert("camera_fixed".to_string(), serde_json::Value::Bool(camera_fixed));
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_substitutes_i2v_for_reference_images() {
        assert_eq!(
            resolve_seedance_model(Some("seedance-1-0-lite-t2v-250428"), true),
            "seedance-1-0-lite-i2v-250428"
        );
    }

    #[test]
    fn model_keeps_t2v_without_reference_image() {
        assert_eq!(
            resolve_seedance_model(Some("seedance-1-0-lite-t2v-250428"), false),
            "seedance-1-0-lite-t2v-250428"
        );
    }

    #[test]
    fn seedance_1_5_pro_single_id_unaffected() {
        assert_eq!(
            resolve_seedance_model(Some("seedance-1-5-pro-251215"), true),
            "seedance-1-5-pro-251215"
        );
    }

    #[test]
    fn default_model_when_unset() {
        assert_eq!(resolve_seedance_model(None, false), DEFAULT_SEEDANCE_VIDEO_MODEL);
        assert_eq!(resolve_seedance_model(Some("  "), false), DEFAULT_SEEDANCE_VIDEO_MODEL);
    }

    #[test]
    fn body_includes_first_frame_reference() {
        let image = SeedanceReferenceImage::Url("https://img.example/ref.png".to_string());
        let body = build_seedance_request_body("a lobster", None, Some(&image), &Default::default());
        assert_eq!(body["content"][0]["type"], "text");
        assert_eq!(body["content"][1]["type"], "image_url");
        assert_eq!(body["content"][1]["role"], "first_frame");
        assert_eq!(body["content"][1]["image_url"]["url"], "https://img.example/ref.png");
        // Auto-substituted to the I2V variant.
        assert_eq!(body["model"], "seedance-1-0-lite-i2v-250428");
    }

    #[test]
    fn body_encodes_byte_references_as_data_url() {
        let image = SeedanceReferenceImage::Bytes {
            data: vec![1, 2, 3],
            mime_type: "image/png".to_string(),
        };
        let body = build_seedance_request_body("x", None, Some(&image), &Default::default());
        let url = body["content"][1]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn resolution_lowercased_and_duration_clamped() {
        let options = SeedanceRequestOptions {
            resolution: Some("480P".to_string()),
            duration_seconds: Some(99),
            ..Default::default()
        };
        let body = build_seedance_request_body("x", None, None, &options);
        assert_eq!(body["resolution"], "480p");
        assert_eq!(body["duration"], 12);
    }

    #[test]
    fn draft_forces_480p_only_when_resolution_unset() {
        let draft_only = SeedanceRequestOptions {
            draft: true,
            ..Default::default()
        };
        let body = build_seedance_request_body("x", None, None, &draft_only);
        assert_eq!(body["resolution"], "480p");

        let with_res = SeedanceRequestOptions {
            draft: true,
            resolution: Some("720p".to_string()),
            ..Default::default()
        };
        let body = build_seedance_request_body("x", None, None, &with_res);
        assert_eq!(body["resolution"], "720p");
    }

    #[test]
    fn provider_options_serialized_with_wire_names() {
        let options = SeedanceRequestOptions {
            aspect_ratio: Some("16:9".to_string()),
            audio: Some(true),
            watermark: Some(false),
            seed: Some(42),
            camera_fixed: Some(true),
            ..Default::default()
        };
        let body = build_seedance_request_body("x", None, None, &options);
        assert_eq!(body["ratio"], "16:9");
        assert_eq!(body["generate_audio"], true);
        assert_eq!(body["watermark"], false);
        assert_eq!(body["seed"], 42);
        assert_eq!(body["camera_fixed"], true);
    }

    #[test]
    fn optional_fields_omitted_by_default() {
        let body = build_seedance_request_body("x", None, None, &Default::default());
        let map = body.as_object().unwrap();
        for key in ["ratio", "resolution", "duration", "generate_audio", "watermark", "seed", "camera_fixed"] {
            assert!(!map.contains_key(key), "{} should be omitted", key);
        }
    }
}
