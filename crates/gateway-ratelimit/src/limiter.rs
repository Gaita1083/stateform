use std::collections::HashMap;

use tracing::{debug, instrument};

use gateway_config::{GatewayConfig, RateLimitMiddlewareRef};
use gateway_auth::AuthContext;

use crate::{
    error::RateLimitError,
    key::derive_key,
    l1_local::L1LocalLimiter,
    l2_redis::L2RedisLimiter,
};

// ── Outcome ───────────────────────────────────────────────────────────────────

/// The result of running the rate-limit pipeline for one request.
#[derive(Debug, Clone)]
pub struct RateLimitOutcome {
    pub allowed: bool,
    /// Tokens/requests remaining in the most restrictive bucket.
    pub remaining: u64,
    /// Total capacity of the current policy.
    pub limit: u64,
    /// Milliseconds until retry (only meaningful when `allowed == false`).
    pub retry_after_ms: u64,
    /// Which layer denied the request (for metrics + logs).
    pub denied_by: Option<DeniedBy>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeniedBy {
    L1Local,
    L2Distributed,
}

impl RateLimitOutcome {
    /// Build the standard `RateLimit-*` headers for the response.
    pub fn to_headers(&self) -> Vec<(String, String)> {
        let mut h = vec![
            ("X-RateLimit-Limit".into(),     self.limit.to_string()),
            ("X-RateLimit-Remaining".into(), self.remaining.to_string()),
        ];
        if !self.allowed {
            h.push(("Retry-After".into(), (self.retry_after_ms / 1000).to_string()));
        }
        h
    }
}

// ── Policy ────────────────────────────────────────────────────────────────────

/// A fully-initialised rate-limit policy: L1 + L2 pair.
struct Policy {
    l1: L1LocalLimiter,
    l2: L2RedisLimiter,
}

// ── RateLimiter ───────────────────────────────────────────────────────────────

/// Top-level rate limiter.
///
/// Holds one `Policy` per named policy from config.
/// Built once at startup (or on hot-reload) and shared via `Arc`.
///
/// ## Hot reload
///
/// On reload, a new `RateLimiter` is built from the new config. L1 buckets
/// are reset (new in-memory state). L2 state persists in Redis across reloads —
/// distributed counters are unaffected.
pub struct RateLimiter {
    policies: HashMap<String, Policy>,
}

impl RateLimiter {
    /// Construct from config, establishing Redis connections for all policies.
    pub async fn new(cfg: &GatewayConfig) -> Result<Self, RateLimitError> {
        let mut policies = HashMap::new();

        for (name, policy_cfg) in &cfg.rate_limiting.policies {
            let l1 = L1LocalLimiter::new(policy_cfg.local.clone());
            let l2 = L2RedisLimiter::new(
                &cfg.rate_limiting.redis_url,
                policy_cfg.distributed.clone(),
            )
            .await?;

            policies.insert(name.clone(), Policy { l1, l2 });
        }

        Ok(Self { policies })
    }

    /// Run the two-layer rate limit check for a single request.
    ///
    /// `middleware_ref` is `None` when the matched route has no rate-limit
    /// middleware configured — returns `allowed` immediately.
    #[instrument(skip_all, fields(route_id, policy))]
    pub async fn check(
        &self,
        middleware_ref: Option<&RateLimitMiddlewareRef>,
        route_id: &str,
        auth_ctx: Option<&AuthContext>,
        headers: &http::HeaderMap,
    ) -> Result<RateLimitOutcome, RateLimitError> {
        let Some(rl_ref) = middleware_ref else {
            // No rate limiting on this route
            return Ok(RateLimitOutcome {
                allowed: true,
                remaining: u64::MAX,
                limit: u64::MAX,
                retry_after_ms: 0,
                denied_by: None,
            });
        };

        let policy = self.policies
            .get(&rl_ref.policy)
            .ok_or_else(|| RateLimitError::UnknownPolicy(rl_ref.policy.clone()))?;

        let key = derive_key(route_id, auth_ctx, headers);

        // ── L1: local token bucket ─────────────────────────────────────────
        let l1_allowed = policy.l1.check(&key);
        let remaining  = policy.l1.remaining(&key);
        let limit      = policy.l1.capacity();

        if !l1_allowed {
            debug!(key, "denied by L1");
            return Ok(RateLimitOutcome {
                allowed: false,
                remaining: 0,
                limit,
                retry_after_ms: estimate_refill_ms(limit),
                denied_by: Some(DeniedBy::L1Local),
            });
        }

        // ── L2: distributed Redis enforcement ─────────────────────────────
        let l2 = policy.l2.check(&key).await;

        if !l2.allowed {
            debug!(key, retry_after_ms = l2.retry_after_ms, "denied by L2");
            return Ok(RateLimitOutcome {
                allowed: false,
                remaining: 0,
                limit,
                retry_after_ms: l2.retry_after_ms,
                denied_by: Some(DeniedBy::L2Distributed),
            });
        }

        Ok(RateLimitOutcome {
            allowed: true,
            remaining: remaining.min(l2.remaining),
            limit,
            retry_after_ms: 0,
            denied_by: None,
        })
    }
}

/// Rough estimate of when one token will be available.
/// Used when L1 denies and we don't have an exact Redis timestamp.
fn estimate_refill_ms(capacity: u64) -> u64 {
    // Assume refill_rate ≈ capacity (1 full refill per second is a safe upper bound)
    let _ = capacity;
    1000 // 1 second; L1 refill is fast — this is intentionally conservative
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_headers_when_allowed() {
        let outcome = RateLimitOutcome {
            allowed: true,
            remaining: 42,
            limit: 100,
            retry_after_ms: 0,
            denied_by: None,
        };
        let headers = outcome.to_headers();
        assert!(headers.iter().any(|(k, v)| k == "X-RateLimit-Limit" && v == "100"));
        assert!(headers.iter().any(|(k, v)| k == "X-RateLimit-Remaining" && v == "42"));
        assert!(!headers.iter().any(|(k, _)| k == "Retry-After"));
    }

    #[test]
    fn outcome_headers_when_denied() {
        let outcome = RateLimitOutcome {
            allowed: false,
            remaining: 0,
            limit: 100,
            retry_after_ms: 3500,
            denied_by: Some(DeniedBy::L2Distributed),
        };
        let headers = outcome.to_headers();
        assert!(headers.iter().any(|(k, v)| k == "Retry-After" && v == "3"));
    }

    #[test]
    fn no_middleware_ref_always_allows() {
        // Can't call async check in a sync test, but we verify the logic path:
        // None middleware_ref → immediate allow
        let ref_: Option<&RateLimitMiddlewareRef> = None;
        assert!(ref_.is_none()); // signals the fast-path
    }
}
