use dashmap::DashMap;
use tracing::trace;

use crate::{entry::CachedResponse, key::CacheKey};

/// In-process response cache backed by a `DashMap`.
///
/// ## Capacity and eviction
///
/// The map is bounded by `capacity` entries. When the limit is reached,
/// a random shard victim is evicted. This is not a true LRU — a proper LRU
/// would require a doubly-linked list under a global lock, which defeats
/// the purpose of using DashMap.
///
/// For a true LRU at this layer consider `moka` crate, which provides a
/// concurrent, bounded, LRU-evicting cache. The interface here is identical
/// so swapping it in is mechanical.
///
/// ## Soft expiry
///
/// L1 does **not** run a background reaper. Entries are checked on read and
/// evicted lazily. Redis (L2) handles TTL enforcement authoritatively — L1
/// is just a fast-path shortcut. A stale L1 hit that passes the soft expiry
/// check will return stale data for at most one extra TTL period, then miss and
/// refresh from L2.
pub struct L1Cache {
    store: DashMap<String, CachedResponse>,
    capacity: usize,
}

impl L1Cache {
    pub fn new(capacity: usize) -> Self {
        Self {
            store: DashMap::with_capacity(capacity.min(4096)),
            capacity,
        }
    }

    /// Look up an entry. Returns `None` if absent or expired.
    pub fn get(&self, key: &CacheKey) -> Option<CachedResponse> {
        let entry = self.store.get(key.as_str())?;

        if entry.is_expired() {
            // Lazy eviction — drop the read guard before removing
            drop(entry);
            self.store.remove(key.as_str());
            trace!(key = %key, "L1 expired entry evicted");
            return None;
        }

        trace!(key = %key, "L1 hit");
        Some(entry.clone())
    }

    /// Store a response. Evicts a random entry if at capacity.
    pub fn put(&self, key: &CacheKey, response: CachedResponse) {
        if self.store.len() >= self.capacity {
            self.evict_one();
        }
        self.store.insert(key.as_str().to_string(), response);
        trace!(key = %key, "L1 stored");
    }

    /// Remove an entry (used when we know the upstream response changed).
    pub fn invalidate(&self, key: &CacheKey) {
        self.store.remove(key.as_str());
    }

    pub fn len(&self) -> usize {
        self.store.len()
    }

    pub fn is_empty(&self) -> bool {
        self.store.is_empty()
    }

    /// Evict the first entry we find — O(1), no LRU bookkeeping.
    ///
    /// DashMap shards are iterated in an arbitrary but consistent order.
    /// In practice the oldest entries tend to be at the front of early shards.
    fn evict_one(&self) {
        if let Some(entry) = self.store.iter().next() {
            let key = entry.key().clone();
            drop(entry);
            self.store.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::time::Duration;

    fn make_entry(ttl_secs: u64) -> CachedResponse {
        CachedResponse::new(200, vec![], Bytes::from("body"), Duration::from_secs(ttl_secs), vec![])
    }

    fn key(s: &str) -> CacheKey {
        CacheKey::new("GET", "route", s, None)
    }

    #[test]
    fn put_and_get() {
        let cache = L1Cache::new(100);
        cache.put(&key("/a"), make_entry(60));
        assert!(cache.get(&key("/a")).is_some());
    }

    #[test]
    fn miss_returns_none() {
        let cache = L1Cache::new(100);
        assert!(cache.get(&key("/missing")).is_none());
    }

    #[test]
    fn expired_entry_returns_none() {
        let cache = L1Cache::new(100);
        cache.put(&key("/b"), make_entry(0)); // TTL = 0 → immediately expired
        assert!(cache.get(&key("/b")).is_none());
    }

    #[test]
    fn invalidate_removes_entry() {
        let cache = L1Cache::new(100);
        cache.put(&key("/c"), make_entry(60));
        cache.invalidate(&key("/c"));
        assert!(cache.get(&key("/c")).is_none());
    }

    #[test]
    fn evicts_at_capacity() {
        let cache = L1Cache::new(3);
        for i in 0..5 {
            cache.put(&key(&format!("/{i}")), make_entry(60));
        }
        assert!(cache.len() <= 3);
    }
}
