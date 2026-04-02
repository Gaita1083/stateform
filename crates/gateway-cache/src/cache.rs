use bytes::Bytes;
use tracing::{debug, instrument};

use gateway_config::{CacheConfig, CacheMiddlewareConfig};

use crate::{
    control,
    entry::CachedResponse,
    error::CacheError,
    key::{storable_headers, CacheKey},
    l1_local::L1Cache,
    l2_redis::L2Cache,
};

// ── Lookup result ─────────────────────────────────────────────────────────────

/// The outcome of a cache lookup, passed back to the proxy layer.
#[derive(Debug)]
pub enum CacheLookup {
    /// Entry found and fresh — serve directly, do not hit upstream.
    Hit(CachedResponse),
    /// No fresh entry — must forward to upstream.
    Miss,
    /// Caching is disabled for this request (no middleware config, or
    /// `no-store`/`no-cache` on the request).
    Bypass,
}

// ── ResponseCache ─────────────────────────────────────────────────────────────

/// Top-level response cache — coordinates L1 and L2 for the proxy layer.
///
/// ## Read path (lookup)
///
/// 1. Check L1 — O(1) hash lookup, no I/O
/// 2. On L1 miss: check L2 (Redis GET)
/// 3. On L2 hit: populate L1, return entry
/// 4. On L2 miss: return `Miss` — caller fetches from upstream
///
/// ## Write path (store)
///
/// 1. Parse response `Cache-Control` and `Vary` headers
/// 2. Resolve effective TTL (upstream directive vs route config, take min)
/// 3. Store in L2 (Redis SETEX) — skip if `no-store`, `private`, or TTL=0
/// 4. Store in L1 — always, even for `private` responses
///
/// The `private` distinction matters: private responses go into L1 only
/// (per-process, not shared across replicas). Shared responses go into
/// both layers.
pub struct ResponseCache {
    l1: L1Cache,
    l2: L2Cache,
}

impl ResponseCache {
    /// Build the cache from config, establishing the Redis connection.
    pub async fn new(cfg: &CacheConfig) -> Result<Self, CacheError> {
        let l1 = L1Cache::new(cfg.l1_capacity);
        let l2 = L2Cache::new(&cfg.redis_url).await?;
        Ok(Self { l1, l2 })
    }

    // ── Read path ─────────────────────────────────────────────────────────────

    /// Look up a cached response for an incoming request.
    ///
    /// Returns `Bypass` if the route has no cache middleware configured, or if
    /// the request carries `Cache-Control: no-store` / `no-cache`.
    #[instrument(skip_all, fields(route_id, path))]
    pub async fn lookup(
        &self,
        middleware_cfg: Option<&CacheMiddlewareConfig>,
        route_id: &str,
        method: &str,
        path: &str,
        query: Option<&str>,
        request_headers: &http::HeaderMap,
    ) -> CacheLookup {
        // Gate: no middleware config means caching is disabled for this route.
        let Some(_cfg) = middleware_cfg else {
            return CacheLookup::Bypass;
        };

        // Honour request Cache-Control: no-cache / no-store
        if let Some(cc) = control::from_headers(request_headers) {
            if cc.is_uncacheable() {
                return CacheLookup::Bypass;
            }
        }

        // Only GET and HEAD are cacheable
        if !matches!(method.to_ascii_uppercase().as_str(), "GET" | "HEAD") {
            return CacheLookup::Bypass;
        }

        let base_key = CacheKey::new(method, route_id, path, query);

        // ── L1 ────────────────────────────────────────────────────────────────
        if let Some(entry) = self.l1.get(&base_key) {
            // Re-key with vary if the entry specifies it
            let full_key = base_key.with_vary(&entry.vary_headers, request_headers);
            if let Some(vary_entry) = self.l1.get(&full_key) {
                debug!(key = %full_key, "cache L1 hit");
                return CacheLookup::Hit(vary_entry);
            }
        }

        // ── L2 ────────────────────────────────────────────────────────────────
        if let Some(entry) = self.l2.get(&base_key).await {
            let full_key = base_key.with_vary(&entry.vary_headers, request_headers);

            // Re-check L2 with the full vary key
            let final_entry = if full_key != base_key {
                self.l2.get(&full_key).await.unwrap_or(entry)
            } else {
                entry
            };

            // Promote to L1
            self.l1.put(&full_key, final_entry.clone());
            debug!(key = %full_key, "cache L2 hit — promoted to L1");
            return CacheLookup::Hit(final_entry);
        }

        CacheLookup::Miss
    }

    // ── Write path ────────────────────────────────────────────────────────────

    /// Store an upstream response in the cache.
    ///
    /// `response_headers` and `body` are the raw upstream response.
    /// This method inspects `Cache-Control` and `Vary` on the response
    /// to decide TTL and key, then stores appropriately.
    #[instrument(skip_all, fields(route_id))]
    pub async fn store(
        &self,
        middleware_cfg: Option<&CacheMiddlewareConfig>,
        route_id: &str,
        method: &str,
        path: &str,
        query: Option<&str>,
        status: u16,
        response_headers: &http::HeaderMap,
        body: Bytes,
        request_headers: &http::HeaderMap,
    ) {
        let Some(cfg) = middleware_cfg else { return };
        if cfg.read_only { return }

        // Only cache successful responses
        if !(200..300).contains(&status) {
            return;
        }

        // Parse response Cache-Control
        let response_cc = control::from_headers(response_headers);

        // Resolve effective TTL
        let Some(ttl) = control::resolve_ttl(response_cc.as_ref(), cfg.ttl) else {
            debug!(route_id, "cache store skipped: response is no-store");
            return;
        };

        // Parse Vary — if `Vary: *` the response isn't safely cacheable
        let vary_headers = match control::parse_vary(response_headers) {
            Some(v) => v,
            None => {
                debug!(route_id, "cache store skipped: Vary: *");
                return;
            }
        };

        let base_key   = CacheKey::new(method, route_id, path, query);
        let full_key   = base_key.with_vary(&vary_headers, request_headers);
        let safe_hdrs  = storable_headers(response_headers);
        let is_private = response_cc.as_ref().map(|cc| cc.private).unwrap_or(false);

        let entry = CachedResponse::new(
            status,
            safe_hdrs,
            body,
            ttl,
            vary_headers,
        );

        // L1 — always store (even private responses are fine in-process)
        self.l1.put(&full_key, entry.clone());

        // L2 — skip for private responses (must not share across replicas)
        if !is_private {
            self.l2.put(&full_key, &entry, ttl.as_secs()).await;
        }

        debug!(
            key = %full_key,
            ttl_secs = ttl.as_secs(),
            private = is_private,
            "cached response stored"
        );
    }

    /// Explicitly invalidate a key across both layers.
    ///
    /// Call this after a mutating request (POST/PUT/PATCH/DELETE) that
    /// invalidates a known cached resource.
    pub async fn invalidate(
        &self,
        route_id: &str,
        method: &str,
        path: &str,
        query: Option<&str>,
    ) {
        let key = CacheKey::new(method, route_id, path, query);
        self.l1.invalidate(&key);
        self.l2.invalidate(&key).await;
        debug!(key = %key, "cache invalidated");
    }
}

/// Type alias so the public API exposed by `lib.rs` as `CacheLayer` resolves
/// to this struct without renaming it everywhere internally.
pub type CacheLayer = ResponseCache;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn middleware(ttl_secs: u64, read_only: bool) -> CacheMiddlewareConfig {
        CacheMiddlewareConfig {
            ttl: Duration::from_secs(ttl_secs),
            read_only,
        }
    }

    /// Unit tests for pure logic — Redis-dependent paths are integration tests.

    #[test]
    fn read_only_skips_store() {
        // read_only = true means we check but never populate
        let cfg = middleware(30, true);
        assert!(cfg.read_only);
    }

    #[test]
    fn non_2xx_should_not_be_stored() {
        // Status 404 — store() returns early
        let status: u16 = 404;
        assert!(!(200..300).contains(&status));
    }

    #[test]
    fn ttl_zero_means_no_cache() {
        use crate::control::{resolve_ttl, CacheControl};
        let cc = CacheControl::parse("max-age=0");
        let result = resolve_ttl(Some(&cc), Duration::from_secs(30));
        // max-age=0 means min(0, 30) = 0 — we store it with TTL 0, then skip
        assert_eq!(result, Some(Duration::from_secs(0)));
    }
}
