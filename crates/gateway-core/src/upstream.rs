use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use tracing::{debug, warn};

use gateway_config::{
    GatewayConfig, LoadBalancingConfig, RetryConfig, UpstreamConfig,
};

use crate::error::CoreError;

// ── Endpoint health ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Healthy,
    Unhealthy,
}

/// Runtime state tracked per endpoint — updated by the health checker.
#[derive(Debug)]
pub struct EndpointState {
    pub address:      String,
    pub weight:       u32,
    pub health:       Health,
    /// Active connection count for Least Connections algorithm
    pub active_conns: std::sync::atomic::AtomicU64,
    /// EWMA latency estimate in milliseconds (Least Connections / EWMA variant)
    pub ewma_latency: std::sync::atomic::AtomicU64,
}

impl EndpointState {
    fn new(address: String, weight: u32) -> Self {
        Self {
            address,
            weight,
            health: Health::Healthy,
            active_conns: std::sync::atomic::AtomicU64::new(0),
            ewma_latency: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn is_available(&self) -> bool {
        self.health == Health::Healthy
    }
}

// ── Load balancer selection ───────────────────────────────────────────────────

/// Select an endpoint from the pool using the configured algorithm.
///
/// Returns `None` only when all endpoints are unhealthy.
pub fn select_endpoint<'a>(
    endpoints: &'a [Arc<EndpointState>],
    algorithm: &LoadBalancingConfig,
    hash_key: Option<&str>,
    counter: &std::sync::atomic::AtomicUsize,
) -> Option<&'a Arc<EndpointState>> {
    let available: Vec<_> = endpoints.iter().filter(|e| e.is_available()).collect();
    if available.is_empty() {
        return None;
    }

    match algorithm {
        // ── Weighted Round Robin ──────────────────────────────────────────────
        LoadBalancingConfig::WeightedRoundRobin => {
            let total_weight: u32 = available.iter().map(|e| e.weight).sum();
            if total_weight == 0 { return available.first().copied(); }

            // Map the counter to a position in the weight space
            let pos = (counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                as u32) % total_weight;
            let mut acc = 0u32;
            for ep in &available {
                acc += ep.weight;
                if pos < acc { return Some(ep); }
            }
            available.first().copied()
        }

        // ── Least Connections (EWMA) ──────────────────────────────────────────
        LoadBalancingConfig::LeastConnections { .. } => {
            available.iter()
                .min_by_key(|e| {
                    let conns = e.active_conns.load(std::sync::atomic::Ordering::Relaxed);
                    let latency = e.ewma_latency.load(std::sync::atomic::Ordering::Relaxed);
                    // Score = active_conns * ewma_latency_ms (or just conns if latency unknown)
                    if latency == 0 { conns } else { conns.saturating_mul(latency) }
                })
                .copied()
        }

        // ── Consistent Hashing ────────────────────────────────────────────────
        LoadBalancingConfig::ConsistentHashing { virtual_nodes, .. } => {
            let key = hash_key.unwrap_or("default");
            let hash = fnv1a_64(key.as_bytes());

            // Build a minimal virtual ring from available endpoints
            // In production: precompute this ring and store it with the endpoint pool
            let ring_size = available.len() * virtual_nodes;
            let idx = (hash as usize) % ring_size / virtual_nodes;
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

/// Per-upstream connection pool and endpoint state.
pub struct UpstreamPool {
    pub name:       String,
    pub config:     UpstreamConfig,
    pub endpoints:  Vec<Arc<EndpointState>>,
    /// Monotonic counter for round-robin selection
    pub rr_counter: std::sync::atomic::AtomicUsize,
    /// Hyper HTTP client — reuses connections via keep-alive
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

        Self {
            name,
            config,
            endpoints,
            rr_counter: std::sync::atomic::AtomicUsize::new(0),
            client,
        }
    }

    /// Select an endpoint and send the request upstream.
    ///
    /// Applies retries per the upstream's retry config.
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

        // Rewrite the URI to point at the selected endpoint
        let original_uri = req.uri().clone();
        *req.uri_mut() = rewrite_uri(&endpoint.address, &original_uri);

        // Track active connections for Least Connections scoring
        endpoint.active_conns.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let started = Instant::now();

        let result = self.send_with_retry(req, endpoint, &self.config.retry).await;

        let elapsed = started.elapsed();
        endpoint.active_conns.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        update_ewma(&endpoint.ewma_latency, elapsed.as_millis() as u64);

        result.map(|r| (r, elapsed))
    }

    async fn send_with_retry(
        &self,
        req: Request<Full<Bytes>>,
        endpoint: &Arc<EndpointState>,
        retry: &Option<RetryConfig>,
    ) -> Result<Response<Bytes>, CoreError> {
        let max_attempts = retry.as_ref().map(|r| r.max_attempts).unwrap_or(1);
        let retry_statuses: Vec<u16> = retry.as_ref()
            .map(|r| r.on_statuses.clone())
            .unwrap_or_default();
        let base_backoff = retry.as_ref()
            .map(|r| r.base_backoff)
            .unwrap_or(Duration::from_millis(50));

        let timeout = self.config.timeouts.request;

        for attempt in 0..max_attempts {
            // Clone the request for retry (body must be buffered — we use Full<Bytes>)
            let attempt_req = clone_request(&req);

            let resp = tokio::time::timeout(timeout, self.client.request(attempt_req))
                .await
                .map_err(|_| CoreError::UpstreamTimeout { secs: timeout.as_secs() })?
                .map_err(|e| CoreError::Upstream {
                    upstream: endpoint.address.clone(),
                    message: e.to_string(),
                })?;

            let status = resp.status().as_u16();

            if retry_statuses.contains(&status) && attempt + 1 < max_attempts {
                warn!(
                    upstream = %endpoint.address,
                    status,
                    attempt,
                    "retrying upstream request"
                );
                tokio::time::sleep(base_backoff * 2u32.pow(attempt)).await;
                continue;
            }

            // Collect the body into Bytes for caching and metrics
            let (parts, body) = resp.into_parts();
            let body_bytes = body.collect().await
                .map_err(|e| CoreError::Upstream {
                    upstream: endpoint.address.clone(),
                    message: e.to_string(),
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

/// Rewrite an incoming request URI to target a specific upstream address.
fn rewrite_uri(upstream_addr: &str, original: &http::Uri) -> http::Uri {
    let scheme = "http"; // TLS termination is at the gateway; upstream is plain HTTP
    let path_query = original
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");

    format!("{scheme}://{upstream_addr}{path_query}")
        .parse()
        .unwrap_or_else(|_| format!("http://{upstream_addr}/").parse().unwrap())
}

/// Clone a `Request<Full<Bytes>>` — safe because `Full<Bytes>` is `Clone`.
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

/// Exponential weighted moving average update.
/// α = 0.2 — recent samples count more without wild swings.
fn update_ewma(stored: &std::sync::atomic::AtomicU64, new_sample_ms: u64) {
    use std::sync::atomic::Ordering;
    const ALPHA_X10: u64 = 2; // 0.2 × 10

    let old = stored.load(Ordering::Relaxed);
    if old == 0 {
        stored.store(new_sample_ms, Ordering::Relaxed);
        return;
    }
    // EWMA without floats: scale by 10 to keep integer precision
    let new = (ALPHA_X10 * new_sample_ms + (10 - ALPHA_X10) * old) / 10;
    stored.store(new, Ordering::Relaxed);
}

// ── UpstreamRegistry ──────────────────────────────────────────────────────────

/// All upstream pools, looked up by name on every request.
pub struct UpstreamRegistry {
    pools: HashMap<String, UpstreamPool>,
}

impl UpstreamRegistry {
    pub fn build(cfg: &GatewayConfig) -> Self {
        let pools = cfg.upstreams.iter()
            .map(|(name, upstream_cfg)| {
                let pool = UpstreamPool::new(name.clone(), upstream_cfg.clone());
                (name.clone(), pool)
            })
            .collect();

        Self { pools }
    }

    pub fn get(&self, name: &str) -> Option<&UpstreamPool> {
        self.pools.get(name)
    }
}
