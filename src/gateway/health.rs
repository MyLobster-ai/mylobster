//! Cached health RPC snapshots with channel-state divergence refresh
//! (v2026.5.2 parity: "Refresh cached health RPC snapshots when channel
//! runtime state diverges").

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

/// Default TTL after which a cached health snapshot is refreshed even when
/// channel state has not diverged.
pub const HEALTH_SNAPSHOT_TTL: Duration = Duration::from_secs(30);

/// Compute a stable fingerprint of channel runtime state.
///
/// Only fields that reflect *runtime* state (connected/enabled/lastError/
/// mode) contribute — cosmetic fields (labels, ordering metadata) do not
/// cause a refresh.
pub fn channel_state_fingerprint(status: &serde_json::Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    fingerprint_value(status, &mut hasher, 0);
    hasher.finish()
}

const RUNTIME_KEYS: &[&str] = &[
    "connected",
    "enabled",
    "configured",
    "running",
    "lastError",
    "last_error",
    "mode",
    "status",
    "accountId",
    "account_id",
];

fn fingerprint_value(value: &serde_json::Value, hasher: &mut DefaultHasher, depth: usize) {
    if depth > 8 {
        return;
    }
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for k in keys {
                let v = &map[k];
                // At leaf level only runtime keys are hashed; containers are
                // always descended so nested account states are covered.
                if v.is_object() || v.is_array() {
                    k.hash(hasher);
                    fingerprint_value(v, hasher, depth + 1);
                } else if RUNTIME_KEYS.contains(&k.as_str()) {
                    k.hash(hasher);
                    v.to_string().hash(hasher);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                fingerprint_value(v, hasher, depth + 1);
            }
        }
        other => {
            other.to_string().hash(hasher);
        }
    }
}

struct CachedSnapshot {
    fingerprint: u64,
    stored_at: Instant,
    value: serde_json::Value,
}

/// Cache for the health RPC snapshot. A cached snapshot is reused only while
/// the channel-state fingerprint matches and the TTL has not elapsed; any
/// divergence forces an immediate refresh.
pub struct HealthSnapshotCache {
    ttl: Duration,
    inner: parking_lot::Mutex<Option<CachedSnapshot>>,
}

impl HealthSnapshotCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: parking_lot::Mutex::new(None),
        }
    }

    /// Return the cached snapshot when still valid for `fingerprint`.
    pub fn get_if_fresh(&self, fingerprint: u64) -> Option<serde_json::Value> {
        let guard = self.inner.lock();
        guard.as_ref().and_then(|c| {
            if c.fingerprint == fingerprint && c.stored_at.elapsed() < self.ttl {
                Some(c.value.clone())
            } else {
                None
            }
        })
    }

    /// Store a freshly built snapshot.
    pub fn store(&self, fingerprint: u64, value: serde_json::Value) {
        *self.inner.lock() = Some(CachedSnapshot {
            fingerprint,
            stored_at: Instant::now(),
            value,
        });
    }

    /// Invalidate the cache (e.g. on channel logout / config apply).
    pub fn invalidate(&self) {
        *self.inner.lock() = None;
    }
}

impl Default for HealthSnapshotCache {
    fn default() -> Self {
        Self::new(HEALTH_SNAPSHOT_TTL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fingerprint_stable_for_same_state() {
        let s = json!({"telegram": {"connected": true, "lastError": null}});
        assert_eq!(channel_state_fingerprint(&s), channel_state_fingerprint(&s));
    }

    #[test]
    fn fingerprint_diverges_on_runtime_change() {
        let a = json!({"telegram": {"connected": true}});
        let b = json!({"telegram": {"connected": false}});
        assert_ne!(channel_state_fingerprint(&a), channel_state_fingerprint(&b));
    }

    #[test]
    fn fingerprint_ignores_cosmetic_fields() {
        let a = json!({"telegram": {"connected": true, "label": "Telegram"}});
        let b = json!({"telegram": {"connected": true, "label": "TG (renamed)"}});
        assert_eq!(channel_state_fingerprint(&a), channel_state_fingerprint(&b));
    }

    #[test]
    fn fingerprint_sees_nested_account_state() {
        let a = json!({"channels": {"discord": {"accounts": [{"accountId": "a", "connected": true}]}}});
        let b = json!({"channels": {"discord": {"accounts": [{"accountId": "a", "connected": false}]}}});
        assert_ne!(channel_state_fingerprint(&a), channel_state_fingerprint(&b));
    }

    #[test]
    fn cache_hit_requires_matching_fingerprint() {
        let cache = HealthSnapshotCache::new(Duration::from_secs(60));
        cache.store(42, json!({"status": "ok"}));
        assert_eq!(cache.get_if_fresh(42).unwrap()["status"], "ok");
        // Diverged channel state → no cached value → caller rebuilds
        assert!(cache.get_if_fresh(43).is_none());
    }

    #[test]
    fn cache_respects_ttl() {
        let cache = HealthSnapshotCache::new(Duration::from_millis(0));
        cache.store(1, json!({"status": "ok"}));
        std::thread::sleep(Duration::from_millis(2));
        assert!(cache.get_if_fresh(1).is_none());
    }

    #[test]
    fn cache_invalidate_clears() {
        let cache = HealthSnapshotCache::new(Duration::from_secs(60));
        cache.store(1, json!({"a": 1}));
        cache.invalidate();
        assert!(cache.get_if_fresh(1).is_none());
    }
}
