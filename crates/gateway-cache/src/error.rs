use thiserror::Error;

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("redis error: {0}")]
    Redis(String),
    #[error("serialisation error: {0}")]
    Serialise(#[from] serde_json::Error),
    #[error("config error: {0}")]
    Config(String),
}
