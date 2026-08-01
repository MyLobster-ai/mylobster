//! Search payload cache (v2026.7.1 parity).
//!
//! Ports OpenClaw's `web-shared.ts` cache semantics: a bounded (100-entry)
//! TTL map with insertion-order eviction, shared by all web-search providers.
//! Cache keys are normalized (lowercased) and built from provider-specific
//! dimensions so distinct endpoints/base URLs never collide.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Matches upstream `DEFAULT_CACHE_MAX_ENTRIES`.
pub const SEARCH_CACHE_MAX_ENTRIES: usize = 100;

struct CacheEntry {
    value: serde_json::Value,
    expires_at: Instant,
}

/// Bounded TTL cache with insertion-order (oldest-inserted-first) eviction.
pub struct SearchCache {
    entries: HashMap<String, CacheEntry>,
    /// Insertion order for capacity eviction. May contain stale keys for
    /// entries that were overwritten; those are skipped on eviction.
    order: VecDeque<String>,
    max_entries: usize,
}

impl SearchCache {
    pub fn new(max_entries: usize) -> Self {
        Self { entries: HashMap::new(), order: VecDeque::new(), max_entries }
    }

    pub fn read(&mut self, key: &str) -> Option<serde_json::Value> {
        let expired = match self.entries.get(key) {
            Some(entry) => Instant::now() > entry.expires_at,
            None => return None,
        };
        if expired {
            self.entries.remove(key);
            return None;
        }
        self.entries.get(key).map(|e| e.value.clone())
    }

    pub fn write(&mut self, key: &str, value: serde_json::Value, ttl_ms: u64) {
        // TTL <= 0 disables caching entirely, mirroring upstream writeCache.
        if ttl_ms == 0 {
            return;
        }
        if self.entries.len() >= self.max_entries && !self.entries.contains_key(key) {
            // Evict the oldest inserted live entry.
            while let Some(oldest) = self.order.pop_front() {
                if self.entries.remove(&oldest).is_some() {
                    break;
                }
            }
        }
        self.order.push_back(key.to_string());
        self.entries.insert(
            key.to_string(),
            CacheEntry {
                value,
                expires_at: Instant::now() + Duration::from_millis(ttl_ms),
            },
        );
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Normalize a cache key: lowercase (upstream `normalizeCacheKey`).
pub fn normalize_cache_key(value: &str) -> String {
    value.to_lowercase()
}

/// Builds a normalized cache key from provider-specific search dimensions.
/// `None` parts serialize as `"default"` so optional filters still partition.
pub fn build_search_cache_key(parts: &[Option<&str>]) -> String {
    normalize_cache_key(
        &parts
            .iter()
            .map(|p| p.unwrap_or("default"))
            .collect::<Vec<_>>()
            .join(":"),
    )
}

fn global_cache() -> &'static Mutex<SearchCache> {
    static CACHE: once_cell::sync::Lazy<Mutex<SearchCache>> =
        once_cell::sync::Lazy::new(|| Mutex::new(SearchCache::new(SEARCH_CACHE_MAX_ENTRIES)));
    &CACHE
}

/// Reads a cached search payload, marking it `cached: true` so provider
/// responses can disclose cache hits.
pub fn read_cached_search_payload(cache_key: &str) -> Option<serde_json::Value> {
    let mut cache = global_cache().lock().ok()?;
    let mut value = cache.read(cache_key)?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert("cached".to_string(), serde_json::Value::Bool(true));
    }
    Some(value)
}

/// Stores one provider search payload with its provider-selected TTL.
pub fn write_cached_search_payload(cache_key: &str, payload: &serde_json::Value, ttl_ms: u64) {
    if let Ok(mut cache) = global_cache().lock() {
        cache.write(cache_key, payload.clone(), ttl_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cache_key_uses_default_for_missing_parts() {
        let key = build_search_cache_key(&[Some("brave"), None, Some("Query")]);
        assert_eq!(key, "brave:default:query");
    }

    #[test]
    fn cache_keys_partition_by_endpoint() {
        let a = build_search_cache_key(&[
            Some("brave"),
            Some("web"),
            Some("https://api.search.brave.com"),
            Some("q"),
        ]);
        let b = build_search_cache_key(&[
            Some("brave"),
            Some("web"),
            Some("https://proxy.internal"),
            Some("q"),
        ]);
        let c = build_search_cache_key(&[
            Some("brave"),
            Some("llm-context"),
            Some("https://api.search.brave.com"),
            Some("q"),
        ]);
        assert_ne!(a, b, "different base URLs must not collide");
        assert_ne!(a, c, "web and llm-context endpoints must not collide");
    }

    #[test]
    fn read_returns_written_value_before_ttl() {
        let mut cache = SearchCache::new(10);
        cache.write("k", json!({"v": 1}), 60_000);
        assert_eq!(cache.read("k"), Some(json!({"v": 1})));
    }

    #[test]
    fn zero_ttl_disables_write() {
        let mut cache = SearchCache::new(10);
        cache.write("k", json!({"v": 1}), 0);
        assert_eq!(cache.read("k"), None);
        assert!(cache.is_empty());
    }

    #[test]
    fn expired_entries_are_dropped_on_read() {
        let mut cache = SearchCache::new(10);
        cache.write("k", json!(1), 1);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(cache.read("k"), None);
        assert!(cache.is_empty());
    }

    #[test]
    fn capacity_evicts_oldest_inserted_entry() {
        let mut cache = SearchCache::new(3);
        cache.write("a", json!(1), 60_000);
        cache.write("b", json!(2), 60_000);
        cache.write("c", json!(3), 60_000);
        cache.write("d", json!(4), 60_000);
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.read("a"), None, "oldest entry evicted");
        assert_eq!(cache.read("d"), Some(json!(4)));
    }

    #[test]
    fn overwriting_existing_key_does_not_evict_others() {
        let mut cache = SearchCache::new(2);
        cache.write("a", json!(1), 60_000);
        cache.write("b", json!(2), 60_000);
        cache.write("a", json!(10), 60_000);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.read("a"), Some(json!(10)));
        assert_eq!(cache.read("b"), Some(json!(2)));
    }

    #[test]
    fn global_read_marks_cached_flag() {
        let key = "global-cache-test-key-unique";
        write_cached_search_payload(key, &json!({"provider": "test"}), 60_000);
        let cached = read_cached_search_payload(key).expect("cached payload");
        assert_eq!(cached["cached"], true);
        assert_eq!(cached["provider"], "test");
    }
}
