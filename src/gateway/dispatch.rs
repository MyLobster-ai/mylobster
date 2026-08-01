//! Dispatch-path caches (v2026.5.2 parity: "Avoid repeated
//! plugin-tool-descriptor config hashing").
//!
//! Large runtime configs made per-reply plugin-tool-descriptor hashing a
//! startup bottleneck upstream. The cache here memoizes the descriptor hash
//! keyed by a cheap config generation counter, so the expensive SHA-256 over
//! the serialized descriptor set runs once per config generation instead of
//! once per reply.

use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};

/// Compute the (expensive) hash of a plugin-tool-descriptor set.
pub fn compute_descriptor_hash(descriptors: &serde_json::Value) -> String {
    let canonical = serde_json::to_string(descriptors).unwrap_or_default();
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

/// Memoizes the plugin-tool-descriptor hash per config generation.
#[derive(Default)]
pub struct ToolDescriptorHashCache {
    inner: parking_lot::Mutex<Option<(u64, String)>>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl ToolDescriptorHashCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached hash for `generation`, or compute-and-store via
    /// `compute` on generation change.
    pub fn get_or_compute<F>(&self, generation: u64, compute: F) -> String
    where
        F: FnOnce() -> String,
    {
        {
            let guard = self.inner.lock();
            if let Some((cached_gen, hash)) = guard.as_ref() {
                if *cached_gen == generation {
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    return hash.clone();
                }
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let hash = compute();
        *self.inner.lock() = Some((generation, hash.clone()));
        hash
    }

    pub fn stats(&self) -> (u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        )
    }

    pub fn invalidate(&self) {
        *self.inner.lock() = None;
    }
}

/// Monotonic config generation counter. Bumped whenever the runtime config
/// is replaced (config.patch/set/apply/reload) so descriptor-hash consumers
/// can key their caches cheaply.
#[derive(Default)]
pub struct ConfigGeneration(AtomicU64);

impl ConfigGeneration {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn current(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }

    pub fn bump(&self) -> u64 {
        self.0.fetch_add(1, Ordering::AcqRel) + 1
    }
}

// ============================================================================
// Version-scoped cache (v2026.4.29: "stale-session recovery + version-scoped
// update caches")
// ============================================================================

/// A cache whose entries are only valid for the gateway version that wrote
/// them — a new binary version never reuses stale cached values (e.g. update
/// metadata) from a previous version.
pub struct VersionScopedCache {
    version: String,
    entries: parking_lot::Mutex<std::collections::HashMap<String, serde_json::Value>>,
}

impl VersionScopedCache {
    pub fn new(version: &str) -> Self {
        Self {
            version: version.to_string(),
            entries: parking_lot::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Get an entry written by the same gateway version.
    pub fn get(&self, key: &str, current_version: &str) -> Option<serde_json::Value> {
        if current_version != self.version {
            return None;
        }
        self.entries.lock().get(key).cloned()
    }

    pub fn store(&self, key: &str, value: serde_json::Value) {
        self.entries.lock().insert(key.to_string(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn version_scoped_cache_rejects_other_versions() {
        let cache = VersionScopedCache::new("1.0.0");
        cache.store("update-check", serde_json::json!({"latest": "1.0.1"}));
        assert!(cache.get("update-check", "1.0.0").is_some());
        // A different (upgraded) version must not see stale entries.
        assert!(cache.get("update-check", "1.0.2").is_none());
        assert!(cache.get("missing", "1.0.0").is_none());
    }

    #[test]
    fn descriptor_hash_deterministic_and_sensitive() {
        let a = serde_json::json!([{"name": "t1", "schema": {"type": "object"}}]);
        let b = serde_json::json!([{"name": "t2", "schema": {"type": "object"}}]);
        assert_eq!(compute_descriptor_hash(&a), compute_descriptor_hash(&a));
        assert_ne!(compute_descriptor_hash(&a), compute_descriptor_hash(&b));
    }

    #[test]
    fn cache_computes_once_per_generation() {
        let cache = ToolDescriptorHashCache::new();
        let calls = AtomicUsize::new(0);
        let compute = || {
            calls.fetch_add(1, Ordering::Relaxed);
            "hash-1".to_string()
        };

        assert_eq!(cache.get_or_compute(1, compute), "hash-1");
        assert_eq!(
            cache.get_or_compute(1, || {
                calls.fetch_add(1, Ordering::Relaxed);
                "hash-1".to_string()
            }),
            "hash-1"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1, "second call must hit cache");
        let (hits, misses) = cache.stats();
        assert_eq!((hits, misses), (1, 1));
    }

    #[test]
    fn cache_recomputes_on_generation_change() {
        let cache = ToolDescriptorHashCache::new();
        assert_eq!(cache.get_or_compute(1, || "h1".to_string()), "h1");
        assert_eq!(cache.get_or_compute(2, || "h2".to_string()), "h2");
        assert_eq!(cache.get_or_compute(2, || "never".to_string()), "h2");
        let (hits, misses) = cache.stats();
        assert_eq!((hits, misses), (1, 2));
    }

    #[test]
    fn cache_invalidate_forces_recompute() {
        let cache = ToolDescriptorHashCache::new();
        cache.get_or_compute(1, || "h1".to_string());
        cache.invalidate();
        assert_eq!(cache.get_or_compute(1, || "h1b".to_string()), "h1b");
    }

    #[test]
    fn config_generation_monotonic() {
        let g = ConfigGeneration::new();
        assert_eq!(g.current(), 0);
        assert_eq!(g.bump(), 1);
        assert_eq!(g.bump(), 2);
        assert_eq!(g.current(), 2);
    }
}
