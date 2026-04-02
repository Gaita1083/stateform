use thiserror::Error;

#[derive(Debug, Error)]
pub enum RateLimitError {
    #[error("rate limit exceeded")]
    Exceeded,
    #[error("redis error: {0}")]
    Redis(String),
    #[error("lua script error: {0}")]
    Script(String),
    #[error("unknown rate-limit policy: {0}")]
    UnknownPolicy(String),
    #[error("config error: {0}")]
    Config(String),
}
