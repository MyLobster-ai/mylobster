//! fal.ai media-generation request shaping (v2026.5.x–6.x).
//!
//! Ports the upstream fal image-generation profile resolution: per-model
//! `/edit` (vs `/image-to-image`) routing, reference-image caps, and
//! geometry contracts for Krea 2, GPT Image, Nano Banana (legacy/2/2 Lite),
//! and Grok Imagine.

/// Krea 2 model prefix and canonical text-to-image models.
pub const FAL_KREA_2_MODEL_PREFIX: &str = "krea/v2/";
pub const FAL_KREA_2_MEDIUM_MODEL: &str = "krea/v2/medium/text-to-image";
pub const FAL_KREA_2_LARGE_MODEL: &str = "krea/v2/large/text-to-image";
pub const FAL_NANO_BANANA_MODEL: &str = "fal-ai/nano-banana";
pub const FAL_NANO_BANANA_2_LITE_MODEL: &str = "google/nano-banana-2-lite";
pub const FAL_GROK_IMAGINE_MODEL: &str = "xai/grok-imagine-image";

/// Reference-image caps (current upstream values).
pub const GPT_IMAGE_EDIT_MAX_INPUT_IMAGES: usize = 10;
pub const NANO_BANANA_LEGACY_EDIT_MAX_INPUT_IMAGES: usize = 3;
pub const NANO_BANANA_EDIT_MAX_INPUT_IMAGES: usize = 14;
pub const GROK_IMAGINE_EDIT_MAX_INPUT_IMAGES: usize = 3;
pub const KREA_STYLE_REFERENCE_MAX_INPUT_IMAGES: usize = 10;

/// How reference images ride the request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FalReferenceImages {
    /// Multiple refs via `image_urls`.
    ImageUrls,
    /// One ref via `image_url`.
    ImageUrl,
}

/// Path suffix appended for edit requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FalEditRoute {
    /// `<model>/edit`
    Edit,
    /// `<model>/image-to-image`
    ImageToImage,
    /// Model has no edit route (text-to-image only).
    None,
}

/// Resolved per-model fal image profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FalImageProfile {
    pub edit_route: FalEditRoute,
    pub reference_images: FalReferenceImages,
    pub max_input_images: usize,
    pub supports_count: bool,
}

/// Resolve the fal image-generation profile for a model id (port of the
/// upstream per-model capability resolution, v2026.7.1 state).
pub fn resolve_fal_image_profile(model: &str) -> FalImageProfile {
    let model = model.trim();
    // Krea 2 text-to-image: style references, no edit path append.
    if model.starts_with(FAL_KREA_2_MODEL_PREFIX) {
        return FalImageProfile {
            edit_route: FalEditRoute::None,
            reference_images: FalReferenceImages::ImageUrls,
            max_input_images: KREA_STYLE_REFERENCE_MAX_INPUT_IMAGES,
            supports_count: false,
        };
    }
    // Legacy Nano Banana (`fal-ai/nano-banana` or nested paths).
    if model == FAL_NANO_BANANA_MODEL || model.starts_with(&format!("{}/", FAL_NANO_BANANA_MODEL)) {
        return FalImageProfile {
            edit_route: FalEditRoute::Edit,
            reference_images: FalReferenceImages::ImageUrls,
            max_input_images: NANO_BANANA_LEGACY_EDIT_MAX_INPUT_IMAGES,
            supports_count: true,
        };
    }
    // GPT Image (openai/gpt-image-*) and Nano Banana 2 (fal-ai/nano-banana-*).
    if model.starts_with("openai/gpt-image-")
        || model.starts_with(&format!("{}-", FAL_NANO_BANANA_MODEL))
    {
        let is_nano_banana = model.starts_with(&format!("{}-", FAL_NANO_BANANA_MODEL));
        return FalImageProfile {
            edit_route: FalEditRoute::Edit,
            reference_images: FalReferenceImages::ImageUrls,
            max_input_images: if is_nano_banana {
                NANO_BANANA_EDIT_MAX_INPUT_IMAGES
            } else {
                GPT_IMAGE_EDIT_MAX_INPUT_IMAGES
            },
            supports_count: true,
        };
    }
    // Nano Banana 2 Lite (Gemini 3.1 Flash Lite Image): /edit, same contracts
    // as Nano Banana 2.
    if model.starts_with(FAL_NANO_BANANA_2_LITE_MODEL) {
        return FalImageProfile {
            edit_route: FalEditRoute::Edit,
            reference_images: FalReferenceImages::ImageUrls,
            max_input_images: NANO_BANANA_EDIT_MAX_INPUT_IMAGES,
            supports_count: true,
        };
    }
    // Grok Imagine: /edit, up to 3 refs via image_urls.
    if model.starts_with(FAL_GROK_IMAGINE_MODEL) {
        return FalImageProfile {
            edit_route: FalEditRoute::Edit,
            reference_images: FalReferenceImages::ImageUrls,
            max_input_images: GROK_IMAGINE_EDIT_MAX_INPUT_IMAGES,
            supports_count: true,
        };
    }
    // Default (Flux-style): single image_url ref via /image-to-image.
    FalImageProfile {
        edit_route: FalEditRoute::ImageToImage,
        reference_images: FalReferenceImages::ImageUrl,
        max_input_images: 1,
        supports_count: true,
    }
}

/// Build the edit endpoint path for a model (`None` when the model has no
/// edit route).
pub fn fal_edit_endpoint(model: &str) -> Option<String> {
    match resolve_fal_image_profile(model).edit_route {
        FalEditRoute::Edit => Some(format!("{}/edit", model.trim())),
        FalEditRoute::ImageToImage => Some(format!("{}/image-to-image", model.trim())),
        FalEditRoute::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn krea_2_has_no_edit_route() {
        let profile = resolve_fal_image_profile(FAL_KREA_2_MEDIUM_MODEL);
        assert_eq!(profile.edit_route, FalEditRoute::None);
        assert!(!profile.supports_count);
        assert_eq!(profile.max_input_images, KREA_STYLE_REFERENCE_MAX_INPUT_IMAGES);
        assert!(fal_edit_endpoint(FAL_KREA_2_LARGE_MODEL).is_none());
    }

    #[test]
    fn gpt_image_routes_to_edit_with_10_ref_cap() {
        let profile = resolve_fal_image_profile("openai/gpt-image-2");
        assert_eq!(profile.edit_route, FalEditRoute::Edit);
        assert_eq!(profile.max_input_images, GPT_IMAGE_EDIT_MAX_INPUT_IMAGES);
        assert_eq!(
            fal_edit_endpoint("openai/gpt-image-2").as_deref(),
            Some("openai/gpt-image-2/edit")
        );
    }

    #[test]
    fn nano_banana_generations() {
        // Legacy nano-banana: /edit, 3 refs.
        let legacy = resolve_fal_image_profile(FAL_NANO_BANANA_MODEL);
        assert_eq!(legacy.max_input_images, NANO_BANANA_LEGACY_EDIT_MAX_INPUT_IMAGES);
        // Nano Banana 2 (suffixed id): /edit, 14 refs.
        let v2 = resolve_fal_image_profile("fal-ai/nano-banana-2");
        assert_eq!(v2.max_input_images, NANO_BANANA_EDIT_MAX_INPUT_IMAGES);
        // Nano Banana 2 Lite.
        let lite = resolve_fal_image_profile(FAL_NANO_BANANA_2_LITE_MODEL);
        assert_eq!(lite.edit_route, FalEditRoute::Edit);
        assert_eq!(lite.max_input_images, NANO_BANANA_EDIT_MAX_INPUT_IMAGES);
    }

    #[test]
    fn grok_imagine_edit_route_and_caps() {
        let profile = resolve_fal_image_profile(FAL_GROK_IMAGINE_MODEL);
        assert_eq!(profile.edit_route, FalEditRoute::Edit);
        assert_eq!(profile.max_input_images, GROK_IMAGINE_EDIT_MAX_INPUT_IMAGES);
        assert_eq!(
            fal_edit_endpoint(FAL_GROK_IMAGINE_MODEL).as_deref(),
            Some("xai/grok-imagine-image/edit")
        );
    }

    #[test]
    fn flux_default_uses_image_to_image_single_ref() {
        let profile = resolve_fal_image_profile("fal-ai/flux/dev");
        assert_eq!(profile.edit_route, FalEditRoute::ImageToImage);
        assert_eq!(profile.reference_images, FalReferenceImages::ImageUrl);
        assert_eq!(profile.max_input_images, 1);
        assert_eq!(
            fal_edit_endpoint("fal-ai/flux/dev").as_deref(),
            Some("fal-ai/flux/dev/image-to-image")
        );
    }
}
