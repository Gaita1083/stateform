use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use prometheus::Encoder;
use tokio::net::TcpListener;
use tracing::{error, info};

use crate::{error::MetricsError, recorder::MetricsRecorder};

/// Serves the Prometheus `/metrics` scrape endpoint on a dedicated TCP port.
///
/// ## Why a separate server?
///
/// Prometheus scrapes are out-of-band management traffic. Serving them on the
/// same port as the proxy would mix scrape latency into gateway latency metrics
/// and make it harder to apply different network policies (e.g. only allow
/// scrapes from the monitoring VLAN).
///
/// The metrics server is a minimal Hyper 1.x listener that handles only:
/// - `GET /metrics` → 200 with Prometheus text encoding
/// - Everything else → 404
///
/// It runs as a background Tokio task and never affects the main request path.
pub struct MetricsServer {
    recorder: MetricsRecorder,
    bind:     String,
}

impl MetricsServer {
    pub fn new(recorder: MetricsRecorder, bind: impl Into<String>) -> Self {
        Self { recorder, bind: bind.into() }
    }

    /// Spawn the metrics server as a background task.
    ///
    /// Returns immediately; the server runs until the process exits.
    /// Bind errors are returned so the caller can decide whether to abort
    /// startup or continue without a metrics endpoint.
    pub async fn spawn(self) -> Result<(), MetricsError> {
        let addr: SocketAddr = self.bind.parse().map_err(|_| MetricsError::Bind {
            addr: self.bind.clone(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid metrics bind address",
            ),
        })?;

        let listener = TcpListener::bind(addr).await.map_err(|e| MetricsError::Bind {
            addr: self.bind.clone(),
            source: e,
        })?;

        info!(addr = %addr, "metrics server listening");

        let recorder = Arc::new(self.recorder);

        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        let recorder = Arc::clone(&recorder);
                        tokio::spawn(async move {
                            let io = TokioIo::new(stream);
                            let svc = MetricsService { recorder };

                            if let Err(e) = hyper::server::conn::http1::Builder::new()
                                .serve_connection(io, hyper::service::service_fn(move |req| {
                                    let svc = svc.clone();
                                    async move { svc.handle(req).await }
                                }))
                                .await
                            {
                                error!(peer = %peer, error = %e, "metrics connection error");
                            }
                        });
                    }
                    Err(e) => {
                        error!(error = %e, "metrics listener accept error");
                        // Brief backoff to avoid spinning on persistent errors
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        });

        Ok(())
    }
}

// ── Service ───────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct MetricsService {
    recorder: Arc<MetricsRecorder>,
}

impl MetricsService {
    async fn handle(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
        let resp = match (req.method(), req.uri().path()) {
            (&http::Method::GET, "/metrics") => self.render_metrics(),
            (&http::Method::GET, "/healthz") => self.health_check(),
            _ => not_found(),
        };
        Ok(resp)
    }

    fn render_metrics(&self) -> Response<Full<Bytes>> {
        let encoder = prometheus::TextEncoder::new();
        let families = self.recorder.registry().gather();

        let mut buf = Vec::with_capacity(4096);
        match encoder.encode(&families, &mut buf) {
            Ok(()) => Response::builder()
                .status(StatusCode::OK)
                .header(
                    http::header::CONTENT_TYPE,
                    encoder.format_type(),
                )
                .body(Full::new(Bytes::from(buf)))
                .unwrap(),
            Err(e) => {
                error!(error = %e, "failed to encode metrics");
                internal_error()
            }
        }
    }

    fn health_check(&self) -> Response<Full<Bytes>> {
        Response::builder()
            .status(StatusCode::OK)
            .header(http::header::CONTENT_TYPE, "text/plain")
            .body(Full::new(Bytes::from("ok")))
            .unwrap()
    }
}

// ── Static responses ──────────────────────────────────────────────────────────

fn not_found() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Full::new(Bytes::from("not found")))
        .unwrap()
}

fn internal_error() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Full::new(Bytes::from("internal error")))
        .unwrap()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The metrics service logic is tested by asserting the render path
    /// produces valid Prometheus text format. Network bind tests live in
    /// integration tests.
    #[tokio::test]
    async fn renders_prometheus_text() {
        use crate::registry::Metrics;

        let recorder = MetricsRecorder::new(Metrics::new().unwrap());
        recorder.record_request("route-1", "GET", 200,
            std::time::Duration::from_millis(5), 0, 64);

        let svc = MetricsService { recorder: Arc::new(recorder) };
        // handle() requires a live hyper Incoming body which can't be constructed
        // in a unit test — test the render path directly instead.
        let _req = Request::builder()
            .method("GET")
            .uri("/metrics")
            .body(http_body_util::Empty::<Bytes>::new())
            .unwrap();

        let resp = svc.render_metrics();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        let text = std::str::from_utf8(&body).unwrap();

        assert!(text.contains("stateform_requests_total"));
        assert!(text.contains("stateform_request_duration_seconds"));
    }

}
