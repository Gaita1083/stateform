use redis::{aio::ConnectionManager, AsyncCommands};
use tracing::{trace, warn};

use crate::{entry::CachedResponse, error::CacheError, key::CacheKey};

/// Redis-backed L2 response cache.
///
/// Entries are stored as JSON strings with a Redis `EXPIRE` TTL matching the
/// effective cache TTL. Redis handles expiry authoritatively — no background
/// task needed on our side.
///
/// ## Key namespace
///
/// All keys are already namespaced by [`CacheKey`] (`stateform:cache:…`), so
/// no additional prefix is applied here.
///
/// ## Failure posture
///
/// On any Redis error we **fail open**:
/// - A `get` failure returns `Ok(None)` — treat as a cache miss, serve fresh.
/// - A `put` failure logs a warning and is silently swallowed — the request
///   already got a response, caching is best-effort.
///
/// This means a Redis outage degrades to an all-miss cache rather than a
/// gateway outage.
pub struct L2Cache {
    conn: ConnectionManager,
}

impl L2Cache {
    pub async fn new(redis_url: &str) -> Result<Self, CacheError> {
        let client = redis::Client::open(redis_url)
            .map_err(|e| CacheError::Redis(e.to_string()))?;
        let conn = ConnectionManager::new(client)
            .await
            .map_err(|e| CacheError::Redis(e.to_string()))?;
        Ok(Self { conn })
    }

    /// Retrieve a cached entry from Redis. Returns `None` on miss or error.
    pub async fn get(&self, key: &CacheKey) -> Option<CachedResponse> {
        let mut conn = self.conn.clone();
        let raw: Option<String> = conn.get(key.as_str()).await
            .map_err(|e| warn!(error = %e, key = %key, "L2 get failed"))
            .ok()
            .flatten();

        let json = raw?;

        match serde_json::from_str::<CachedResponse>(&json) {
            Ok(entry) => {
                trace!(key = %key, "L2 hit");
                Some(entry)
            }
            Err(e) => {
                warn!(error = %e, key = %key, "L2 deserialise failed — treating as miss");
                None
            }
        }
    }

    /// Store a response in Redis with a TTL.
    ///
    /// `ttl_secs` of 0 is a no-op — we never store entries that expire
    /// immediately.
    pub async fn put(&self, key: &CacheKey, response: &CachedResponse, ttl_secs: u64) {
        if ttl_secs == 0 {
            return;
        }

        let json = match serde_json::to_string(response) {
            Ok(j) => j,
            Err(e) => {
                warn!(error = %e, key = %key, "L2 serialise failed — skipping cache store");
                return;
            }
        };

        let mut conn = self.conn.clone();
        let _: Result<(), _> = redis::cmd("SETEX")
            .arg(key.as_str())
            .arg(ttl_secs)
            .arg(&json)
            .query_async(&mut conn)
            .await
            .map_err(|e| warn!(error = %e, key = %key, "L2 put failed — skipping"));

        trace!(key = %key, ttl_secs, "L2 stored");
    }

    /// Explicitly invalidate a key (e.g. after a write-through mutation).
    pub async fn invalidate(&self, key: &CacheKey) {
        let mut conn = self.conn.clone();
        let _: Result<(), _> = conn
            .del(key.as_str())
            .await
            .map_err(|e| warn!(error = %e, key = %key, "L2 invalidate failed"));
    }
}
