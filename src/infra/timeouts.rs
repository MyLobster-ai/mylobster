//! Shared timeout clamping (v2026.7.1 parity: "Systemic timeout hardening").
//!
//! Caps extremely large / zero / negative timeouts across commands, image
//! understanding, file locks, queued tasks, and auto-replies. SDK semantics:
//! `timeoutMs: 0` disables the client watchdog (returns `None`).

/// Hard ceiling applied to any timeout (24 h).
pub const MAX_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;

/// Clamp a requested timeout.
///
/// * `None` → `Some(default_ms)`
/// * `Some(0)` → `None` (watchdog disabled, SDK `timeoutMs: 0` semantics)
/// * negative → `Some(default_ms)` (invalid input falls back to default)
/// * larger than `max_ms` (or the global ceiling) → capped
pub fn clamp_timeout_ms(requested: Option<i64>, default_ms: u64, max_ms: u64) -> Option<u64> {
    let ceiling = max_ms.min(MAX_TIMEOUT_MS).max(1);
    match requested {
        None => Some(default_ms.min(ceiling)),
        Some(0) => None,
        Some(n) if n < 0 => Some(default_ms.min(ceiling)),
        Some(n) => Some((n as u64).min(ceiling)),
    }
}

/// Clamp an already-unsigned timeout where 0 means "use default" rather than
/// "disable" (used for config-sourced values).
pub fn clamp_config_timeout_ms(configured: Option<u64>, default_ms: u64, max_ms: u64) -> u64 {
    let ceiling = max_ms.min(MAX_TIMEOUT_MS).max(1);
    match configured {
        None | Some(0) => default_ms.min(ceiling),
        Some(n) => n.min(ceiling),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_uses_default() {
        assert_eq!(clamp_timeout_ms(None, 30_000, 600_000), Some(30_000));
    }

    #[test]
    fn zero_disables_watchdog() {
        assert_eq!(clamp_timeout_ms(Some(0), 30_000, 600_000), None);
    }

    #[test]
    fn negative_falls_back_to_default() {
        assert_eq!(clamp_timeout_ms(Some(-5), 30_000, 600_000), Some(30_000));
    }

    #[test]
    fn huge_values_capped() {
        assert_eq!(
            clamp_timeout_ms(Some(i64::MAX), 30_000, 600_000),
            Some(600_000)
        );
        // Global ceiling always applies
        assert_eq!(
            clamp_timeout_ms(Some(i64::MAX), 30_000, u64::MAX),
            Some(MAX_TIMEOUT_MS)
        );
    }

    #[test]
    fn in_range_passthrough() {
        assert_eq!(clamp_timeout_ms(Some(45_000), 30_000, 600_000), Some(45_000));
    }

    #[test]
    fn default_larger_than_ceiling_is_capped() {
        assert_eq!(clamp_timeout_ms(None, 900_000, 600_000), Some(600_000));
    }

    #[test]
    fn config_clamp_zero_means_default() {
        assert_eq!(clamp_config_timeout_ms(Some(0), 10_000, 60_000), 10_000);
        assert_eq!(clamp_config_timeout_ms(None, 10_000, 60_000), 10_000);
        assert_eq!(clamp_config_timeout_ms(Some(99_999_999), 10_000, 60_000), 60_000);
        assert_eq!(clamp_config_timeout_ms(Some(5_000), 10_000, 60_000), 5_000);
    }
}
