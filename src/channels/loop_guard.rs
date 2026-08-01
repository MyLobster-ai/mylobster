//! Channel-turn kernel bot pair loop guard.
//!
//! Ported from OpenClaw `src/plugin-sdk/pair-loop-guard-runtime.ts` +
//! `src/channels/turn/bot-loop-protection.ts` (v2026.5.x): suppresses
//! repeated bidirectional bot-to-bot reply loops per
//! (scope, conversation, unordered sender/receiver pair). Config chain:
//! per-channel/account `botLoopProtection` → `channels.defaults.
//! botLoopProtection` → built-in defaults (`maxEventsPerWindow: 20`,
//! `windowSeconds: 60`, `cooldownSeconds: 60`).

use crate::config::BotLoopProtectionConfig;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Resolved guard settings in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairLoopGuardSettings {
    pub enabled: bool,
    pub max_events_per_window: u32,
    pub window_ms: u64,
    pub cooldown_ms: u64,
}

/// Built-in defaults (upstream `DEFAULT_PAIR_LOOP_GUARD_CONFIG`).
pub const DEFAULT_MAX_EVENTS_PER_WINDOW: u32 = 20;
pub const DEFAULT_WINDOW_SECONDS: u32 = 60;
pub const DEFAULT_COOLDOWN_SECONDS: u32 = 60;

fn positive(value: Option<u32>) -> Option<u32> {
    value.filter(|v| *v > 0)
}

/// Resolve runtime settings from the config chain and the channel-level
/// capability gate (`default_enabled`). Mirror of
/// `resolvePairLoopGuardSettings`: channel gates can disable protection even
/// when config enables it.
pub fn resolve_pair_loop_guard_settings(
    config: Option<&BotLoopProtectionConfig>,
    defaults_config: Option<&BotLoopProtectionConfig>,
    default_enabled: bool,
) -> PairLoopGuardSettings {
    let configured_enabled = config
        .and_then(|c| c.enabled)
        .or_else(|| defaults_config.and_then(|c| c.enabled))
        .unwrap_or(true);
    let max_events = positive(config.and_then(|c| c.max_events_per_window))
        .or_else(|| positive(defaults_config.and_then(|c| c.max_events_per_window)))
        .unwrap_or(DEFAULT_MAX_EVENTS_PER_WINDOW);
    let window_seconds = positive(config.and_then(|c| c.window_seconds))
        .or_else(|| positive(defaults_config.and_then(|c| c.window_seconds)))
        .unwrap_or(DEFAULT_WINDOW_SECONDS);
    let cooldown_seconds = positive(config.and_then(|c| c.cooldown_seconds))
        .or_else(|| positive(defaults_config.and_then(|c| c.cooldown_seconds)))
        .unwrap_or(DEFAULT_COOLDOWN_SECONDS);
    PairLoopGuardSettings {
        enabled: default_enabled && configured_enabled,
        max_events_per_window: max_events,
        window_ms: window_seconds as u64 * 1000,
        cooldown_ms: cooldown_seconds as u64 * 1000,
    }
}

/// Result of recording one pair interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairLoopGuardResult {
    Pass,
    Suppressed { cooldown_until_ms: u64 },
}

impl PairLoopGuardResult {
    pub fn suppressed(&self) -> bool {
        matches!(self, PairLoopGuardResult::Suppressed { .. })
    }
}

#[derive(Debug, Default)]
struct PairEntry {
    recent_ms: Vec<u64>,
    window_ms: u64,
    cooldown_started_at_ms: u64,
    cooldown_until_ms: u64,
}

/// In-memory guard for suppressing repeated bidirectional bot pair loops.
pub struct PairLoopGuard {
    tracked: Mutex<GuardState>,
    prune_interval_ms: u64,
}

#[derive(Default)]
struct GuardState {
    entries: HashMap<String, PairEntry>,
    next_prune_at_ms: u64,
}

const KEY_SEPARATOR: char = '\u{1}';

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn build_pair_key(scope_id: &str, conversation_id: &str, sender_id: &str, receiver_id: &str) -> String {
    // Sort sender/receiver so A→B and B→A count as the same bot loop pair.
    let (lhs, rhs) = if sender_id < receiver_id {
        (sender_id, receiver_id)
    } else {
        (receiver_id, sender_id)
    };
    format!("{scope_id}{KEY_SEPARATOR}{conversation_id}{KEY_SEPARATOR}{lhs}{KEY_SEPARATOR}{rhs}")
}

impl PairLoopGuard {
    pub fn new(prune_interval_ms: u64) -> Self {
        Self {
            tracked: Mutex::new(GuardState::default()),
            prune_interval_ms,
        }
    }

    /// Record one sender/receiver interaction; reports whether it enters or
    /// is inside cooldown.
    pub fn record_and_check(
        &self,
        scope_id: &str,
        conversation_id: &str,
        sender_id: &str,
        receiver_id: &str,
        settings: PairLoopGuardSettings,
        now_ms: Option<u64>,
    ) -> PairLoopGuardResult {
        if !settings.enabled
            || scope_id.is_empty()
            || conversation_id.is_empty()
            || sender_id.is_empty()
            || receiver_id.is_empty()
            || sender_id == receiver_id
            || settings.max_events_per_window == 0
            || settings.window_ms == 0
            || settings.cooldown_ms == 0
        {
            return PairLoopGuardResult::Pass;
        }

        let now = now_ms.unwrap_or_else(now_epoch_ms);
        let mut state = self.tracked.lock();

        // Bounded periodic pruning.
        if self.prune_interval_ms > 0 && now >= state.next_prune_at_ms {
            state.next_prune_at_ms = now + self.prune_interval_ms;
            state.entries.retain(|_, entry| {
                let cutoff = now.saturating_sub(entry.window_ms);
                entry.recent_ms.retain(|t| *t > cutoff);
                !entry.recent_ms.is_empty() || entry.cooldown_until_ms > now
            });
        }

        let key = build_pair_key(scope_id, conversation_id, sender_id, receiver_id);
        let entry = state.entries.entry(key).or_default();

        if entry.cooldown_started_at_ms <= now && entry.cooldown_until_ms > now {
            return PairLoopGuardResult::Suppressed {
                cooldown_until_ms: entry.cooldown_until_ms,
            };
        }

        entry.window_ms = settings.window_ms;
        let cutoff = now.saturating_sub(settings.window_ms);
        entry.recent_ms.retain(|t| *t > cutoff);
        entry.recent_ms.push(now);
        let current_window_events = entry.recent_ms.iter().filter(|t| **t <= now).count() as u32;
        if current_window_events > settings.max_events_per_window {
            entry.cooldown_started_at_ms = now;
            entry.cooldown_until_ms = now + settings.cooldown_ms;
            // Past events must not extend suppression once cooldown starts.
            entry.recent_ms.retain(|t| *t > now);
            return PairLoopGuardResult::Suppressed {
                cooldown_until_ms: entry.cooldown_until_ms,
            };
        }
        PairLoopGuardResult::Pass
    }

    /// Clear all tracked state (test isolation).
    pub fn clear(&self) {
        let mut state = self.tracked.lock();
        state.entries.clear();
        state.next_prune_at_ms = 0;
    }
}

/// Shared process-wide guard used by channel turn adapters (mirror of the
/// module-level `channelBotPairLoopGuard` upstream; 60 s prune interval).
pub fn shared_channel_pair_loop_guard() -> &'static PairLoopGuard {
    static GUARD: once_cell::sync::Lazy<PairLoopGuard> =
        once_cell::sync::Lazy::new(|| PairLoopGuard::new(60_000));
    &GUARD
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(max: u32, window_s: u32, cooldown_s: u32) -> PairLoopGuardSettings {
        PairLoopGuardSettings {
            enabled: true,
            max_events_per_window: max,
            window_ms: window_s as u64 * 1000,
            cooldown_ms: cooldown_s as u64 * 1000,
        }
    }

    #[test]
    fn defaults_resolution() {
        let s = resolve_pair_loop_guard_settings(None, None, true);
        assert!(s.enabled);
        assert_eq!(s.max_events_per_window, 20);
        assert_eq!(s.window_ms, 60_000);
        assert_eq!(s.cooldown_ms, 60_000);
        // Channel gate disables even when config would enable.
        let s = resolve_pair_loop_guard_settings(None, None, false);
        assert!(!s.enabled);
    }

    #[test]
    fn config_chain_precedence() {
        let account = BotLoopProtectionConfig {
            enabled: None,
            max_events_per_window: Some(3),
            window_seconds: None,
            cooldown_seconds: Some(5),
        };
        let defaults = BotLoopProtectionConfig {
            enabled: Some(false),
            max_events_per_window: Some(10),
            window_seconds: Some(120),
            cooldown_seconds: Some(999),
        };
        let s = resolve_pair_loop_guard_settings(Some(&account), Some(&defaults), true);
        assert!(!s.enabled); // defaults' enabled=false applies when account unset
        assert_eq!(s.max_events_per_window, 3);
        assert_eq!(s.window_ms, 120_000);
        assert_eq!(s.cooldown_ms, 5_000);
        // Zero/invalid values fall through to the next layer.
        let zeroed = BotLoopProtectionConfig {
            enabled: Some(true),
            max_events_per_window: Some(0),
            window_seconds: Some(0),
            cooldown_seconds: Some(0),
        };
        let s = resolve_pair_loop_guard_settings(Some(&zeroed), None, true);
        assert_eq!(s.max_events_per_window, 20);
        assert_eq!(s.window_ms, 60_000);
        assert_eq!(s.cooldown_ms, 60_000);
    }

    #[test]
    fn suppresses_after_threshold_and_cools_down() {
        let guard = PairLoopGuard::new(0);
        let s = settings(3, 60, 60);
        let mut now = 1_000_000;
        for _ in 0..3 {
            assert_eq!(
                guard.record_and_check("scope", "conv", "botA", "botB", s, Some(now)),
                PairLoopGuardResult::Pass
            );
            now += 100;
        }
        // 4th event in window exceeds threshold.
        let r = guard.record_and_check("scope", "conv", "botA", "botB", s, Some(now));
        assert!(r.suppressed());
        // Still suppressed inside cooldown, direction-independent.
        let r = guard.record_and_check("scope", "conv", "botB", "botA", s, Some(now + 1_000));
        assert!(r.suppressed());
        // After cooldown expires, events pass again.
        let r = guard.record_and_check("scope", "conv", "botA", "botB", s, Some(now + 61_000));
        assert_eq!(r, PairLoopGuardResult::Pass);
    }

    #[test]
    fn window_expiry_resets_counting() {
        let guard = PairLoopGuard::new(0);
        let s = settings(2, 10, 60);
        assert!(!guard
            .record_and_check("s", "c", "a", "b", s, Some(0))
            .suppressed());
        assert!(!guard
            .record_and_check("s", "c", "a", "b", s, Some(1_000))
            .suppressed());
        // Both prior events fall out of the 10s window.
        assert!(!guard
            .record_and_check("s", "c", "a", "b", s, Some(12_000))
            .suppressed());
    }

    #[test]
    fn distinct_pairs_and_conversations_isolated() {
        let guard = PairLoopGuard::new(0);
        let s = settings(1, 60, 60);
        let t0 = 1_000_000;
        assert!(!guard
            .record_and_check("s", "c1", "a", "b", s, Some(t0))
            .suppressed());
        assert!(guard
            .record_and_check("s", "c1", "b", "a", s, Some(t0 + 1))
            .suppressed());
        // Different conversation: independent counter.
        assert!(!guard
            .record_and_check("s", "c2", "a", "b", s, Some(t0 + 2))
            .suppressed());
        // Different pair: independent counter.
        assert!(!guard
            .record_and_check("s", "c1", "a", "x", s, Some(t0 + 3))
            .suppressed());
    }

    #[test]
    fn disabled_and_degenerate_inputs_pass() {
        let guard = PairLoopGuard::new(0);
        let mut s = settings(1, 60, 60);
        s.enabled = false;
        assert!(!guard
            .record_and_check("s", "c", "a", "b", s, Some(0))
            .suppressed());
        let s = settings(1, 60, 60);
        // Self-pair never suppresses.
        assert!(!guard
            .record_and_check("s", "c", "a", "a", s, Some(0))
            .suppressed());
        // Missing ids never suppress.
        assert!(!guard
            .record_and_check("", "c", "a", "b", s, Some(0))
            .suppressed());
    }
}
