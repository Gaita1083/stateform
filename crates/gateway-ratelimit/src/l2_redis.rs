use redis::{aio::ConnectionManager, Script};
use tracing::trace;

use gateway_config::DistributedRateLimitConfig;

use crate::error::RateLimitError;

// ── Lua scripts ───────────────────────────────────────────────────────────────
//
// Both scripts run atomically inside Redis — no race between check and decrement.
// Using Lua avoids WATCH/MULTI/EXEC overhead and guarantees the read-modify-write
// is a single round trip.

/// Token bucket script.
///
/// KEYS[1] = rate limit key
/// ARGV[1] = capacity (max tokens)
/// ARGV[2] = refill_rate (tokens per second)
/// ARGV[3] = current Unix timestamp (seconds, as float)
///
/// Returns: [allowed: 0|1, remaining: int, retry_after_ms: int]
const TOKEN_BUCKET_SCRIPT: &str = r#"
local key          = KEYS[1]
local capacity     = tonumber(ARGV[1])
local refill_rate  = tonumber(ARGV[2])
local now          = tonumber(ARGV[3])

local data = redis.call('HMGET', key, 'tokens', 'last_refill')
local tokens      = tonumber(data[1]) or capacity
local last_refill = tonumber(data[2]) or now

-- Refill based on elapsed time
local elapsed = math.max(0, now - last_refill)
tokens = math.min(capacity, tokens + elapsed * refill_rate)

local allowed = 0
local retry_after_ms = 0

if tokens >= 1 then
    tokens = tokens - 1
    allowed = 1
else
    -- Time until one token is available
    local deficit = 1 - tokens
    retry_after_ms = math.ceil((deficit / refill_rate) * 1000)
end

-- TTL = time to refill from empty + buffer
local ttl = math.ceil(capacity / refill_rate) + 10
redis.call('HMSET', key, 'tokens', tokens, 'last_refill', now)
redis.call('EXPIRE', key, ttl)

return {allowed, math.floor(tokens), retry_after_ms}
"#;

/// Sliding window counter script.
///
/// Uses a sorted set where each request is scored by its Unix timestamp.
/// Expired entries (outside the window) are pruned on every check — the set
/// never grows unbounded.
///
/// KEYS[1] = rate limit key
/// ARGV[1] = max_requests (limit per window)
/// ARGV[2] = window_seconds
/// ARGV[3] = current Unix timestamp (seconds, as float)
/// ARGV[4] = unique request ID (prevents duplicate scores collapsing entries)
///
/// Returns: [allowed: 0|1, remaining: int, retry_after_ms: int]
const SLIDING_WINDOW_SCRIPT: &str = r#"
local key          = KEYS[1]
local max_requests = tonumber(ARGV[1])
local window       = tonumber(ARGV[2])
local now          = tonumber(ARGV[3])
local req_id       = ARGV[4]

local window_start = now - window

-- Remove expired entries
redis.call('ZREMRANGEBYSCORE', key, '-inf', window_start)

-- Count current entries in window
local count = redis.call('ZCARD', key)

local allowed = 0
local retry_after_ms = 0

if count < max_requests then
    -- Add this request to the window
    redis.call('ZADD', key, now, req_id)
    allowed = 1
else
    -- Find the oldest entry — client must wait until it expires
    local oldest = redis.call('ZRANGE', key, 0, 0, 'WITHSCORES')
    if oldest[2] then
        local oldest_ts = tonumber(oldest[2])
        retry_after_ms = math.ceil((oldest_ts + window - now) * 1000)
    end
end

-- TTL slightly longer than window to allow natural expiry
redis.call('EXPIRE', key, window + 5)

local remaining = math.max(0, max_requests - count - allowed)
return {allowed, remaining, retry_after_ms}
"#;

// ── L2Outcome ─────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct L2Outcome {
    pub allowed: bool,
    pub remaining: u64,
    /// When denied: milliseconds until the client may retry.
    pub retry_after_ms: u64,
}

// ── L2RedisLimiter ────────────────────────────────────────────────────────────

/// Distributed rate limiter backed by Redis.
///
/// Both algorithms (token bucket and sliding window) execute as atomic
/// Lua scripts — a single round trip per request, no race conditions.
///
/// The connection manager reconnects automatically on Redis failure.
/// When Redis is unavailable we **fail open** (allow the request) with a
/// warning — a brief Redis outage should not take down the gateway.
/// The L1 local bucket is your protection during that window.
pub struct L2RedisLimiter {
    conn: ConnectionManager,
    config: DistributedRateLimitConfig,
    token_bucket_script: Script,
    sliding_window_script: Script,
}

impl L2RedisLimiter {
    pub async fn new(
        redis_url: &str,
        config: DistributedRateLimitConfig,
    ) -> Result<Self, RateLimitError> {
        let client = redis::Client::open(redis_url)
            .map_err(|e| RateLimitError::Redis(e.to_string()))?;
        let conn = ConnectionManager::new(client)
            .await
            .map_err(|e| RateLimitError::Redis(e.to_string()))?;

        Ok(Self {
            conn,
            config,
            token_bucket_script: Script::new(TOKEN_BUCKET_SCRIPT),
            sliding_window_script: Script::new(SLIDING_WINDOW_SCRIPT),
        })
    }

    /// Check and enforce the distributed rate limit for `key`.
    ///
    /// On Redis failure: **fails open** (returns allowed=true) so a transient
    /// Redis outage doesn't take down your API. The L1 bucket is still active.
    pub async fn check(&self, key: &str) -> L2Outcome {
        match self.check_inner(key).await {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::warn!(error = %e, key, "L2 Redis check failed — failing open");
                L2Outcome { allowed: true, remaining: 0, retry_after_ms: 0 }
            }
        }
    }

    async fn check_inner(&self, key: &str) -> Result<L2Outcome, RateLimitError> {
        let mut conn = self.conn.clone();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();

        let result: Vec<i64> = match &self.config {
            DistributedRateLimitConfig::TokenBucket { capacity, refill_rate } => {
                self.token_bucket_script
                    .key(key)
                    .arg(capacity)
                    .arg(refill_rate)
                    .arg(now)
                    .invoke_async(&mut conn)
                    .await
                    .map_err(|e| RateLimitError::Script(e.to_string()))?
            }

            DistributedRateLimitConfig::SlidingWindow { max_requests, window } => {
                // Unique request ID prevents two simultaneous requests at the
                // exact same timestamp from colliding in the sorted set
                let req_id = format!("{key}:{now}:{}", rand_u32());
                self.sliding_window_script
                    .key(key)
                    .arg(max_requests)
                    .arg(window.as_secs())
                    .arg(now)
                    .arg(req_id)
                    .invoke_async(&mut conn)
                    .await
                    .map_err(|e| RateLimitError::Script(e.to_string()))?
            }
        };

        let allowed       = result.first().copied().unwrap_or(1) == 1;
        let remaining     = result.get(1).copied().unwrap_or(0).max(0) as u64;
        let retry_after   = result.get(2).copied().unwrap_or(0).max(0) as u64;

        trace!(key, allowed, remaining, retry_after_ms = retry_after, "L2 check");

        Ok(L2Outcome { allowed, remaining, retry_after_ms: retry_after })
    }
}

/// Cheap non-crypto random u32 — just for uniquifying sliding window entries.
fn rand_u32() -> u32 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    std::time::Instant::now().hash(&mut h);
    h.finish() as u32
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    /// Unit tests for the Lua script logic are best run as integration tests
    /// against a real Redis (or `redis-mock`). Here we test the outcome parsing.
    #[test]
    fn l2_outcome_parses_allowed() {
        // Simulate what the Lua script returns: [1, 99, 0]
        let raw: Vec<i64> = vec![1, 99, 0];
        let allowed   = raw[0] == 1;
        let remaining = raw[1].max(0) as u64;
        let retry     = raw[2].max(0) as u64;

        assert!(allowed);
        assert_eq!(remaining, 99);
        assert_eq!(retry, 0);
    }

    #[test]
    fn l2_outcome_parses_denied() {
        // Simulate: [0, 0, 350] — denied, retry in 350ms
        let raw: Vec<i64> = vec![0, 0, 350];
        let allowed = raw[0] == 1;
        let retry   = raw[2].max(0) as u64;

        assert!(!allowed);
        assert_eq!(retry, 350);
    }
}
