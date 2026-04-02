use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("JWKS fetch failed: {0}")]
    JwksFetch(String),
    #[error("unknown key ID: {0}")]
    UnknownKid(String),
    #[error("invalid token: {0}")]
    InvalidToken(String),
    #[error("missing bearer token")]
    MissingToken,
    #[error("invalid API key")]
    InvalidApiKey,
    #[error("missing API key header")]
    MissingApiKey,
    #[error("API key store error: {0}")]
    ApiKeyStore(String),
    #[error("missing client certificate")]
    MissingClientCert,
    #[error("invalid client certificate: {0}")]
    InvalidClientCert(String),
    #[error("no auth provider enabled")]
    NoProviderEnabled,
    #[error("auth config error: {0}")]
    Config(String),
}

impl AuthError {
    /// HTTP status code appropriate for this error.
    pub fn status_code(&self) -> u16 {
        match self {
            // 401 — client must authenticate
            AuthError::MissingToken
            | AuthError::MissingApiKey
            | AuthError::MissingClientCert => 401,
            // 403 — credentials present but invalid
            AuthError::InvalidToken(_)
            | AuthError::InvalidApiKey
            | AuthError::UnknownKid(_)
            | AuthError::InvalidClientCert(_) => 403,
            // 503 — upstream / dependency failure
            AuthError::JwksFetch(_) | AuthError::ApiKeyStore(_) => 503,
            // 500 — internal / config
            _ => 500,
        }
    }

    /// Returns `true` for errors caused by bad/missing client input,
    /// `false` for server-side failures.
    pub fn is_client_error(&self) -> bool {
        self.status_code() < 500
    }
}
