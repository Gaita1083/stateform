//! Layered concurrency control: global → per-route semaphores.
//!
//! ## Why two layers?
//!
//! **Global limit** — protects the process. With `max_body_bytes = 4 MiB` and
//! unlimited concurrency, a 50k-connection spike allocates up to 200 GB of RAM
//! before any back-pressure kicks in. The global semaphore is the last line of
//! defence.
//!
//! **Per-route limit** — bulkhead isolation. Without it, a single slow upstream
//! (e.g. a payments service doing heavy DB work) can consume all 10,000 global
//! permits, starving every other route. Per-route caps bound the blast radius.
//!
//! ## Acquisition order
//!
//! ALWAYS: global permit first, then route permit. Never reversed.
//!
//! Why this prevents deadlocks: a deadlock requires task A holding resource X
//! and waiting for Y, while task B holds Y and waits for X. With a strict
//! global → route order, both tasks always try to acquire in the same sequence,
//! so neither can be blocked waiting for the other.
//!
//! ## Implementation detail
//!
//! Uses [`tokio::sync::OwnedSemaphorePermit`] (via `acquire_owned()` on
//! `Arc<Semaphore>`) so permits have no lifetime relationship with the limiter.
//! The `ConcurrencyPermit` can be held across `.await` points inside the
//! request pipeline without borrow-checker issues.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{Response, StatusCode};
use http_body_util::Full;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::warn;

use gateway_config::GatewayConfig;

// ── ConcurrencyLimiter ────────────────────────────────────────────────────────

/// Layered concurrency gate: process-level + per-route bulkheads.
///
/// Built once at startup from [`GatewayConfig`] and shared via `Arc` across
/// all request handlers. Semaphores are never rebuilt during hot-reload because
/// their capacity is rarely changed at runtime; a restart is appropriate for
/// concurrency limit changes.
pub struct ConcurrencyLimiter {
    /// Process-wide ceiling. Protects RAM and file descriptor budget.
    global: Arc<Semaphore>,

    /// Route-specific ceilings, keyed by route ID.
    /// Routes without a configured limit do not appear here and only consume
    /// from the global pool.
    routes: HashMap<String, Arc<Semaphore>>,

    /// How long to wait for a permit before returning 503.
    /// Keeps latency predictable under overload — requests should not queue
    /// indefinitely inside the gateway.
    acquire_timeout: Duration,
}

impl ConcurrencyLimiter {
    /// Build from the global listener config and per-route route configs.
    pub fn build(cfg: &GatewayConfig) -> Self {
        let global_limit     = cfg.listener.max_concurrent_requests;
        let acquire_timeout  = Duration::from_millis(cfg.listener.concurrency_acquire_timeout_ms);

        let routes = cfg.routes.iter()
            .filter_map(|r| {
                r.max_concurrent_requests.map(|limit| {
                    (r.id.clone(), Arc::new(Semaphore::new(limit)))
                })
            })
            .collect();

        Self {
            global: Arc::new(Semaphore::new(global_limit)),
            routes,
            acquire_timeout,
        }
    }

    /// Acquire concurrency permits for one request.
    ///
    /// Acquisition order: **global → route** (always, to prevent deadlocks).
    ///
    /// Returns:
    /// - `Ok(ConcurrencyPermit)` — both permits held; released when the permit drops.
    /// - `Err(Response)` — 503 response to return immediately; limit exceeded.
    pub async fn acquire(
        &self,
        route_id: &str,
    ) -> Result<ConcurrencyPermit, Response<Full<Bytes>>> {
        // ── Step 1: Global permit ─────────────────────────────────────────────
        let global = match tokio::time::timeout(
            self.acquire_timeout,
            Arc::clone(&self.global).acquire_owned(),
        ).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_))     => {
                // Semaphore closed — only happens during shutdown
                warn!(route_id, "global semaphore closed during shutdown");
                return Err(service_unavailable("gateway shutting down"));
            }
            Err(_timeout) => {
                warn!(
                    route_id,
                    limit  = self.global.available_permits(),
                    "global concurrency limit reached — returning 503"
                );
                return Err(service_unavailable("server concurrency limit exceeded"));
            }
        };

        // ── Step 2: Per-route permit (only if a limit is configured) ──────────
        let route = if let Some(sem) = self.routes.get(route_id) {
            match tokio::time::timeout(
                self.acquire_timeout,
                Arc::clone(sem).acquire_owned(),
            ).await {
                Ok(Ok(permit)) => Some(permit),
                Ok(Err(_))     => {
                    warn!(route_id, "route semaphore closed during shutdown");
                    return Err(service_unavailable("gateway shutting down"));
                }
                Err(_timeout) => {
                    warn!(
                        route_id,
                        limit = sem.available_permits(),
                        "route concurrency limit reached — returning 503"
                    );
                    return Err(service_unavailable("route concurrency limit exceeded"));
                }
            }
        } else {
            None
        };

        Ok(ConcurrencyPermit { _global: global, _route: route })
    }
}

// ── ConcurrencyPermit ─────────────────────────────────────────────────────────

/// RAII guard holding global and optional route permits.
///
/// Both permits are released (returned to their semaphores) when this drops —
/// either at the end of the request or on early return.
// OwnedSemaphorePermit doesn't implement Debug, so we provide a minimal one.
pub struct ConcurrencyPermit {
    _global: OwnedSemaphorePermit,
    _route:  Option<OwnedSemaphorePermit>,
}

impl std::fmt::Debug for ConcurrencyPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConcurrencyPermit").finish_non_exhaustive()
    }
}

// ── Response builders ─────────────────────────────────────────────────────────

fn service_unavailable(msg: &str) -> Response<Full<Bytes>> {
    let body = serde_json::json!({ "error": msg }).to_string();
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(http::header::CONTENT_TYPE, "application/json")
        // Retry-After: 1 signals the client to back off for at least 1 second.
        // Combined with client-side jitter this breaks synchronised retry storms.
        .header("Retry-After", "1")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;
    use gateway_config::*;

    fn cfg_with_limits(global: usize, route_limit: Option<usize>) -> GatewayConfig {
        GatewayConfig {
            listener: ListenerConfig {
                bind: "0.0.0.0:8080".into(),
                tls: None,
                max_body_bytes: 4 * 1024 * 1024,
                keepalive: Duration::from_secs(75),
                max_concurrent_requests: global,
                concurrency_acquire_timeout_ms: 100, // short timeout for tests
            },
            routes: vec![RouteConfig {
                id: "test-route".into(),
                match_: RouteMatch {
                    path: PathMatch::Prefix { value: "/".into() },
                    method: None,
                    headers: HashMap::new(),
                },
                upstream: "backend".into(),
                load_balancing: None,
                middleware: MiddlewareConfig::default(),
                headers: HeaderMutations::default(),
                max_concurrent_requests: route_limit,
            }],
            upstreams: {
                let mut m = HashMap::new();
                m.insert("backend".into(), UpstreamConfig {
                    endpoints: vec![EndpointConfig {
                        address: "127.0.0.1:9000".into(),
                        weight: 1,
                        active: true,
                    }],
                    load_balancing: LoadBalancingConfig::WeightedRoundRobin,
                    health_check: None,
                    timeouts: TimeoutConfig {
                        connect: Duration::from_secs(2),
                        request: Duration::from_secs(30),
                        idle:    Duration::from_secs(90),
                    },
                    retry: None,
                });
                m
            },
            rate_limiting: RateLimitConfig {
                redis_url: "redis://localhost".into(),
                policies: HashMap::new(),
            },
            cache: CacheConfig {
                redis_url: "redis://localhost".into(),
                l1_capacity: 10_000,
            },
            auth: AuthConfig {
                jwt: JwtConfig {
                    jwks_url: "https://example.com/.well-known/jwks.json".into(),
                    audience: vec!["stateform".into()],
                    issuers: vec!["https://example.com".into()],
                    jwks_cache_ttl: Duration::from_secs(300),
                },
                api_keys: ApiKeyConfig {
                    redis_key_prefix: "stateform:apikey:".into(),
                    redis_url: "redis://localhost".into(),
                },
                mtls: MtlsConfig {
                    client_ca_path: "/certs/ca.pem".into(),
                    required: true,
                },
            },
            observability: ObservabilityConfig {
                metrics_bind: "0.0.0.0:9090".into(),
            },
            control_plane: ControlPlaneConfig {
                grpc_address: "http://localhost:50051".into(),
                reconnect_interval: Duration::from_secs(5),
            },
        }
    }

    #[tokio::test]
    async fn permit_acquired_and_released() {
        let limiter = ConcurrencyLimiter::build(&cfg_with_limits(10, None));
        let permit = limiter.acquire("test-route").await;
        assert!(permit.is_ok(), "should acquire permit within global limit");
        drop(permit); // permit released here
        // After release the semaphore should be back to 10
        assert_eq!(limiter.global.available_permits(), 10);
    }

    #[tokio::test]
    async fn global_limit_returns_503() {
        // Limit of 1: hold the one permit, then try to acquire another
        let limiter = ConcurrencyLimiter::build(&cfg_with_limits(1, None));
        let _held = Arc::clone(&limiter.global).acquire_owned().await.unwrap();

        let result = limiter.acquire("test-route").await;
        assert!(result.is_err(), "should return 503 when global limit exhausted");

        let resp = result.unwrap_err();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn route_limit_returns_503_while_global_available() {
        // Global = 10, route = 1: hold the route permit, try again
        let limiter = ConcurrencyLimiter::build(&cfg_with_limits(10, Some(1)));
        let _held = Arc::clone(
            limiter.routes.get("test-route").unwrap()
        ).acquire_owned().await.unwrap();

        let result = limiter.acquire("test-route").await;
        assert!(result.is_err(), "route limit should fire before global");
        let resp = result.unwrap_err();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        // Global permits should still be available
        assert_eq!(limiter.global.available_permits(), 10);
    }

    #[tokio::test]
    async fn no_route_limit_uses_only_global() {
        let limiter = ConcurrencyLimiter::build(&cfg_with_limits(10, None));
        // Route "other" has no per-route semaphore
        let permit = limiter.acquire("other").await;
        assert!(permit.is_ok());
        assert_eq!(limiter.global.available_permits(), 9);
    }
}
