use std::time::Instant;

use dashmap::DashMap;
use tracing::trace;

use gateway_config::TokenBucketConfig;

// ── Bucket ────────────────────────────────────────────────────────────────────

/// A single token bucket for one (route, identity) pair.
///
/// ## Algorithm
///
/// On each `consume` call:
/// 1. Compute elapsed time since last refill
/// 2. Add `elapsed_secs × refill_rate` tokens (capped at `capacity`)
/// 3. If tokens ≥ 1, decrement and allow. Otherwise deny.
///
/// This is a **lazy refill** strategy — we don't need a background timer.
/// The math is equivalent to a continuous token bucket.
///
/// ## Thread safety
///
/// `Bucket` itself is not `Sync`. Thread safety comes from `DashMap`, which
/// shards its entries and gives each shard its own `RwLock`. A `consume` call
/// holds the shard lock only for the duration of the arithmetic — nanoseconds.
#[derive(Debug)]
pub struct Bucket {
    /// Current token count (fractional to avoid drift over many small windows)
    tokens: f64,
    /// Maximum tokens the bucket can hold
    capacity: f64,
    /// Tokens added per second
    refill_rate: f64,
    /// Monotonic timestamp of the last refill calculation
    last_refill: Instant,
}

impl Bucket {
    fn new(cfg: &TokenBucketConfig) -> Self {
        Self {
            // Start full — don't penalise the first burst after startup
            tokens: cfg.capacity as f64,
            capacity: cfg.capacity as f64,
            refill_rate: cfg.refill_rate as f64,
            last_refill: Instant::now(),
        }
    }

    /// Attempt to consume one token. Returns `true` if allowed.
    fn consume(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;

        // Refill
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.capacity);

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Tokens available right now (read-only, for headers).
    fn available(&self) -> u64 {
        let elapsed = self.last_refill.elapsed().as_secs_f64();
        let projected = (self.tokens + elapsed * self.refill_rate).min(self.capacity);
        projected.floor() as u64
    }
}

// ── L1LocalLimiter ────────────────────────────────────────────────────────────

/// In-process token bucket store.
///
/// One `DashMap` shard per key — no global lock on the hot path.
/// Keys are created on first use and persist for the lifetime of the limiter
/// (which is swapped out on config reload alongside the `RateLimiter`).
pub struct L1LocalLimiter {
    buckets: DashMap<String, Bucket>,
    config: TokenBucketConfig,
}

impl L1LocalLimiter {
    pub fn new(config: TokenBucketConfig) -> Self {
        Self {
            buckets: DashMap::new(),
            config,
        }
    }

    /// Check and consume one token for `key`.
    /// Returns `true` if the request is allowed.
    pub fn check(&self, key: &str) -> bool {
        let allowed = self.buckets
            .entry(key.to_string())
            .or_insert_with(|| Bucket::new(&self.config))
            .consume();

        trace!(key, allowed, "L1 bucket check");
        allowed
    }

    /// Current token count for `key` (for `X-RateLimit-*` response headers).
    pub fn remaining(&self, key: &str) -> u64 {
        self.buckets
            .get(key)
            .map(|b| b.available())
            .unwrap_or(self.config.capacity)
    }

    /// Current capacity (constant per policy).
    pub fn capacity(&self) -> u64 {
        self.config.capacity
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    fn cfg(capacity: u64, refill_rate: u64) -> TokenBucketConfig {
        TokenBucketConfig { capacity, refill_rate }
    }

    #[test]
    fn allows_up_to_capacity() {
        let limiter = L1LocalLimiter::new(cfg(5, 1));
        for _ in 0..5 {
            assert!(limiter.check("key"));
        }
    }

    #[test]
    fn denies_when_empty() {
        let limiter = L1LocalLimiter::new(cfg(2, 0));
        assert!(limiter.check("k"));
        assert!(limiter.check("k"));
        assert!(!limiter.check("k")); // bucket empty, refill_rate=0
    }

    #[test]
    fn refills_over_time() {
        let limiter = L1LocalLimiter::new(cfg(1, 100)); // 100 tokens/sec
        assert!(limiter.check("k"));
        assert!(!limiter.check("k")); // empty now
        thread::sleep(Duration::from_millis(20)); // 2 tokens should refill
        assert!(limiter.check("k"));
    }

    #[test]
    fn keys_are_independent() {
        let limiter = L1LocalLimiter::new(cfg(1, 0));
        assert!(limiter.check("a"));
        assert!(!limiter.check("a"));
        // "b" has its own full bucket
        assert!(limiter.check("b"));
    }

    #[test]
    fn remaining_starts_at_capacity() {
        let limiter = L1LocalLimiter::new(cfg(100, 10));
        assert_eq!(limiter.remaining("new-key"), 100);
    }

    #[test]
    fn remaining_decrements_after_check() {
        let limiter = L1LocalLimiter::new(cfg(10, 0));
        limiter.check("k");
        assert_eq!(limiter.remaining("k"), 9);
    }
}
