use std::sync::Arc;
use std::time::{Duration, Instant};

use gateway_auth::AuthError;
use gateway_cache::CacheLookup;
use gateway_ratelimit::{RateLimitOutcome, DeniedBy};

use crate::registry::Metrics;

/// High-level recording API consumed by `gateway-core`.
///
/// Wraps the raw Prometheus metric objects with purpose-built methods
/// so call sites read as intent, not as metric manipulation:
///
/// ```rust,ignore
/// recorder.record_request("api-v1", "GET", 200, duration, req_bytes, resp_bytes);
/// recorder.record_cache_lookup("api-v1", &lookup_result);
/// recorder.record_rate_limit("api-v1", &outcome);
/// ```
///
/// All methods take `&self` — the underlying `Arc<Metrics>` is cheaply
/// cloned per request handler without any locking.
#[derive(Clone)]
pub struct MetricsRecorder {
    inner: Arc<Metrics>,
}

impl MetricsRecorder {
    pub fn new(metrics: Metrics) -> Self {
        Self { inner: Arc::new(metrics) }
    }

    /// Access the raw registry for the `/metrics` endpoint encoder.
    pub fn registry(&self) -> &prometheus::Registry {
        &self.inner.registry
    }

    // ── Request lifecycle ─────────────────────────────────────────────────────

    /// Record a completed inbound request.
    ///
    /// Call once per request, after the response has been sent.
    pub fn record_request(
        &self,
        route_id: &str,
        method: &str,
        status: u16,
        duration: Duration,
        request_bytes: usize,
        response_bytes: usize,
    ) {
        let status_class = status_class(status);
        let method = method.to_ascii_uppercase();

        self.inner.requests_total
            .with_label_values(&[route_id, &method, status_class])
            .inc();

        self.inner.request_duration_seconds
            .with_label_values(&[route_id, &method])
            .observe(duration.as_secs_f64());

        if request_bytes > 0 {
            self.inner.request_size_bytes
                .with_label_values(&[route_id])
                .observe(request_bytes as f64);
        }

        if response_bytes > 0 {
            self.inner.response_size_bytes
                .with_label_values(&[route_id])
                .observe(response_bytes as f64);
        }
    }

    // ── Upstream ──────────────────────────────────────────────────────────────

    /// Record a single upstream request-response cycle.
    pub fn record_upstream(
        &self,
        route_id: &str,
        upstream_name: &str,
        status: u16,
        duration: Duration,
    ) {
        let status_class = status_class(status);

        self.inner.upstream_requests_total
            .with_label_values(&[route_id, upstream_name, status_class])
            .inc();

        self.inner.upstream_duration_seconds
            .with_label_values(&[route_id, upstream_name])
            .observe(duration.as_secs_f64());
    }

    // ── Rate limiting ─────────────────────────────────────────────────────────

    /// Record the outcome of a rate-limit check.
    /// Only increments a counter when the request was denied.
    pub fn record_rate_limit(&self, route_id: &str, outcome: &RateLimitOutcome) {
        if outcome.allowed {
            return;
        }
        let layer = match &outcome.denied_by {
            Some(DeniedBy::L1Local)       => "l1_local",
            Some(DeniedBy::L2Distributed) => "l2_distributed",
            None                          => "unknown",
        };
        self.inner.rate_limit_denied_total
            .with_label_values(&[route_id, layer])
            .inc();
    }

    // ── Cache ─────────────────────────────────────────────────────────────────

    /// Record the result of a cache lookup.
    pub fn record_cache_lookup(&self, route_id: &str, lookup: &CacheLookup) {
        let result = match lookup {
            CacheLookup::Hit(_) => "hit",   // L1/L2 distinction tracked separately if needed
            CacheLookup::Miss   => "miss",
            CacheLookup::Bypass => "bypass",
        };
        self.inner.cache_lookups_total
            .with_label_values(&[route_id, result])
            .inc();
    }

    // ── Auth ──────────────────────────────────────────────────────────────────

    /// Record an auth failure. Only called when the pipeline returns an error.
    pub fn record_auth_failure(&self, route_id: &str, err: &AuthError) {
        let mechanism = match err {
            AuthError::MissingToken
            | AuthError::InvalidToken(_)
            | AuthError::JwksFetch(_)
            | AuthError::UnknownKid(_)   => "jwt",

            AuthError::MissingApiKey
            | AuthError::InvalidApiKey
            | AuthError::ApiKeyStore(_)  => "api_key",

            AuthError::MissingClientCert
            | AuthError::InvalidClientCert(_) => "mtls",

            AuthError::NoProviderEnabled | AuthError::Config(_) => "none",
        };
        self.inner.auth_failures_total
            .with_label_values(&[route_id, mechanism])
            .inc();
    }

    // ── Connections ───────────────────────────────────────────────────────────

    /// Increment the active connection gauge when a connection is accepted.
    pub fn connection_open(&self) {
        self.inner.active_connections.inc();
    }

    /// Decrement the active connection gauge when a connection closes.
    pub fn connection_close(&self) {
        self.inner.active_connections.dec();
    }

    // ── Config reload ─────────────────────────────────────────────────────────

    pub fn record_config_reload_success(&self) {
        self.inner.config_reloads_total
            .with_label_values(&["success"])
            .inc();
    }

    pub fn record_config_reload_failure(&self) {
        self.inner.config_reloads_total
            .with_label_values(&["failure"])
            .inc();
    }

    // ── Health check ──────────────────────────────────────────────────────────

    /// Update the `stateform_endpoint_health` gauge for one endpoint.
    ///
    /// `gauge_value` should be the result of `Health::as_gauge_value()` in
    /// `gateway-core`:
    ///   - `1.0` = Healthy
    ///   - `0.5` = Degraded (operator should investigate)
    ///   - `0.0` = Unhealthy (alert should fire, traffic excluded)
    ///
    /// Passing a plain `f64` keeps `gateway-metrics` free of a direct dependency
    /// on `gateway-core` (which would create a circular dependency cycle).
    pub fn record_endpoint_health(
        &self,
        upstream:    &str,
        address:     &str,
        gauge_value: f64,
    ) {
        self.inner.endpoint_health
            .with_label_values(&[upstream, address])
            .set(gauge_value);
    }

    // ── Concurrency ───────────────────────────────────────────────────────────

    /// Record a request rejected by the concurrency limiter.
    ///
    /// `layer`: `"global"` when the process-wide limit fired, `"route"` when
    /// the per-route bulkhead fired. Lets operators distinguish overload patterns:
    ///   - `global` → system needs more capacity or the global limit is too low
    ///   - `route`  → one route is noisy; increase its limit or scale its upstream
    pub fn record_concurrency_rejected(&self, route_id: &str, layer: &str) {
        self.inner.concurrency_rejected_total
            .with_label_values(&[route_id, layer])
            .inc();
    }
}

// ── Request timer ─────────────────────────────────────────────────────────────

/// RAII guard that records request duration on drop.
///
/// Usage:
/// ```rust,ignore
/// let _timer = RequestTimer::start(&recorder, route_id, method);
/// // ... handle request ...
/// // timer records duration automatically when it goes out of scope
/// ```
pub struct RequestTimer<'a> {
    recorder: &'a MetricsRecorder,
    route_id: String,
    method:   String,
    started:  Instant,
}

impl<'a> RequestTimer<'a> {
    pub fn start(recorder: &'a MetricsRecorder, route_id: &str, method: &str) -> Self {
        Self {
            recorder,
            route_id: route_id.to_string(),
            method:   method.to_string(),
            started:  Instant::now(),
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

impl Drop for RequestTimer<'_> {
    fn drop(&mut self) {
        self.recorder.inner
            .request_duration_seconds
            .with_label_values(&[&self.route_id, &self.method])
            .observe(self.started.elapsed().as_secs_f64());
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Collapse a numeric HTTP status code into a Prometheus-friendly class string.
fn status_class(status: u16) -> &'static str {
    match status {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        500..=599 => "5xx",
        _         => "unknown",
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Metrics;
    use std::time::Duration;

    fn recorder() -> MetricsRecorder {
        MetricsRecorder::new(Metrics::new().unwrap())
    }

    #[test]
    fn record_request_increments_counter() {
        let r = recorder();
        r.record_request("api", "GET", 200, Duration::from_millis(10), 128, 512);

        let families = r.registry().gather();
        let req_family = families.iter()
            .find(|f| f.get_name() == "stateform_requests_total")
            .expect("metric registered");

        assert!(!req_family.get_metric().is_empty());
    }

    #[test]
    fn status_class_mapping() {
        // 1xx
        assert_eq!(status_class(100), "1xx");
        // 2xx
        assert_eq!(status_class(200), "2xx");
        assert_eq!(status_class(201), "2xx");
        assert_eq!(status_class(204), "2xx");
        // 3xx
        assert_eq!(status_class(301), "3xx");
        assert_eq!(status_class(304), "3xx");
        // 4xx
        assert_eq!(status_class(400), "4xx");
        assert_eq!(status_class(401), "4xx");
        assert_eq!(status_class(404), "4xx");
        assert_eq!(status_class(429), "4xx");
        // 5xx
        assert_eq!(status_class(500), "5xx");
        assert_eq!(status_class(502), "5xx");
        assert_eq!(status_class(503), "5xx");
        // unknown
        assert_eq!(status_class(0),   "unknown");
        assert_eq!(status_class(600), "unknown");
    }

    #[test]
    fn connection_gauge_increments_and_decrements() {
        let r = recorder();
        r.connection_open();
        r.connection_open();
        r.connection_close();

        let families = r.registry().gather();
        let gauge = families.iter()
            .find(|f| f.get_name() == "stateform_active_connections")
            .expect("metric registered");

        let value = gauge.get_metric()[0].get_gauge().get_value();
        assert_eq!(value, 1.0);
    }

    #[test]
    fn rate_limit_allowed_does_not_increment() {
        let r = recorder();
        let outcome = RateLimitOutcome {
            allowed: true,
            remaining: 99,
            limit: 100,
            retry_after_ms: 0,
            denied_by: None,
        };
        r.record_rate_limit("api", &outcome);

        let families = r.registry().gather();
        let rl = families.iter()
            .find(|f| f.get_name() == "stateform_rate_limit_denied_total");

        // Counter family should be present but have no samples yet
        if let Some(fam) = rl {
            assert!(fam.get_metric().is_empty());
        }
    }

    #[test]
    fn request_timer_records_on_drop() {
        let r = recorder();
        {
            let _t = RequestTimer::start(&r, "route-1", "POST");
            std::thread::sleep(Duration::from_millis(1));
        } // timer drops here

        let families = r.registry().gather();
        let hist = families.iter()
            .find(|f| f.get_name() == "stateform_request_duration_seconds")
            .expect("histogram registered");

        let sample_count = hist.get_metric()[0].get_histogram().get_sample_count();
        assert_eq!(sample_count, 1);
    }
}
