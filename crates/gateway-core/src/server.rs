use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info, warn};

use gateway_auth::AuthPipeline;
use gateway_cache::ResponseCache;
use gateway_config::ConfigWatcher;
use gateway_metrics::{MetricsRecorder, MetricsServer};
use gateway_ratelimit::RateLimiter;
use gateway_router::RouterHandle;

use crate::{
    error::CoreError,
    pipeline::Pipeline,
    shutdown::{DrainGuard, ShutdownCoordinator, ShutdownSignal},
    tls,
    upstream::UpstreamRegistry,
};

// ── GatewayServer ─────────────────────────────────────────────────────────────

/// The top-level server. Owns the accept loop, all shared state, and
/// coordinates startup and graceful shutdown.
pub struct GatewayServer {
    config:   Arc<ConfigWatcher>,
    pipeline: Pipeline,
    metrics:  MetricsRecorder,
    shutdown: Arc<ShutdownCoordinator>,
}

impl GatewayServer {
    /// Build the complete server from a config watcher.
    ///
    /// Establishes all Redis connections, builds router and auth pipeline,
    /// and constructs the shared pipeline state. Does not bind any sockets.
    pub async fn build(config: Arc<ConfigWatcher>) -> Result<Self, CoreError> {
        let cfg = config.current();

        // Registry of Prometheus metrics (private registry — no global state)
        let metrics_inner = gateway_metrics::registry::Metrics::new()?;
        let metrics       = MetricsRecorder::new(metrics_inner);

        // Router (hot-reloadable via ArcSwap)
        let router = RouterHandle::new(&cfg)
            .map_err(|e| CoreError::Config(gateway_config::ConfigError::Validation(e.to_string())))?;
        let router = Arc::new(router);

        // Auth pipeline (establishes Redis connection for API key lookups)
        let auth = AuthPipeline::new(&cfg.auth)
            .await
            .map_err(|e| CoreError::Config(
                gateway_config::ConfigError::Validation(e.to_string())
            ))?;
        let auth = Arc::new(auth);

        // Rate limiter (establishes Redis connections per policy)
        let ratelimit = RateLimiter::new(&cfg)
            .await
            .map_err(|e| CoreError::Config(
                gateway_config::ConfigError::Validation(e.to_string())
            ))?;
        let ratelimit = Arc::new(ratelimit);

        // Response cache (establishes Redis connection)
        let cache = ResponseCache::new(&cfg.cache)
            .await
            .map_err(|e| CoreError::Config(
                gateway_config::ConfigError::Validation(e.to_string())
            ))?;
        let cache = Arc::new(cache);

        // Upstream pools (HTTP connection pools per upstream)
        let upstreams = Arc::new(UpstreamRegistry::build(&cfg));

        let pipeline = Pipeline {
            router,
            auth,
            ratelimit,
            cache,
            metrics: metrics.clone(),
            upstreams,
        };

        Ok(Self {
            config,
            pipeline,
            metrics,
            shutdown: Arc::new(ShutdownCoordinator::new()),
        })
    }

    /// Start the gateway — binds the listener, spawns the metrics server,
    /// and runs the accept loop until shutdown is signalled.
    pub async fn run(self) -> Result<(), CoreError> {
        let cfg = self.config.current();

        // ── Metrics server (separate port) ────────────────────────────────────
        MetricsServer::new(
            self.metrics.clone(),
            &cfg.observability.metrics_bind,
        )
        .spawn()
        .await?;

        // ── Hot-reload watcher ────────────────────────────────────────────────
        let mut reload_rx = self.config.subscribe();
        let reload_router = Arc::clone(&self.pipeline.router);
        let reload_metrics = self.metrics.clone();
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
        let shutdown_arc = Arc::clone(&self.shutdown);
        tokio::spawn(async move {
            wait_for_os_signal().await;
            shutdown_arc.shutdown().await;
        });

        // ── TCP listener ──────────────────────────────────────────────────────
        let bind = &cfg.listener.bind;
        let listener = TcpListener::bind(bind)
            .await
            .map_err(|e| CoreError::Bind { addr: bind.clone(), source: e })?;

        info!(addr = %bind, "gateway listening");

        // ── Accept loop ───────────────────────────────────────────────────────
        let tls_acceptor = cfg.listener.tls.as_ref()
            .map(|tls_cfg| tls::build_acceptor(tls_cfg))
            .transpose()?;

        self.accept_loop(listener, tls_acceptor, shutdown_signal).await;
        Ok(())
    }

    async fn accept_loop(
        &self,
        listener: TcpListener,
        tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
        mut shutdown: ShutdownSignal,
    ) {
        loop {
            tokio::select! {
                // Accept new connections
                result = listener.accept() => {
                    match result {
                        Ok((stream, peer)) => {
                            let pipeline    = self.pipeline.clone();
                            let drain_guard = self.shutdown.acquire();
                            let tls         = tls_acceptor.clone();
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
                // Shutdown signal received
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
    stream: TcpStream,
    peer: SocketAddr,
    pipeline: Pipeline,
    tls: Option<tokio_rustls::TlsAcceptor>,
    _drain: DrainGuard, // held for lifetime of connection
) {
    match tls {
        Some(acceptor) => {
            match acceptor.accept(stream).await {
                Ok(tls_stream) => {
                    let io = TokioIo::new(tls_stream);
                    serve_http(io, peer, pipeline).await;
                }
                Err(e) => warn!(peer = %peer, error = %e, "TLS handshake failed"),
            }
        }
        None => {
            let io = TokioIo::new(stream);
            serve_http(io, peer, pipeline).await;
        }
    }
}

async fn serve_http<IO>(io: IO, peer: SocketAddr, pipeline: Pipeline)
where
    IO: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let svc = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
        let pipeline = pipeline.clone();
        async move {
            // Collect the incoming body into Bytes so it's owned and cloneable
            let (parts, body) = req.into_parts();
            let body_bytes = match body.collect().await {
                Ok(c) => c.to_bytes(),
                Err(e) => {
                    warn!(peer = %peer, error = %e, "failed to read request body");
                    Bytes::new()
                }
            };
            let owned_req = hyper::Request::from_parts(parts, body_bytes);
            let resp = crate::pipeline::handle(&pipeline, owned_req).await;
            Ok::<_, std::convert::Infallible>(resp)
        }
    });

    if let Err(e) = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
        .serve_connection(io, svc)
        .await
    {
        // Connection reset / client disconnect are expected — log at debug
        if !is_benign_conn_error(e.as_ref()) {
            error!(peer = %peer, error = %e, "connection error");
        }
    }
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
