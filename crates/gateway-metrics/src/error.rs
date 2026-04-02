use thiserror::Error;

#[derive(Debug, Error)]
pub enum MetricsError {
    #[error("prometheus error: {0}")]
    Prometheus(#[from] prometheus::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("metrics server error: {0}")]
    Server(String),
    #[error("failed to bind metrics listener on {addr}: {source}")]
    Bind {
        addr: String,
        source: std::io::Error,
    },
    #[error("failed to register metric `{name}`: {source}")]
    Register {
        name: &'static str,
        source: prometheus::Error,
    },
}
