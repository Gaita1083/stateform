use prometheus::{
    exponential_buckets, histogram_opts, opts, GaugeVec, HistogramVec, IntCounterVec, IntGauge,
    Registry,
};

use crate::error::MetricsError;

/// All Prometheus metrics owned by Stateform.
///
/// Using a private [`Registry`] (not the global default) means:
/// - Tests can create isolated registries without cross-test pollution
/// - Multiple gateway instances in the same process don't collide
/// - The `/metrics` endpoint renders only Stateform metrics — no Go runtime
///   noise leaking in from the default registry
pub struct Metrics {
    pub registry: Registry,

    // ── Request metrics ───────────────────────────────────────────────────────
    /// Total requests received, labelled by route, method, and status class.
    /// status = "2xx" | "3xx" | "4xx" | "5xx"
    pub requests_total: IntCounterVec,

    /// End-to-end request latency (gateway receives → gateway sends response).
    /// Buckets chosen for typical API gateway distribution: sub-ms to 10s.
    pub request_duration_seconds: HistogramVec,

    /// Request body size in bytes.
    pub request_size_bytes: HistogramVec,

    /// Response body size in bytes.
    pub response_size_bytes: HistogramVec,

    // ── Upstream metrics ──────────────────────────────────────────────────────
    /// Requests forwarded to upstream, labelled by route and upstream name.
    pub upstream_requests_total: IntCounterVec,

    /// Time from upstream request send to full response received.
    pub upstream_duration_seconds: HistogramVec,

    // ── Rate limit metrics ────────────────────────────────────────────────────
    /// Requests denied by rate limiting.
    /// layer = "l1_local" | "l2_distributed"
    pub rate_limit_denied_total: IntCounterVec,

    // ── Cache metrics ─────────────────────────────────────────────────────────
    /// Cache lookups by result.
    /// result = "hit_l1" | "hit_l2" | "miss" | "bypass"
    pub cache_lookups_total: IntCounterVec,

    // ── Auth metrics ──────────────────────────────────────────────────────────
    /// Auth failures by mechanism.
    /// mechanism = "jwt" | "api_key" | "mtls"
    pub auth_failures_total: IntCounterVec,

    // ── Connection metrics ────────────────────────────────────────────────────
    /// Currently open inbound connections.
    pub active_connections: IntGauge,

    // ── Operational metrics ───────────────────────────────────────────────────
    /// Config reload attempts.
    /// status = "success" | "failure"
    pub config_reloads_total: IntCounterVec,

    // ── Health check metrics ──────────────────────────────────────────────────
    /// Per-endpoint health state as a gauge.
    ///
    /// Values: 1.0 = Healthy, 0.5 = Degraded, 0.0 = Unhealthy.
    /// Labels: upstream (name), address (host:port).
    ///
    /// Alert rule example:
    ///   `stateform_endpoint_health{upstream="payments"} == 0`
    ///   → "payments upstream has at least one fully unhealthy endpoint"
    pub endpoint_health: GaugeVec,

    // ── Concurrency metrics ───────────────────────────────────────────────────
    /// Requests rejected because the concurrency limit was exceeded.
    /// layer = "global" | "route"
    pub concurrency_rejected_total: IntCounterVec,
}

impl Metrics {
    /// Construct and register all metrics against a fresh private registry.
    pub fn new() -> Result<Self, MetricsError> {
        let registry = Registry::new();

        // ── Latency buckets ───────────────────────────────────────────────────
        // Range: 0.5ms → 10s across 18 buckets.
        // Chosen to capture:
        //   - Sub-ms: cache hits, health checks
        //   - 1–10ms: fast upstream APIs
        //   - 10–100ms: typical database-backed APIs
        //   - 100ms–1s: slow queries, external services
        //   - 1–10s: streaming, long-poll, timeouts
        let latency_buckets = exponential_buckets(0.0005, 2.0, 18)
            .expect("valid bucket spec");

        // ── Size buckets ──────────────────────────────────────────────────────
        // Range: 64B → 64MB across 14 buckets.
        let size_buckets = exponential_buckets(64.0, 4.0, 14)
            .expect("valid bucket spec");

        // ── Request metrics ───────────────────────────────────────────────────
        let requests_total = IntCounterVec::new(
            opts!("stateform_requests_total", "Total inbound requests"),
            &["route_id", "method", "status"],
        ).map_err(|e| MetricsError::Register { name: "stateform_requests_total", source: e })?;

        let request_duration_seconds = HistogramVec::new(
            histogram_opts!(
                "stateform_request_duration_seconds",
                "End-to-end request latency in seconds",
                latency_buckets.clone()
            ),
            &["route_id", "method"],
        ).map_err(|e| MetricsError::Register { name: "stateform_request_duration_seconds", source: e })?;

        let request_size_bytes = HistogramVec::new(
            histogram_opts!(
                "stateform_request_size_bytes",
                "Request body size in bytes",
                size_buckets.clone()
            ),
            &["route_id"],
        ).map_err(|e| MetricsError::Register { name: "stateform_request_size_bytes", source: e })?;

        let response_size_bytes = HistogramVec::new(
            histogram_opts!(
                "stateform_response_size_bytes",
                "Response body size in bytes",
                size_buckets.clone()
            ),
            &["route_id"],
        ).map_err(|e| MetricsError::Register { name: "stateform_response_size_bytes", source: e })?;

        // ── Upstream metrics ──────────────────────────────────────────────────
        let upstream_requests_total = IntCounterVec::new(
            opts!("stateform_upstream_requests_total", "Requests forwarded to upstream"),
            &["route_id", "upstream", "status"],
        ).map_err(|e| MetricsError::Register { name: "stateform_upstream_requests_total", source: e })?;

        let upstream_duration_seconds = HistogramVec::new(
            histogram_opts!(
                "stateform_upstream_duration_seconds",
                "Upstream response time in seconds",
                latency_buckets.clone()
            ),
            &["route_id", "upstream"],
        ).map_err(|e| MetricsError::Register { name: "stateform_upstream_duration_seconds", source: e })?;

        // ── Rate limit metrics ────────────────────────────────────────────────
        let rate_limit_denied_total = IntCounterVec::new(
            opts!("stateform_rate_limit_denied_total", "Requests denied by rate limiter"),
            &["route_id", "layer"],
        ).map_err(|e| MetricsError::Register { name: "stateform_rate_limit_denied_total", source: e })?;

        // ── Cache metrics ─────────────────────────────────────────────────────
        let cache_lookups_total = IntCounterVec::new(
            opts!("stateform_cache_lookups_total", "Cache lookups by result"),
            &["route_id", "result"],
        ).map_err(|e| MetricsError::Register { name: "stateform_cache_lookups_total", source: e })?;

        // ── Auth metrics ──────────────────────────────────────────────────────
        let auth_failures_total = IntCounterVec::new(
            opts!("stateform_auth_failures_total", "Auth failures by mechanism"),
            &["route_id", "mechanism"],
        ).map_err(|e| MetricsError::Register { name: "stateform_auth_failures_total", source: e })?;

        // ── Connection + operational ──────────────────────────────────────────
        let active_connections = IntGauge::new(
            "stateform_active_connections",
            "Currently open inbound connections",
        ).map_err(|e| MetricsError::Register { name: "stateform_active_connections", source: e })?;

        let config_reloads_total = IntCounterVec::new(
            opts!("stateform_config_reloads_total", "Config reload attempts by status"),
            &["status"],
        ).map_err(|e| MetricsError::Register { name: "stateform_config_reloads_total", source: e })?;

        // ── Health check metrics ──────────────────────────────────────────────
        let endpoint_health = GaugeVec::new(
            opts!(
                "stateform_endpoint_health",
                "Endpoint health state: 1=Healthy, 0.5=Degraded, 0=Unhealthy"
            ),
            &["upstream", "address"],
        ).map_err(|e| MetricsError::Register { name: "stateform_endpoint_health", source: e })?;

        // ── Concurrency metrics ───────────────────────────────────────────────
        let concurrency_rejected_total = IntCounterVec::new(
            opts!(
                "stateform_concurrency_rejected_total",
                "Requests rejected due to concurrency limit (global or route)"
            ),
            &["route_id", "layer"],
        ).map_err(|e| MetricsError::Register { name: "stateform_concurrency_rejected_total", source: e })?;

        // ── Register all ─────────────────────────────────────────────────────
        for collector in [
            Box::new(requests_total.clone())            as Box<dyn prometheus::core::Collector>,
            Box::new(request_duration_seconds.clone()),
            Box::new(request_size_bytes.clone()),
            Box::new(response_size_bytes.clone()),
            Box::new(upstream_requests_total.clone()),
            Box::new(upstream_duration_seconds.clone()),
            Box::new(rate_limit_denied_total.clone()),
            Box::new(cache_lookups_total.clone()),
            Box::new(auth_failures_total.clone()),
            Box::new(active_connections.clone()),
            Box::new(config_reloads_total.clone()),
            Box::new(endpoint_health.clone()),
            Box::new(concurrency_rejected_total.clone()),
        ] {
            registry.register(collector)
                .map_err(|e| MetricsError::Register { name: "collector", source: e })?;
        }

        Ok(Self {
            registry,
            requests_total,
            request_duration_seconds,
            request_size_bytes,
            response_size_bytes,
            upstream_requests_total,
            upstream_duration_seconds,
            rate_limit_denied_total,
            cache_lookups_total,
            auth_failures_total,
            active_connections,
            config_reloads_total,
            endpoint_health,
            concurrency_rejected_total,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_metrics_register_without_error() {
        Metrics::new().expect("metrics should register cleanly");
    }

    #[test]
    fn independent_registries_dont_collide() {
        // Two separate Metrics instances must not panic on duplicate registration
        let _m1 = Metrics::new().unwrap();
        let _m2 = Metrics::new().unwrap();
    }
}
