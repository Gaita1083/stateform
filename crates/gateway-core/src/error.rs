use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("tls error: {0}")]
    Tls(String),
    #[error("upstream error at {upstream}: {message}")]
    Upstream {
        upstream: String,
        message: String,
    },
    #[error("upstream timed out after {secs}s")]
    UpstreamTimeout { secs: u64 },
    #[error("no healthy endpoints for upstream `{0}`")]
    NoHealthyEndpoints(String),
    #[error("failed to bind listener on {addr}: {source}")]
    Bind {
        addr: String,
        source: std::io::Error,
    },
    #[error("hyper error: {0}")]
    Hyper(#[from] hyper::Error),
    #[error("http error: {0}")]
    Http(#[from] http::Error),
    #[error("metrics error: {0}")]
    Metrics(#[from] gateway_metrics::MetricsError),
    #[error("config error: {0}")]
    Config(gateway_config::ConfigError),
    #[error("shutdown timeout: in-flight requests did not drain in time")]
    DrainTimeout,
}
