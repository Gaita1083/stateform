use thiserror::Error;

#[derive(Debug, Error)]
pub enum RouterError {
    #[error("no route matched: {0}")]
    NoMatch(String),
    #[error("invalid regex pattern `{pattern}`: {source}")]
    InvalidRegex {
        pattern: String,
        #[source]
        source: regex::Error,
    },
    #[error("route config error: {0}")]
    Config(String),
}
