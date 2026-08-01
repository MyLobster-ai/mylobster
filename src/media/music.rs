//! Music generation runtime helpers (v2026.5.2, refreshed to the current
//! upstream v2026.7.1 state).
//!
//! Two parity behaviors:
//!
//! * **Timeout floor** — too-small tool timeouts are raised to a
//!   provider-safe floor (originally 10s in v2026.5.2; the current upstream
//!   minimum is 120s with a 300s default) so short-timeout callers do not
//!   abort generations that providers cannot possibly satisfy. Below-minimum
//!   requests are normalized with an explanatory note.
//! * **Collapsed cascading abort fallback errors** — when a run is cancelled
//!   or times out, every fallback candidate fails with the same abort error;
//!   the failure summary collapses those into one root-cause line instead of
//!   a cascade of identical provider errors.

/// Bundled `music_generate` provider registry (v2026.5.x–6.x: fal MiniMax /
/// ACE-Step / Stable Audio plus OpenRouter Lyria).
pub const FAL_MUSIC_DEFAULT_MODEL: &str = "fal-ai/minimax-music/v2.6";
pub const FAL_MUSIC_ACE_STEP_MODEL: &str = "fal-ai/ace-step/prompt-to-audio";
pub const FAL_MUSIC_STABLE_AUDIO_MODEL: &str = "fal-ai/stable-audio-25/text-to-audio";
pub const OPENROUTER_MUSIC_DEFAULT_MODEL: &str = "google/lyria-3-pro-preview";
pub const OPENROUTER_MUSIC_CLIP_MODEL: &str = "google/lyria-3-clip-preview";

/// One music-generation provider row: `(provider id, models — first is the
/// default)`.
pub const MUSIC_GENERATION_PROVIDERS: &[(&str, &[&str])] = &[
    (
        "fal",
        &[
            FAL_MUSIC_DEFAULT_MODEL,
            FAL_MUSIC_ACE_STEP_MODEL,
            FAL_MUSIC_STABLE_AUDIO_MODEL,
        ],
    ),
    (
        "openrouter",
        &[OPENROUTER_MUSIC_DEFAULT_MODEL, OPENROUTER_MUSIC_CLIP_MODEL],
    ),
];

/// Models registered for a music-generation provider (first entry is the
/// default); empty for unknown providers.
pub fn music_models_for_provider(provider: &str) -> &'static [&'static str] {
    let normalized = provider.trim().to_ascii_lowercase();
    MUSIC_GENERATION_PROVIDERS
        .iter()
        .find(|(id, _)| *id == normalized)
        .map(|(_, models)| *models)
        .unwrap_or(&[])
}

/// Default music-generation timeout (current upstream state).
pub const DEFAULT_MUSIC_GENERATION_TIMEOUT_MS: u64 = 300_000;

/// Provider-safe minimum timeout (current upstream state; the v2026.5.2
/// change introduced the floor concept at 10s and it was later raised).
pub const MIN_MUSIC_GENERATION_TIMEOUT_MS: u64 = 120_000;

/// Result of timeout normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedTimeout {
    pub timeout_ms: u64,
    /// Present when the requested value was below the provider minimum.
    pub note: Option<String>,
}

/// Normalize a requested music-generation timeout: `None` uses the default;
/// below-minimum values are raised to the floor with an explanatory note.
pub fn normalize_music_generation_timeout_ms(timeout_ms: Option<u64>) -> NormalizedTimeout {
    match timeout_ms {
        None => NormalizedTimeout {
            timeout_ms: DEFAULT_MUSIC_GENERATION_TIMEOUT_MS,
            note: None,
        },
        Some(requested) if requested >= MIN_MUSIC_GENERATION_TIMEOUT_MS => NormalizedTimeout {
            timeout_ms: requested,
            note: None,
        },
        Some(requested) => NormalizedTimeout {
            timeout_ms: MIN_MUSIC_GENERATION_TIMEOUT_MS,
            note: Some(format!(
                "Timeout normalized: requested {}ms; used {}ms.",
                requested, MIN_MUSIC_GENERATION_TIMEOUT_MS
            )),
        },
    }
}

/// One failed provider/model candidate during capability fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackAttempt {
    pub provider: String,
    pub model: String,
    pub error: String,
}

impl FallbackAttempt {
    pub fn new(provider: &str, model: &str, error: &str) -> Self {
        Self {
            provider: provider.to_string(),
            model: model.to_string(),
            error: error.to_string(),
        }
    }
}

fn is_abort_like(attempt: &FallbackAttempt) -> bool {
    let message = attempt.error.trim().to_ascii_lowercase();
    message == "this operation was aborted"
        || message == "operation was aborted"
        || message.contains("operation was aborted")
        || message.contains("request was aborted")
}

fn attempt_ref(attempt: &FallbackAttempt) -> String {
    format!("{}/{}", attempt.provider, attempt.model)
}

fn format_attempt(attempt: &FallbackAttempt) -> String {
    format!("{}: {}", attempt_ref(attempt), attempt.error)
}

/// Format a capability failure summary, collapsing cascading abort fallback
/// errors into a single root-cause line (port of upstream
/// `formatCapabilityFailureAttempts`).
pub fn format_capability_failure_attempts(attempts: &[FallbackAttempt]) -> String {
    if attempts.is_empty() {
        return "unknown".to_string();
    }
    let aborted: Vec<&FallbackAttempt> = attempts.iter().filter(|a| is_abort_like(a)).collect();
    if aborted.is_empty() {
        return attempts
            .iter()
            .map(format_attempt)
            .collect::<Vec<_>>()
            .join(" | ");
    }
    let aborted_summary = format!(
        "{} fallback(s) aborted after the request was cancelled or timed out: {}",
        aborted.len(),
        aborted
            .iter()
            .map(|a| attempt_ref(a))
            .collect::<Vec<_>>()
            .join(", ")
    );
    if aborted.len() == attempts.len() {
        return aborted_summary;
    }
    let primary = attempts
        .iter()
        .filter(|a| !is_abort_like(a))
        .map(format_attempt)
        .collect::<Vec<_>>()
        .join(" | ");
    format!("{} | {}", primary, aborted_summary)
}

/// Build the final all-candidates-failed error message. With a single
/// attempt, the raw error passes through; multiple attempts get the
/// collapsed summary.
pub fn build_capability_failure_message(
    capability_label: &str,
    attempts: &[FallbackAttempt],
) -> String {
    if attempts.len() == 1 {
        return attempts[0].error.clone();
    }
    format!(
        "All {} models failed ({}): {}",
        capability_label,
        attempts.len(),
        format_capability_failure_attempts(attempts)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // Timeout floor
    // ------------------------------------------------------------------

    #[test]
    fn default_timeout_when_unset() {
        let normalized = normalize_music_generation_timeout_ms(None);
        assert_eq!(normalized.timeout_ms, DEFAULT_MUSIC_GENERATION_TIMEOUT_MS);
        assert!(normalized.note.is_none());
    }

    #[test]
    fn timeout_at_or_above_minimum_is_honored() {
        let normalized = normalize_music_generation_timeout_ms(Some(150_000));
        assert_eq!(normalized.timeout_ms, 150_000);
        assert!(normalized.note.is_none());
        let at_min = normalize_music_generation_timeout_ms(Some(MIN_MUSIC_GENERATION_TIMEOUT_MS));
        assert_eq!(at_min.timeout_ms, MIN_MUSIC_GENERATION_TIMEOUT_MS);
    }

    #[test]
    fn too_small_timeout_raised_to_floor_with_note() {
        // The original v2026.5.2 fix: a 5s tool timeout cannot satisfy any
        // music provider; it is raised to the safe floor.
        let normalized = normalize_music_generation_timeout_ms(Some(5_000));
        assert_eq!(normalized.timeout_ms, MIN_MUSIC_GENERATION_TIMEOUT_MS);
        let note = normalized.note.unwrap();
        assert!(note.contains("5000ms"));
        assert!(note.contains("120000ms"));
    }

    // ------------------------------------------------------------------
    // Abort collapse
    // ------------------------------------------------------------------

    fn abort(provider: &str, model: &str) -> FallbackAttempt {
        FallbackAttempt::new(provider, model, "This operation was aborted")
    }

    #[test]
    fn empty_attempts_is_unknown() {
        assert_eq!(format_capability_failure_attempts(&[]), "unknown");
    }

    #[test]
    fn non_abort_failures_listed_individually() {
        let attempts = vec![
            FallbackAttempt::new("google", "lyria-3", "quota exceeded"),
            FallbackAttempt::new("suno", "v5", "invalid key"),
        ];
        let summary = format_capability_failure_attempts(&attempts);
        assert_eq!(summary, "google/lyria-3: quota exceeded | suno/v5: invalid key");
    }

    #[test]
    fn all_aborts_collapse_to_single_root_cause_line() {
        let attempts = vec![abort("google", "lyria-3"), abort("suno", "v5")];
        let summary = format_capability_failure_attempts(&attempts);
        assert_eq!(
            summary,
            "2 fallback(s) aborted after the request was cancelled or timed out: google/lyria-3, suno/v5"
        );
    }

    #[test]
    fn mixed_failures_keep_primary_and_collapse_aborts() {
        let attempts = vec![
            FallbackAttempt::new("google", "lyria-3", "quota exceeded"),
            abort("suno", "v5"),
            abort("minimax", "music-2"),
        ];
        let summary = format_capability_failure_attempts(&attempts);
        assert!(summary.starts_with("google/lyria-3: quota exceeded | "));
        assert!(summary.contains("2 fallback(s) aborted"));
        assert!(summary.contains("suno/v5, minimax/music-2"));
        // Cascading identical abort messages appear once, not per candidate.
        assert_eq!(summary.matches("aborted after the request").count(), 1);
    }

    #[test]
    fn abort_detection_matches_known_shapes() {
        for msg in [
            "This operation was aborted",
            "operation was aborted",
            "fetch failed: The operation was aborted midway",
            "request was aborted by the client",
        ] {
            assert!(
                is_abort_like(&FallbackAttempt::new("p", "m", msg)),
                "{} should be abort-like",
                msg
            );
        }
        assert!(!is_abort_like(&FallbackAttempt::new("p", "m", "429 rate limited")));
    }

    #[test]
    fn single_attempt_failure_passes_raw_error_through() {
        let attempts = vec![FallbackAttempt::new("google", "lyria-3", "boom")];
        assert_eq!(
            build_capability_failure_message("music generation", &attempts),
            "boom"
        );
    }

    #[test]
    fn multi_attempt_failure_uses_summary() {
        let attempts = vec![abort("a", "m1"), abort("b", "m2")];
        let msg = build_capability_failure_message("music generation", &attempts);
        assert!(msg.starts_with("All music generation models failed (2):"));
        assert!(msg.contains("2 fallback(s) aborted"));
    }

    // ------------------------------------------------------------------
    // music_generate provider registry (v2026.5.x–6.x)
    // ------------------------------------------------------------------

    #[test]
    fn fal_music_models_registered_with_minimax_default() {
        let models = music_models_for_provider("fal");
        assert_eq!(models[0], FAL_MUSIC_DEFAULT_MODEL);
        assert!(models.contains(&FAL_MUSIC_ACE_STEP_MODEL));
        assert!(models.contains(&FAL_MUSIC_STABLE_AUDIO_MODEL));
    }

    #[test]
    fn openrouter_lyria_registered() {
        let models = music_models_for_provider("OpenRouter");
        assert_eq!(models[0], OPENROUTER_MUSIC_DEFAULT_MODEL);
        assert!(models.contains(&OPENROUTER_MUSIC_CLIP_MODEL));
    }

    #[test]
    fn unknown_music_provider_has_no_models() {
        assert!(music_models_for_provider("suno").is_empty());
    }
}
