use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use tracing::{debug, warn};

use gateway_config::{
    GatewayConfig, HealthCheckConfig, LoadBalancingConfig, RetryConfig, UpstreamConfig,
};

use crate::error::CoreError;

// ── Health state ──────────────────────────────────────────────────────────────

/// Three-state endpoint health for production-grade routing decisions.
///
/// Stored as [`AtomicU8`] on [`EndpointState`] so health reads on the request
/// hot path are a single atomic load — no mutex, no contention.
///
/// ## State semantics
///
/// | State       | Routing behaviour                           |
/// |-------------|---------------------------------------------|
/// | `Healthy`   | Full configured weight — normal traffic     |
/// | `Degraded`  | Half weight — takes traffic, signals issues |
/// | `Unhealthy` | Excluded from selection — zero traffic      |
///
/// ## Transitions
///
/// ```text
/// Healthy ──[1st failure]──▶ Degraded ──[unhealthy_threshold]──▶ Unhealthy
///                                              │
/// Healthy ◀──[healthy_threshold successes]────-+
///   ▲                                          │
///   └───────── Degraded ◀──[1st success]───────┘
/// ```
///
/// ## Flapping mitigation
///
/// A `FLAP_COOLDOWN_MS` (10 s) window prevents rapid state oscillation.
/// State transitions require BOTH the threshold counter AND cooldown elapsed.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Unhealthy = 0,
    Healthy   = 1,
    Degraded  = 2,
}

impl Health {
    /// Convert raw `AtomicU8` storage back to the typed enum.
    /// Unknown values (e.g. from a race during a torn write) default to `Unhealthy`.
    #[inline]
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Healthy,
            2 => Self::Degraded,
            _ => Self::Unhealthy,
        }
    }

    /// Prometheus gauge value for `stateform_endpoint_health`.
    ///
    /// - `1.0` = Healthy
    /// - `0.5` = Degraded (partial failure — operator should investigate)
    /// - `0.0` = Unhealthy (no traffic, alert should fire)
    pub fn as_gauge_value(self) -> f64 {
        match self {
            Self::Healthy   => 1.0,
            Self::Degraded  => 0.5,
            Self::Unhealthy => 0.0,
        }
    }
}

/// Minimum milliseconds between health state transitions (anti-flapping).
///
/// An endpoint oscillating between success/failure must stay in each state for
/// at least this long before the next transition is allowed.
const FLAP_COOLDOWN_MS: u64 = 10_000; // 10 seconds

// ── EndpointState ─────────────────────────────────────────────────────────────

/// Runtime state tracked per upstream endpoint.
pub struct EndpointState {
    pub address: String,
    pub weight:  u32,

    /// Current health state stored atomically.
    ///
    /// Hot-path reads use `Ordering::Relaxed` — a slightly stale read is
    /// acceptable; threshold + cooldown logic absorbs transient noise.
    /// Writes use `Ordering::Release` so the health check task's transition
    /// is promptly visible to all routing threads.
    pub health: AtomicU8,

    /// Consecutive health check failures since last success (or startup).
    pub consecutive_failures: AtomicU32,

    /// Consecutive health check successes since last failure.
    pub consecutive_successes: AtomicU32,

    /// Unix timestamp (ms) of last state transition — for flapping cooldown.
    pub last_state_change_ms: AtomicU64,

    /// Active in-flight request count for Least Connections scoring.
    pub active_conns: AtomicU64,

    /// EWMA latency estimate in milliseconds (updated after each upstream call).
    pub ewma_latency: AtomicU64,
}

impl EndpointState {
    pub fn new(address: String, weight: u32) -> Self {
        Self {
            address,
            weight,
            health:                AtomicU8::new(Health::Healthy as u8),
            consecutive_failures:  AtomicU32::new(0),
            consecutive_successes: AtomicU32::new(0),
            last_state_change_ms:  AtomicU64::new(0),
            active_conns:          AtomicU64::new(0),
            ewma_latency:          AtomicU64::new(0),
        }
    }

    /// Read the current health state as the typed enum.
    #[inline]
    pub fn health_state(&self) -> Health {
        Health::from_u8(self.health.load(Ordering::Relaxed))
    }

    /// Returns `true` if this endpoint should receive traffic.
    /// `Degraded` endpoints are available (at reduced weight); `Unhealthy` are not.
    #[inline]
    pub fn is_available(&self) -> bool {
        !matches!(self.health_state(), Health::Unhealthy)
    }

    /// Process one health check **failure**. Applies threshold + cooldown logic.
    /// Returns the new health state.
    ///
    /// `AcqRel` on counter increments so concurrent health-check tasks
    /// on different threads each see the correct count.
    /// `Release` on the health store makes the transition visible to routing.
    pub fn mark_failure(&self, cfg: &HealthCheckConfig) -> Health {
        let failures = self.consecutive_failures.fetch_add(1, Ordering::AcqRel) + 1;
        self.consecutive_successes.store(0, Ordering::Relaxed);

        let current     = self.health_state();
        let now         = now_ms();
        let last        = self.last_state_change_ms.load(Ordering::Acquire);
        let in_cooldown = now.saturating_sub(last) < FLAP_COOLDOWN_MS;

        if in_cooldown {
            return current; // Cooldown window active — no state transition
        }

        let new_state = if failures >= cfg.unhealthy_threshold {
            Health::Unhealthy
        } else if current == Health::Healthy {
            // Step down to Degraded first — keeps partial capacity in rotation
            // while the team investigates, instead of immediately removing the endpoint.
            Health::Degraded
        } else {
            current
        };

        if new_state != current {
            self.transition_to(new_state, now);
        }

        new_state
    }

    /// Process one health check **success**. Applies threshold + cooldown logic.
    /// Returns the new health state.
    pub fn mark_success(&self, cfg: &HealthCheckConfig) -> Health {
        let successes = self.consecutive_successes.fetch_add(1, Ordering::AcqRel) + 1;
        self.consecutive_failures.store(0, Ordering::Relaxed);

        let current     = self.health_state();
        let now         = now_ms();
        let last        = self.last_state_change_ms.load(Ordering::Acquire);
        let in_cooldown = now.saturating_sub(last) < FLAP_COOLDOWN_MS;

        if in_cooldown {
            return current;
        }

        let new_state = if current == Health::Unhealthy && successes >= 1 {
            // Step up through Degraded first — require healthy_threshold sustained
            // successes before fully re-admitting to the pool.
            Health::Degraded
        } else if successes >= cfg.healthy_threshold && current != Health::Healthy {
            Health::Healthy
        } else {
            current
        };

        if new_state != current {
            self.transition_to(new_state, now);
        }

        new_state
    }

    fn transition_to(&self, new_state: Health, timestamp_ms: u64) {
        self.health.store(new_state as u8, Ordering::Release);
        self.last_state_change_ms.store(timestamp_ms, Ordering::Release);
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── Load balancer selection ───────────────────────────────────────────────────

/// Select an endpoint using the configured algorithm.
///
/// Returns `None` only when all endpoints are `Unhealthy`.
/// `Degraded` endpoints participate at half weight.
pub fn select_endpoint<'a>(
    endpoints: &'a [Arc<EndpointState>],
    algorithm: &LoadBalancingConfig,
    hash_key: Option<&str>,
    counter: &AtomicUsize,
) -> Option<&'a Arc<EndpointState>> {
    let available: Vec<_> = endpoints.iter().filter(|e| e.is_available()).collect();
    if available.is_empty() {
        return None;
    }

    match algorithm {
        // ── Weighted Round Robin ──────────────────────────────────────────────
        LoadBalancingConfig::WeightedRoundRobin => {
            // Degraded endpoints count at weight/2 (min 1) — reduced share without
            // full removal. Healthy endpoints take proportionally more traffic.
            let effective_weight = |e: &&Arc<EndpointState>| -> u32 {
                match e.health_state() {
                    Health::Healthy   => e.weight,
                    Health::Degraded  => (e.weight / 2).max(1),
                    Health::Unhealthy => 0,
                }
            };

            let total_weight: u32 = available.iter().map(effective_weight).sum();
            if total_weight == 0 { return available.first().copied(); }

            let pos = (counter.fetch_add(1, Ordering::Relaxed) as u32) % total_weight;
            let mut acc = 0u32;
            for ep in &available {
                acc += effective_weight(ep);
                if pos < acc { return Some(ep); }
            }
            available.first().copied()
        }

        // ── Least Connections (EWMA) ──────────────────────────────────────────
        LoadBalancingConfig::LeastConnections { .. } => {
            available.iter()
                .min_by_key(|e| {
                    let conns   = e.active_conns.load(Ordering::Relaxed);
                    let latency = e.ewma_latency.load(Ordering::Relaxed);
                    // Penalty multiplier for Degraded: they're chosen last among equals
                    let penalty = if e.health_state() == Health::Degraded { 2u64 } else { 1u64 };
                    let base    = if latency == 0 { conns } else { conns.saturating_mul(latency) };
                    base.saturating_mul(penalty)
                })
                .copied()
        }

        // ── Consistent Hashing ────────────────────────────────────────────────
        LoadBalancingConfig::ConsistentHashing { virtual_nodes, .. } => {
            let key  = hash_key.unwrap_or("default");
            let hash = fnv1a_64(key.as_bytes());
            let ring_size = available.len() * virtual_nodes;
            let idx       = (hash as usize) % ring_size / virtual_nodes;
            available.get(idx % available.len()).copied()
        }
    }
}

/// FNV-1a 64-bit hash — fast, no-dep, good distribution for consistent hashing.
fn fnv1a_64(data: &[u8]) -> u64 {
    const OFFSET: u64 = 14695981039346656037;
    const PRIME:  u64 = 1099511628211;
    data.iter().fold(OFFSET, |hash, &b| (hash ^ b as u64).wrapping_mul(PRIME))
}

// ── UpstreamPool ──────────────────────────────────────────────────────────────

pub struct UpstreamPool {
    pub name:       String,
    pub config:     UpstreamConfig,
    pub endpoints:  Vec<Arc<EndpointState>>,
    pub rr_counter: AtomicUsize,
    pub client:     Client<HttpConnector, Full<Bytes>>,
}

impl UpstreamPool {
    pub fn new(name: String, config: UpstreamConfig) -> Self {
        let endpoints = config.endpoints.iter()
            .filter(|e| e.active)
            .map(|e| Arc::new(EndpointState::new(e.address.clone(), e.weight)))
            .collect();

        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(config.timeouts.idle)
            .pool_max_idle_per_host(32)
            .build_http();

        Self { name, config, endpoints, rr_counter: AtomicUsize::new(0), client }
    }

    pub async fn send(
        &self,
        mut req: Request<Full<Bytes>>,
        hash_key: Option<&str>,
    ) -> Result<(Response<Bytes>, Duration), CoreError> {
        let endpoint = select_endpoint(
            &self.endpoints,
            &self.config.load_balancing,
            hash_key,
            &self.rr_counter,
        ).ok_or_else(|| CoreError::NoHealthyEndpoints(self.name.clone()))?;

        let original_uri = req.uri().clone();
        *req.uri_mut() = rewrite_uri(&endpoint.address, &original_uri);

        endpoint.active_conns.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();

        let result = self.send_with_retry(req, endpoint, &self.config.retry).await;

        let elapsed = started.elapsed();
        endpoint.active_conns.fetch_sub(1, Ordering::Relaxed);
        update_ewma(&endpoint.ewma_latency, elapsed.as_millis() as u64);

        result.map(|r| (r, elapsed))
    }

    async fn send_with_retry(
        &self,
        req: Request<Full<Bytes>>,
        endpoint: &Arc<EndpointState>,
        retry: &Option<RetryConfig>,
    ) -> Result<Response<Bytes>, CoreError> {
        let max_attempts   = retry.as_ref().map(|r| r.max_attempts).unwrap_or(1);
        let retry_statuses = retry.as_ref().map(|r| r.on_statuses.clone()).unwrap_or_default();
        let base_backoff   = retry.as_ref()
            .map(|r| r.base_backoff)
            .unwrap_or(Duration::from_millis(50));
        let timeout = self.config.timeouts.request;

        for attempt in 0..max_attempts {
            let attempt_req = clone_request(&req);

            let resp = tokio::time::timeout(timeout, self.client.request(attempt_req))
                .await
                .map_err(|_| CoreError::UpstreamTimeout { secs: timeout.as_secs() })?
                .map_err(|e| CoreError::Upstream {
                    upstream: endpoint.address.clone(),
                    message:  e.to_string(),
                })?;

            let status = resp.status().as_u16();

            if retry_statuses.contains(&status) && attempt + 1 < max_attempts {
                warn!(upstream = %endpoint.address, status, attempt, "retrying upstream request");

                // Exponential backoff WITH ±25% jitter to prevent retry storms.
                //
                // Without jitter: N concurrent failures all retry at the same
                // millisecond, producing a synchronised wave that hammers the
                // upstream harder than the original spike.
                // With jitter: retries spread across a ±25% window, breaking
                // synchronisation and giving the upstream breathing room to recover.
                let base_ms  = base_backoff.as_millis() as u64 * 2u64.pow(attempt);
                let quarter  = (base_ms / 4).max(1);
                // Cheap non-crypto random using system time subseconds
                let nanos    = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos() as u64;
                let jitter   = (nanos % (quarter * 2)).wrapping_sub(quarter);
                let sleep_ms = base_ms.saturating_add(jitter);
                tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
                continue;
            }

            let (parts, body) = resp.into_parts();
            let body_bytes = body.collect().await
                .map_err(|e| CoreError::Upstream {
                    upstream: endpoint.address.clone(),
                    message:  e.to_string(),
                })?
                .to_bytes();

            debug!(
                upstream  = %endpoint.address,
                status,
                bytes     = body_bytes.len(),
                attempt   = attempt + 1,
                "upstream response received"
            );

            return Ok(Response::from_parts(parts, body_bytes));
        }

        Err(CoreError::NoHealthyEndpoints(self.name.clone()))
    }
}

fn rewrite_uri(upstream_addr: &str, original: &http::Uri) -> http::Uri {
    let path_query = original.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    format!("http://{upstream_addr}{path_query}")
        .parse()
        .unwrap_or_else(|_| format!("http://{upstream_addr}/").parse().unwrap())
}

fn clone_request(req: &Request<Full<Bytes>>) -> Request<Full<Bytes>> {
    let mut builder = Request::builder()
        .method(req.method().clone())
        .uri(req.uri().clone())
        .version(req.version());

    for (k, v) in req.headers() {
        builder = builder.header(k, v);
    }

    builder.body(req.body().clone()).unwrap()
}

fn update_ewma(stored: &AtomicU64, new_sample_ms: u64) {
    const ALPHA_X10: u64 = 2; // α = 0.2
    let old = stored.load(Ordering::Relaxed);
    let new = if old == 0 {
        new_sample_ms
    } else {
        (ALPHA_X10 * new_sample_ms + (10 - ALPHA_X10) * old) / 10
    };
    stored.store(new, Ordering::Relaxed);
}

// ── UpstreamRegistry ──────────────────────────────────────────────────────────

pub struct UpstreamRegistry {
    pools: HashMap<String, UpstreamPool>,
}

impl UpstreamRegistry {
    pub fn build(cfg: &GatewayConfig) -> Self {
        let pools = cfg.upstreams.iter()
            .map(|(name, upstream_cfg)| {
                (name.clone(), UpstreamPool::new(name.clone(), upstream_cfg.clone()))
            })
            .collect();

        Self { pools }
    }

    pub fn get(&self, name: &str) -> Option<&UpstreamPool> {
        self.pools.get(name)
    }

    /// Iterate over all (upstream_name, pool) pairs.
    ///
    /// Used by the health check task to enumerate all endpoints that need polling.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &UpstreamPool)> {
        self.pools.iter().map(|(k, v)| (k.as_str(), v))
    }
}
