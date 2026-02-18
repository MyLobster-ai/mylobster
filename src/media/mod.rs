/// Default maximum image dimension in pixels (reduced from 2048 in v2026.2.17).
pub const DEFAULT_IMAGE_MAX_DIMENSION_PX: u32 = 1200;

/// Default maximum image file size in bytes (5 MB).
pub const DEFAULT_IMAGE_MAX_BYTES: usize = 5 * 1024 * 1024;

/// Limits applied when sanitizing images before sending to AI providers.
pub struct ImageSanitizationLimits {
    pub max_dimension_px: u32,
    pub max_bytes: usize,
}

/// Resolve image sanitization limits from config, falling back to defaults.
pub fn resolve_limits(config_dim: Option<u32>) -> ImageSanitizationLimits {
    ImageSanitizationLimits {
        max_dimension_px: config_dim.unwrap_or(DEFAULT_IMAGE_MAX_DIMENSION_PX),
        max_bytes: DEFAULT_IMAGE_MAX_BYTES,
    }
}
