//! Active health check background task.
//!
//! Spawns one independent Tokio task per endpoint that has a
//! [`HealthCheckConfig`]. Each task polls its endpoint on a fixed interval and
//! calls [`EndpointState::mark_failure`] / [`EndpointState::mark_success`] to
//! update the atomic health state.
//!
//! ## Design choices
//!
//! **One task per endpoint, not one task per upstream.**
//! Polling is I/O-bound — one task per endpoint keeps the fan-out parallelism
//! natural and avoids a single slow endpoint blocking health checks for others
//! in the same upstream.
//!
//! **`MissedTickBehavior::Skip`.**
//! If a health check takes longer than the configured interval (e.g. because the
//! upstream is timing out), the next tick is skipped rather than a burst of
//! checks firing immediately. Avoids pile-ups against an already-struggling upstream.
//!
//! **Passive + active checks.**
//! This module implements *active* checks (periodic HTTP probes). Passive checks
//! (marking failure based on real-traffic errors) are implemented in
//! `pipeline.rs` via the same `mark_failure` / `mark_success` API — both
//! mechanisms converge on the same atomic state.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Empty;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use tracing::{debug, info, warn};

use gateway_metrics::MetricsRecorder;

use crate::upstream::{EndpointState, UpstreamRegistry};

// ── Public entry point ────────────────────────────────────────────────────────

/// Spawn background health check tasks for every endpoint that has a
/// [`HealthCheckConfig`] configured.
///
/// Called once from [`GatewayServer::run`] after the upstream registry is built.
/// Tasks run for the lifetime of the process (no cancellation — Tokio cleans
/// them up on process exit).
pub fn spawn_health_checks(
    registry: Arc<UpstreamRegistry>,
    metrics:  MetricsRecorder,
) {
    // One shared HTTP client — lightweight, connection-pooled, no TLS needed
    // (health checks hit the upstream plain HTTP endpoint directly).
    let client: Arc<Client<HttpConnector, Empty<Bytes>>> = Arc::new(
        Client::builder(TokioExecutor::new()).build_http(),
    );

    let mut spawned = 0usize;

    for (upstream_name, pool) in registry.iter() {
        let Some(hc_cfg) = &pool.config.health_check else {
            continue; // No health check configured for this upstream
        };

        for endpoint in &pool.endpoints {
            let endpoint      = Arc::clone(endpoint);
            let client        = Arc::clone(&client);
            let hc_cfg        = hc_cfg.clone();
            let upstream_name = upstream_name.to_string();
            let metrics       = metrics.clone();

            tokio::spawn(health_check_loop(
                endpoint,
                client,
                upstream_name,
                hc_cfg,
                metrics,
            ));
            spawned += 1;
        }
    }

    if spawned > 0 {
        info!(tasks = spawned, "active health check tasks started");
    }
}

// ── Per-endpoint loop ─────────────────────────────────────────────────────────

async fn health_check_loop(
    endpoint:      Arc<EndpointState>,
    client:        Arc<Client<HttpConnector, Empty<Bytes>>>,
    upstream_name: String,
    cfg:           gateway_config::HealthCheckConfig,
    metrics:       MetricsRecorder,
) {
    let mut interval = tokio::time::interval(cfg.interval);
    // Skip missed ticks — don't pile up checks against a struggling upstream
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    info!(
        upstream = %upstream_name,
        address  = %endpoint.address,
        path     = %cfg.path,
        interval_secs = cfg.interval.as_secs(),
        "health check task started"
    );

    loop {
        interval.tick().await;

        let url = format!("http://{}{}", endpoint.address, cfg.path);

        let req = match hyper::Request::builder()
            .method("GET")
            .uri(&url)
            .header("User-Agent", "stateform-healthcheck/1.0")
            .body(Empty::<Bytes>::new())
        {
            Ok(r)  => r,
            Err(e) => {
                warn!(upstream = %upstream_name, address = %endpoint.address,
                      error = %e, "failed to build health check request");
                continue;
            }
        };

        let result = tokio::time::timeout(cfg.timeout, client.request(req)).await;

        let new_state = match result {
            // ── Success (2xx) ─────────────────────────────────────────────────
            Ok(Ok(resp)) if resp.status().is_success() => {
                debug!(
                    upstream = %upstream_name,
                    address  = %endpoint.address,
                    status   = resp.status().as_u16(),
                    "health check ok"
                );
                endpoint.mark_success(&cfg)
            }

            // ── Non-2xx response ──────────────────────────────────────────────
            Ok(Ok(resp)) => {
                warn!(
                    upstream = %upstream_name,
                    address  = %endpoint.address,
                    status   = resp.status().as_u16(),
                    "health check failed: non-2xx response"
                );
                endpoint.mark_failure(&cfg)
            }

            // ── Connection / protocol error ───────────────────────────────────
            Ok(Err(e)) => {
                warn!(
                    upstream = %upstream_name,
                    address  = %endpoint.address,
                    error    = %e,
                    "health check failed: connection error"
                );
                endpoint.mark_failure(&cfg)
            }

            // ── Timeout ───────────────────────────────────────────────────────
            Err(_elapsed) => {
                warn!(
                    upstream      = %upstream_name,
                    address       = %endpoint.address,
                    timeout_secs  = cfg.timeout.as_secs(),
                    "health check timed out"
                );
                endpoint.mark_failure(&cfg)
            }
        };

        // Emit metric so operators can alert on state changes.
        // Convert to f64 here (1.0/0.5/0.0) so gateway-metrics stays free of
        // a gateway-core dependency (which would create a circular dep cycle).
        metrics.record_endpoint_health(
            &upstream_name,
            &endpoint.address,
            new_state.as_gauge_value(),
        );
    }
}
