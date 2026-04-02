use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::Full;
use tracing::{debug, info, instrument, warn};

use gateway_auth::{AuthContext, AuthPipeline};
use gateway_cache::{CacheLookup, ResponseCache};
use gateway_metrics::MetricsRecorder;
use gateway_ratelimit::RateLimiter;
use gateway_router::RouterHandle;

use crate::{
    error::CoreError,
    upstream::UpstreamRegistry,
};

// ── Pipeline state ────────────────────────────────────────────────────────────

/// All shared state threaded through every request.
///
/// Wrapped in `Arc` and cloned once per connection (cheap — all inner fields
/// are already behind `Arc` or atomic primitives).
pub struct Pipeline {
    pub router:    Arc<RouterHandle>,
    pub auth:      Arc<AuthPipeline>,
    pub ratelimit: Arc<RateLimiter>,
    pub cache:     Arc<ResponseCache>,
    pub metrics:   MetricsRecorder,
    pub upstreams: Arc<UpstreamRegistry>,
}

impl Clone for Pipeline {
    fn clone(&self) -> Self {
        Self {
            router:    Arc::clone(&self.router),
            auth:      Arc::clone(&self.auth),
            ratelimit: Arc::clone(&self.ratelimit),
            cache:     Arc::clone(&self.cache),
            metrics:   self.metrics.clone(),
            upstreams: Arc::clone(&self.upstreams),
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Process one HTTP request through the full middleware pipeline.
///
/// Never panics — all errors are converted to appropriate HTTP responses.
/// The caller is responsible for recording final metrics after this returns.
#[instrument(skip_all, fields(method, path))]
pub async fn handle(
    pipeline: &Pipeline,
    req: Request<Bytes>,
) -> Response<Full<Bytes>> {
    let started  = Instant::now();
    let method   = req.method().to_string();
    let path     = req.uri().path().to_string();
    let query    = req.uri().query().map(str::to_string);
    let req_size = req.body().len();

    tracing::Span::current()
        .record("method", &method.as_str())
        .record("path", &path.as_str());

    // ── 1. Route matching ─────────────────────────────────────────────────────
    let matched = match pipeline.router.current().match_request(
        &path,
        req.method(),
        req.headers(),
    ) {
        Ok(m) => m,
        Err(_) => {
            debug!(path, "no route matched");
            return error_response(StatusCode::NOT_FOUND, "no route matched");
        }
    };

    let route_id = matched.route_id().to_string();
    let upstream_name = matched.upstream().to_string();

    // ── 2. Auth ───────────────────────────────────────────────────────────────
    let auth_ctx: Option<AuthContext> = match pipeline.auth.run(
        matched.route.middleware.auth.as_ref(),
        req.headers(),
        req.extensions(),
    ).await {
        Ok(ctx) => ctx,
        Err(e) => {
            pipeline.metrics.record_auth_failure(&route_id, &e);
            let status = StatusCode::from_u16(e.status_code())
                .unwrap_or(StatusCode::UNAUTHORIZED);
            return error_response(status, &e.to_string());
        }
    };

    // ── 3. Rate limit ─────────────────────────────────────────────────────────
    let rl_outcome = match pipeline.ratelimit.check(
        matched.route.middleware.rate_limit.as_ref(),
        &route_id,
        auth_ctx.as_ref(),
        req.headers(),
    ).await {
        Ok(o) => o,
        Err(e) => {
            warn!(error = %e, route_id, "rate limit error — allowing request");
            gateway_ratelimit::RateLimitOutcome {
                allowed: true,
                remaining: 0,
                limit: 0,
                retry_after_ms: 0,
                denied_by: None,
            }
        }
    };

    pipeline.metrics.record_rate_limit(&route_id, &rl_outcome);

    if !rl_outcome.allowed {
        let mut resp = error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "rate limit exceeded",
        );
        for (k, v) in rl_outcome.to_headers() {
            if let (Ok(name), Ok(val)) = (
                http::header::HeaderName::from_bytes(k.as_bytes()),
                http::header::HeaderValue::from_str(&v),
            ) {
                resp.headers_mut().insert(name, val);
            }
        }
        return resp;
    }

    // ── 4. Cache lookup ───────────────────────────────────────────────────────
    let cache_lookup = pipeline.cache.lookup(
        matched.route.middleware.cache.as_ref(),
        &route_id,
        &method,
        &path,
        query.as_deref(),
        req.headers(),
    ).await;

    pipeline.metrics.record_cache_lookup(&route_id, &cache_lookup);

    if let CacheLookup::Hit(cached) = cache_lookup {
        debug!(route_id, "serving from cache");
        return build_cached_response(cached);
    }

    // ── 5. Upstream proxy ─────────────────────────────────────────────────────
    let upstream = match pipeline.upstreams.get(&upstream_name) {
        Some(u) => u,
        None => {
            warn!(upstream = %upstream_name, "upstream not found");
            return error_response(StatusCode::BAD_GATEWAY, "upstream not configured");
        }
    };

    // Derive consistent-hash key from the configured header (if applicable)
    let hash_key: Option<String> = matched.route.load_balancing
        .as_ref()
        .or(Some(&upstream.config.load_balancing))
        .and_then(|lb| {
            if let gateway_config::LoadBalancingConfig::ConsistentHashing { hash_header, .. } = lb {
                req.headers()
                    .get(hash_header.as_str())
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            } else {
                None
            }
        });

    // Apply request header mutations from the route config
    let upstream_req = apply_request_mutations(req, &matched.route.headers);

    let upstream_result = upstream.send(
        upstream_req,
        hash_key.as_deref(),
    ).await;

    let (upstream_resp, upstream_duration) = match upstream_result {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, upstream = %upstream_name, "upstream error");
            let status = match &e {
                CoreError::UpstreamTimeout { .. } => StatusCode::GATEWAY_TIMEOUT,
                _ => StatusCode::BAD_GATEWAY,
            };
            return error_response(status, &e.to_string());
        }
    };

    pipeline.metrics.record_upstream(
        &route_id,
        &upstream_name,
        upstream_resp.status().as_u16(),
        upstream_duration,
    );

    // ── 6. Cache store ────────────────────────────────────────────────────────
    pipeline.cache.store(
        matched.route.middleware.cache.as_ref(),
        &route_id,
        &method,
        &path,
        query.as_deref(),
        upstream_resp.status().as_u16(),
        upstream_resp.headers(),
        upstream_resp.body().clone(),
        // pass original request headers for Vary key derivation
        upstream_resp.headers(), // placeholder — see note below
    ).await;

    // ── 7. Response mutations + final metrics ─────────────────────────────────
    let resp_size = upstream_resp.body().len();
    let status = upstream_resp.status().as_u16();

    pipeline.metrics.record_request(
        &route_id,
        &method,
        status,
        started.elapsed(),
        req_size,
        resp_size,
    );

    info!(
        route_id,
        method,
        path,
        status,
        upstream  = %upstream_name,
        latency_ms = started.elapsed().as_millis(),
        "request complete"
    );

    build_upstream_response(upstream_resp, &matched.route.headers)
}

// ── Response builders ─────────────────────────────────────────────────────────

fn error_response(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    let body = serde_json::json!({ "error": message }).to_string();
    Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

fn build_cached_response(cached: gateway_cache::CachedResponse) -> Response<Full<Bytes>> {
    let mut builder = Response::builder()
        .status(cached.status)
        .header("X-Cache", "HIT")
        .header("Age", cached.age_header());

    for (k, v) in &cached.headers {
        if let (Ok(name), Ok(val)) = (
            http::header::HeaderName::from_bytes(k.as_bytes()),
            http::header::HeaderValue::from_str(v),
        ) {
            builder = builder.header(name, val);
        }
    }

    builder.body(Full::new(Bytes::from(cached.body))).unwrap()
}

fn build_upstream_response(
    resp: http::Response<Bytes>,
    mutations: &gateway_config::HeaderMutations,
) -> Response<Full<Bytes>> {
    let (mut parts, body) = resp.into_parts();

    // Apply response header mutations from route config
    for (k, v) in &mutations.response_set {
        if let (Ok(name), Ok(val)) = (
            http::header::HeaderName::from_bytes(k.as_bytes()),
            http::header::HeaderValue::from_str(v),
        ) {
            parts.headers.insert(name, val);
        }
    }

    parts.headers.insert("X-Cache", http::header::HeaderValue::from_static("MISS"));

    Response::from_parts(parts, Full::new(body))
}

fn apply_request_mutations(
    req: Request<Bytes>,
    mutations: &gateway_config::HeaderMutations,
) -> Request<Full<Bytes>> {
    let (mut parts, body) = req.into_parts();

    // Remove headers
    for name in &mutations.request_remove {
        parts.headers.remove(name.as_str());
    }

    // Set headers
    for (k, v) in &mutations.request_set {
        if let (Ok(name), Ok(val)) = (
            http::header::HeaderName::from_bytes(k.as_bytes()),
            http::header::HeaderValue::from_str(v),
        ) {
            parts.headers.insert(name, val);
        }
    }

    Request::from_parts(parts, Full::new(body))
}
