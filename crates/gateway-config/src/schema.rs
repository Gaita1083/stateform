use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

// ── Top-level ────────────────────────────────────────────────────────────────

/// Root configuration object.
///
/// Loaded once from YAML at startup, then hot-swapped via [`ConfigWatcher`]
/// on reload. Every crate receives `Arc<GatewayConfig>` — never a raw reference
/// that could outlive a reload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    pub listener: ListenerConfig,
    pub upstreams: HashMap<String, UpstreamConfig>,
    pub routes: Vec<RouteConfig>,
    pub rate_limiting: RateLimitConfig,
    pub cache: CacheConfig,
    pub auth: AuthConfig,
    pub observability: ObservabilityConfig,
    pub control_plane: ControlPlaneConfig,
}

// ── Listener ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListenerConfig {
    /// Address to bind the HTTP/HTTPS listener, e.g. "0.0.0.0:8080"
    pub bind: String,

    /// Optional TLS configuration. When absent the listener runs plain HTTP.
    pub tls: Option<TlsConfig>,

    /// Max body size in bytes before the request is rejected (default: 4 MiB).
    /// Enforced via Content-Length fast-path check AND a streaming body cap.
    #[serde(default = "defaults::max_body_bytes")]
    pub max_body_bytes: usize,

    /// Idle connection keep-alive timeout
    #[serde(with = "duration_secs", default = "defaults::keepalive_secs")]
    pub keepalive: Duration,

    /// Global ceiling on in-flight requests across ALL routes (process-level
    /// protection). When this limit is reached new requests immediately receive
    /// 503 Service Unavailable with `Retry-After: 1`.
    ///
    /// Default: 10,000. Set based on available memory and upstream capacity.
    /// Rule of thumb: each in-flight request buffers up to `max_body_bytes` RAM.
    #[serde(default = "defaults::max_concurrent_requests")]
    pub max_concurrent_requests: usize,

    /// How long (ms) to wait for a concurrency permit before returning 503.
    /// Keeps latency predictable under overload — don't let requests queue
    /// indefinitely inside the gateway.
    ///
    /// Default: 500 ms.
    #[serde(default = "defaults::concurrency_acquire_timeout_ms")]
    pub concurrency_acquire_timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    pub cert_path: String,
    pub key_path: String,
    /// Optional CA for mTLS client verification
    pub client_ca_path: Option<String>,
}

// ── Upstream ─────────────────────────────────────────────────────────────────

/// A named upstream cluster. Referred to by name from [`RouteConfig`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamConfig {
    pub endpoints: Vec<EndpointConfig>,
    pub load_balancing: LoadBalancingConfig,
    pub health_check: Option<HealthCheckConfig>,
    pub timeouts: TimeoutConfig,
    pub retry: Option<RetryConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointConfig {
    pub address: String,
    /// Relative weight for Weighted Round Robin (default: 1)
    #[serde(default = "defaults::weight")]
    pub weight: u32,
    /// Whether to start this endpoint as active (default: true)
    #[serde(default = "defaults::r#true")]
    pub active: bool,
}

// ── Load balancing ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "algorithm", rename_all = "snake_case")]
pub enum LoadBalancingConfig {
    /// L1: Cross-cluster, weight-aware
    WeightedRoundRobin,

    /// L2: Pod-level, connection-count minimisation with EWMA decay
    LeastConnections {
        /// Decay factor for EWMA latency smoothing (0.0–1.0, default: 0.3)
        #[serde(default = "defaults::ewma_alpha")]
        ewma_alpha: f64,
    },

    /// L3: Stateful routes (sessions, caching, chat)
    ConsistentHashing {
        /// Request header whose value is used as the hash key
        hash_header: String,
        /// Number of virtual nodes per endpoint (default: 150)
        #[serde(default = "defaults::vnodes")]
        virtual_nodes: usize,
    },
}

// ── Health check ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckConfig {
    pub path: String,
    #[serde(with = "duration_secs", default = "defaults::health_interval")]
    pub interval: Duration,
    #[serde(with = "duration_secs", default = "defaults::health_timeout")]
    pub timeout: Duration,
    /// Consecutive successes required to mark healthy (default: 2)
    #[serde(default = "defaults::healthy_threshold")]
    pub healthy_threshold: u32,
    /// Consecutive failures required to mark unhealthy (default: 3)
    #[serde(default = "defaults::unhealthy_threshold")]
    pub unhealthy_threshold: u32,
}

// ── Timeouts & retry ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutConfig {
    #[serde(with = "duration_secs", default = "defaults::connect_timeout")]
    pub connect: Duration,
    #[serde(with = "duration_secs", default = "defaults::request_timeout")]
    pub request: Duration,
    #[serde(with = "duration_secs", default = "defaults::idle_timeout")]
    pub idle: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    pub max_attempts: u32,
    #[serde(with = "duration_millis", default = "defaults::retry_backoff")]
    pub base_backoff: Duration,
    /// HTTP status codes that trigger a retry
    #[serde(default = "defaults::retry_statuses")]
    pub on_statuses: Vec<u16>,
}

// ── Route ─────────────────────────────────────────────────────────────────────

/// A single routing rule evaluated top-to-bottom.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteConfig {
    /// Human-readable identifier used in metrics + logs
    pub id: String,

    pub match_: RouteMatch,

    /// Name of the upstream from [`GatewayConfig::upstreams`]
    pub upstream: String,

    /// Load balancing override for this specific route.
    /// When absent, inherits from the upstream.
    pub load_balancing: Option<LoadBalancingConfig>,

    /// Middleware stack applied to this route (order matters).
    #[serde(default)]
    pub middleware: MiddlewareConfig,

    /// Optional request/response header mutations
    #[serde(default)]
    pub headers: HeaderMutations,

    /// Per-route concurrency limit for bulkhead isolation.
    ///
    /// When set, requests beyond this limit for THIS route receive 503, even
    /// if the global limit has capacity. Prevents a single slow upstream from
    /// starving all other routes.
    ///
    /// `None` = only the global limit applies (default).
    #[serde(default)]
    pub max_concurrent_requests: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteMatch {
    /// Path prefix or exact path
    pub path: PathMatch,
    /// Optional HTTP method filter (any method if absent)
    pub method: Option<String>,
    /// Optional header-based matching conditions
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PathMatch {
    Prefix { value: String },
    Exact  { value: String },
    Regex  { pattern: String },
}

// ── Middleware ────────────────────────────────────────────────────────────────

/// Per-route middleware toggles and overrides.
/// Order of evaluation: auth → rate_limit → cache.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MiddlewareConfig {
    pub auth: Option<AuthMiddlewareConfig>,
    pub rate_limit: Option<RateLimitMiddlewareRef>,
    pub cache: Option<CacheMiddlewareConfig>,
}

/// Which auth mechanisms to enforce on this route
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthMiddlewareConfig {
    #[serde(default)]
    pub jwt: bool,
    #[serde(default)]
    pub api_key: bool,
    #[serde(default)]
    pub mtls: bool,
}

/// References a named rate-limit policy from [`RateLimitConfig::policies`]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitMiddlewareRef {
    pub policy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheMiddlewareConfig {
    #[serde(with = "duration_secs")]
    pub ttl: Duration,
    /// When true the cached response is only served, never populated
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HeaderMutations {
    /// Headers to add/overwrite on the request before forwarding
    #[serde(default)]
    pub request_set: HashMap<String, String>,
    /// Headers to remove from the request before forwarding
    #[serde(default)]
    pub request_remove: Vec<String>,
    /// Headers to add/overwrite on the response before returning
    #[serde(default)]
    pub response_set: HashMap<String, String>,
}

// ── Rate limiting ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    pub redis_url: String,
    /// Named policies referenced from route middleware
    pub policies: HashMap<String, RateLimitPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitPolicy {
    /// L1: Local in-process token bucket (fast path, no network)
    pub local: TokenBucketConfig,
    /// L2: Distributed enforcement via Redis
    pub distributed: DistributedRateLimitConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenBucketConfig {
    /// Max tokens in the bucket
    pub capacity: u64,
    /// Tokens added per second
    pub refill_rate: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "algorithm", rename_all = "snake_case")]
pub enum DistributedRateLimitConfig {
    /// General-purpose endpoints
    TokenBucket {
        capacity: u64,
        refill_rate: u64,
    },
    /// Sensitive endpoints (auth, payments, etc.)
    SlidingWindow {
        /// Max requests allowed in the window
        max_requests: u64,
        #[serde(with = "duration_secs")]
        window: Duration,
    },
}

// ── Cache ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    pub redis_url: String,
    /// L1 in-process cache capacity in number of entries
    #[serde(default = "defaults::l1_cache_capacity")]
    pub l1_capacity: usize,
}

// ── Auth ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    pub jwt: JwtConfig,
    pub api_keys: ApiKeyConfig,
    pub mtls: MtlsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwtConfig {
    /// JWKS endpoint URL for public key discovery (OIDC-compatible)
    pub jwks_url: String,
    /// Accepted audience values
    pub audience: Vec<String>,
    /// Accepted issuer values
    pub issuers: Vec<String>,
    /// How long to cache the JWKS before re-fetching
    #[serde(with = "duration_secs", default = "defaults::jwks_cache_ttl")]
    pub jwks_cache_ttl: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyConfig {
    /// Redis key prefix for API key lookups, e.g. "stateform:apikey:"
    pub redis_key_prefix: String,
    pub redis_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MtlsConfig {
    /// Path to the CA bundle used to verify client certificates
    pub client_ca_path: String,
    /// When true, require a valid client cert on every connection
    #[serde(default = "defaults::r#true")]
    pub required: bool,
}

// ── Observability ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservabilityConfig {
    /// Address to serve /metrics on (Prometheus scrape target)
    pub metrics_bind: String,
}

// ── Control plane bridge ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlPlaneConfig {
    /// gRPC address of the Go control plane
    pub grpc_address: String,
    #[serde(with = "duration_secs", default = "defaults::cp_reconnect")]
    pub reconnect_interval: Duration,
}

// ── Serde helpers ─────────────────────────────────────────────────────────────

/// Serialise/deserialise Duration as integer seconds
mod duration_secs {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        d.as_secs().serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_secs(u64::deserialize(d)?))
    }
}

/// Serialise/deserialise Duration as integer milliseconds
mod duration_millis {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        d.as_millis().serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_millis(u64::deserialize(d)?))
    }
}

// ── Sensible defaults ─────────────────────────────────────────────────────────

mod defaults {
    use std::time::Duration;

    pub fn max_body_bytes() -> usize              { 4 * 1024 * 1024 } // 4 MiB
    pub fn keepalive_secs() -> Duration           { Duration::from_secs(75) }
    pub fn max_concurrent_requests() -> usize     { 10_000 }
    pub fn concurrency_acquire_timeout_ms() -> u64 { 500 }
    pub fn weight() -> u32                  { 1 }
    pub fn r#true() -> bool                 { true }
    pub fn ewma_alpha() -> f64              { 0.3 }
    pub fn vnodes() -> usize                { 150 }
    pub fn health_interval() -> Duration    { Duration::from_secs(10) }
    pub fn health_timeout() -> Duration     { Duration::from_secs(2) }
    pub fn healthy_threshold() -> u32       { 2 }
    pub fn unhealthy_threshold() -> u32     { 3 }
    pub fn connect_timeout() -> Duration    { Duration::from_secs(2) }
    pub fn request_timeout() -> Duration    { Duration::from_secs(30) }
    pub fn idle_timeout() -> Duration       { Duration::from_secs(90) }
    pub fn retry_backoff() -> Duration      { Duration::from_millis(50) }
    pub fn retry_statuses() -> Vec<u16>     { vec![502, 503, 504] }
    pub fn l1_cache_capacity() -> usize     { 10_000 }
    pub fn jwks_cache_ttl() -> Duration     { Duration::from_secs(300) }
    pub fn cp_reconnect() -> Duration       { Duration::from_secs(5) }
}
