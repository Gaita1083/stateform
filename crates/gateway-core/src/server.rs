use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http::StatusCode;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info, warn};
use uuid::Uuid;

use gateway_auth::AuthPipeline;
use gateway_cache::ResponseCache;
use gateway_config::ConfigWatcher;
use gateway_metrics::{MetricsRecorder, MetricsServer};
use gateway_ratelimit::RateLimiter;
use gateway_router::RouterHandle;

use crate::{
    concurrency::ConcurrencyLimiter,
    error::CoreError,
    health::spawn_health_checks,
    pipeline::Pipeline,
    shutdown::{DrainGuard, ShutdownCoordinator, ShutdownSignal},
    tls,
    upstream::UpstreamRegistry,
};

// ── GatewayServer ─────────────────────────────────────────────────────────────

pub struct GatewayServer {
    config:   Arc<ConfigWatcher>,
    pipeline: Pipeline,
    metrics:  MetricsRecorder,
    shutdown: Arc<ShutdownCoordinator>,
}

impl GatewayServer {
    pub async fn build(config: Arc<ConfigWatcher>) -> Result<Self, CoreError> {
        let cfg = config.current();

        let metrics_inner = gateway_metrics::registry::Metrics::new()?;
        let metrics       = MetricsRecorder::new(metrics_inner);

        let router = RouterHandle::new(&cfg)
            .map_err(|e| CoreError::Config(gateway_config::ConfigError::Validation(e.to_string())))?;
        let router = Arc::new(router);

        let auth = AuthPipeline::new(&cfg.auth)
            .await
            .map_err(|e| CoreError::Config(
                gateway_config::ConfigError::Validation(e.to_string())
            ))?;
        let auth = Arc::new(auth);

        let ratelimit = RateLimiter::new(&cfg)
            .await
            .map_err(|e| CoreError::Config(
                gateway_config::ConfigError::Validation(e.to_string())
            ))?;
        let ratelimit = Arc::new(ratelimit);

        let cache = ResponseCache::new(&cfg.cache)
            .await
            .map_err(|e| CoreError::Config(
                gateway_config::ConfigError::Validation(e.to_string())
            ))?;
        let cache = Arc::new(cache);

        let upstreams    = Arc::new(UpstreamRegistry::build(&cfg));
        let concurrency  = Arc::new(ConcurrencyLimiter::build(&cfg));
        let max_body_bytes = cfg.listener.max_body_bytes;

        let pipeline = Pipeline {
            router,
            auth,
            ratelimit,
            cache,
            metrics: metrics.clone(),
            upstreams,
            concurrency,
            max_body_bytes,
        };

        Ok(Self {
            config,
            pipeline,
            metrics,
            shutdown: Arc::new(ShutdownCoordinator::new()),
        })
    }

    pub async fn run(self) -> Result<(), CoreError> {
        let cfg = self.config.current();

        // ── Metrics server ────────────────────────────────────────────────────
        MetricsServer::new(self.metrics.clone(), &cfg.observability.metrics_bind)
            .spawn()
            .await?;

        // ── Active health checks ──────────────────────────────────────────────
        // Spawns one background task per endpoint that has health_check configured.
        // Tasks update EndpointState.health atomically; routing immediately reacts.
        spawn_health_checks(Arc::clone(&self.pipeline.upstreams), self.metrics.clone());

        // ── Config hot-reload watcher ─────────────────────────────────────────
        let mut reload_rx    = self.config.subscribe();
        let reload_router    = Arc::clone(&self.pipeline.router);
        let reload_metrics   = self.metrics.clone();
        tokio::spawn(async move {
            loop {
                if reload_rx.changed().await.is_err() { break; }
                let new_cfg = reload_rx.borrow_and_update().clone();
                match reload_router.rebuild(&new_cfg) {
                    Ok(()) => {
                        reload_metrics.record_config_reload_success();
                        info!("router hot-reloaded");
                    }
                    Err(e) => {
                        reload_metrics.record_config_reload_failure();
                        error!(error = %e, "router hot-reload failed — keeping current routes");
                    }
                }
            }
        });

        // ── OS signal handler ─────────────────────────────────────────────────
        let shutdown_signal = self.shutdown.signal();
        let shutdown_arc    = Arc::clone(&self.shutdown);
        tokio::spawn(async move {
            wait_for_os_signal().await;
            shutdown_arc.shutdown().await;
        });

        // ── TCP listener ──────────────────────────────────────────────────────
        let bind     = &cfg.listener.bind;
        let listener = TcpListener::bind(bind)
            .await
            .map_err(|e| CoreError::Bind { addr: bind.clone(), source: e })?;

        info!(addr = %bind, "gateway listening");

        let tls_acceptor = cfg.listener.tls.as_ref()
            .map(|tls_cfg| tls::build_acceptor(tls_cfg))
            .transpose()?;

        self.accept_loop(listener, tls_acceptor, shutdown_signal).await;
        Ok(())
    }

    async fn accept_loop(
        &self,
        listener:     TcpListener,
        tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
        mut shutdown: ShutdownSignal,
    ) {
        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, peer)) => {
                            let pipeline      = self.pipeline.clone();
                            let drain_guard   = self.shutdown.acquire();
                            let tls           = tls_acceptor.clone();
                            self.metrics.connection_open();
                            let metrics_close = self.metrics.clone();

                            tokio::spawn(async move {
                                serve_connection(stream, peer, pipeline, tls, drain_guard).await;
                                metrics_close.connection_close();
                            });
                        }
                        Err(e) => {
                            error!(error = %e, "accept error");
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        }
                    }
                }
                _ = shutdown.wait() => {
                    info!("accept loop stopping");
                    break;
                }
            }
        }
    }
}

// ── Connection handler ────────────────────────────────────────────────────────

async fn serve_connection(
    stream:   TcpStream,
    peer:     SocketAddr,
    pipeline: Pipeline,
    tls:      Option<tokio_rustls::TlsAcceptor>,
    _drain:   DrainGuard,
) {
    match tls {
        Some(acceptor) => {
            match acceptor.accept(stream).await {
                Ok(tls_stream) => serve_http(TokioIo::new(tls_stream), peer, pipeline).await,
                Err(e)         => warn!(peer = %peer, error = %e, "TLS handshake failed"),
            }
        }
        None => serve_http(TokioIo::new(stream), peer, pipeline).await,
    }
}

async fn serve_http<IO>(io: IO, peer: SocketAddr, pipeline: Pipeline)
where
    IO: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let svc = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
        let pipeline = pipeline.clone();
        async move {
            let (mut parts, body) = req.into_parts();

            // ── Body size enforcement ─────────────────────────────────────────
            //
            // Two-phase check:
            // Phase 1: Content-Length fast path — reject before reading a byte.
            //          Clients sending large uploads don't need to stream the whole
            //          body before getting a 413; they get it at headers.
            // Phase 2: Streaming cap via http_body_util::Limited — catches bodies
            //          without Content-Length (chunked transfer-encoding) or those
            //          that lie about their Content-Length.
            let max = pipeline.max_body_bytes;

            if let Some(len) = parts.headers
                .get(http::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<usize>().ok())
            {
                if len > max {
                    warn!(
                        peer = %peer,
                        content_length = len,
                        max,
                        "request body exceeds limit (Content-Length check)"
                    );
                    return Ok::<_, std::convert::Infallible>(payload_too_large());
                }
            }

            let body_bytes = match Limited::new(body, max).collect().await {
                Ok(collected) => collected.to_bytes(),
                Err(e) => {
                    let msg = e.to_string().to_ascii_lowercase();
                    if msg.contains("length limit") || msg.contains("limit exceeded") {
                        warn!(peer = %peer, max, "request body exceeded streaming limit");
                        return Ok(payload_too_large());
                    }
                    warn!(peer = %peer, error = %msg, "failed to read request body");
                    Bytes::new()
                }
            };

            // ── X-Request-ID injection ────────────────────────────────────────
            //
            // Generate or preserve. Done here (not in pipeline.rs) so the ID is
            // available for the tracing span before pipeline::handle() is called.
            if !parts.headers.contains_key("x-request-id") {
                let id = Uuid::new_v4().to_string();
                if let Ok(val) = http::header::HeaderValue::from_str(&id) {
                    parts.headers.insert("x-request-id", val);
                }
            }

            let owned_req = hyper::Request::from_parts(parts, body_bytes);
            let resp      = crate::pipeline::handle(&pipeline, owned_req).await;
            Ok(resp)
        }
    });

    if let Err(e) = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
        .serve_connection(io, svc)
        .await
    {
        if !is_benign_conn_error(e.as_ref()) {
            error!(peer = %peer, error = %e, "connection error");
        }
    }
}

// ── Static responses ──────────────────────────────────────────────────────────

fn payload_too_large() -> hyper::Response<Full<Bytes>> {
    let body = serde_json::json!({ "error": "request body too large" }).to_string();
    hyper::Response::builder()
        .status(StatusCode::PAYLOAD_TOO_LARGE)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

fn is_benign_conn_error(e: &dyn std::error::Error) -> bool {
    let s = e.to_string().to_ascii_lowercase();
    s.contains("connection reset")
        || s.contains("broken pipe")
        || s.contains("connection closed")
        || s.contains("eof")
}

// ── OS signal handler ─────────────────────────────────────────────────────────

async fn wait_for_os_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut sigint  = signal(SignalKind::interrupt()).expect("SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => info!("SIGTERM received"),
            _ = sigint.recv()  => info!("SIGINT received"),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.expect("Ctrl-C handler");
        info!("Ctrl-C received");
    }
}
