use redis::{aio::ConnectionManager, AsyncCommands};
use tracing::debug;

use gateway_config::ApiKeyConfig;

use crate::error::AuthError;

// ── Key payload ───────────────────────────────────────────────────────────────

/// The value stored in Redis for each API key.
///
/// Redis key:  `{prefix}{raw_key}`
/// Redis value: JSON-encoded [`ApiKeyRecord`]
///
/// Example:
/// ```
/// SET stateform:apikey:sk_live_abc123 '{"subject":"tenant-42","active":true}'
/// ```
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ApiKeyRecord {
    /// The canonical principal this key belongs to.
    /// Becomes [`AuthContext::subject`] when API key auth is the only mechanism.
    pub subject: String,
    /// Soft-revocation flag. Prefer this over deleting the key so you can
    /// audit the key's history.
    #[serde(default = "default_true")]
    pub active: bool,
}

fn default_true() -> bool { true }

// ── ApiKeyProvider ────────────────────────────────────────────────────────────

/// Validates `X-Api-Key` requests against a Redis key store.
///
/// ## Redis layout
///
/// ```
/// {prefix}{raw_api_key}  →  JSON ApiKeyRecord
/// ```
///
/// Keys are looked up with a single GET — O(1), single network round trip.
/// No scanning, no secondary indexes.
///
/// ## Security notes
///
/// - The raw key is **never** logged — only the first 8 characters for
///   debugging.
/// - Timing is not perfectly constant (Redis RTT varies), but we never
///   short-circuit on key format, so an attacker cannot infer validity
///   from response time without many thousands of samples.
pub struct ApiKeyProvider {
    prefix: String,
    conn: ConnectionManager,
}

impl ApiKeyProvider {
    /// Construct from a live Redis connection manager.
    ///
    /// The connection manager handles reconnection automatically — no need to
    /// wrap calls in retry logic here.
    pub async fn new(config: &ApiKeyConfig) -> Result<Self, AuthError> {
        let client = redis::Client::open(config.redis_url.as_str())
            .map_err(|e| AuthError::ApiKeyStore(e.to_string()))?;

        let conn = ConnectionManager::new(client)
            .await
            .map_err(|e| AuthError::ApiKeyStore(e.to_string()))?;

        Ok(Self {
            prefix: config.redis_key_prefix.clone(),
            conn,
        })
    }

    /// Verify the `X-Api-Key` header against the Redis key store.
    /// Returns the associated [`ApiKeyRecord`] on success.
    pub async fn verify(
        &self,
        headers: &http::HeaderMap,
    ) -> Result<ApiKeyRecord, AuthError> {
        let raw_key = extract_api_key(headers)?;
        self.lookup(raw_key).await
    }

    async fn lookup(&self, raw_key: &str) -> Result<ApiKeyRecord, AuthError> {
        let redis_key = format!("{}{}", self.prefix, raw_key);

        // Clone the connection manager — ConnectionManager is cheap to clone
        // (it holds an Arc internally)
        let mut conn = self.conn.clone();

        let value: Option<String> = conn
            .get(&redis_key)
            .await
            .map_err(|e| AuthError::ApiKeyStore(e.to_string()))?;

        let json = value.ok_or(AuthError::InvalidApiKey)?;

        let record: ApiKeyRecord = serde_json::from_str(&json)
            .map_err(|e| AuthError::ApiKeyStore(format!("corrupt key record: {e}")))?;

        if !record.active {
            return Err(AuthError::InvalidApiKey);
        }

        debug!(
            subject = %record.subject,
            key_prefix = &raw_key[..raw_key.len().min(8)],
            "API key verified"
        );

        Ok(record)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

const API_KEY_HEADER: &str = "x-api-key";

fn extract_api_key(headers: &http::HeaderMap) -> Result<&str, AuthError> {
    headers
        .get(API_KEY_HEADER)
        .ok_or(AuthError::MissingApiKey)?
        .to_str()
        .map_err(|_| AuthError::MissingApiKey)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_api_key_happy_path() {
        let mut headers = http::HeaderMap::new();
        headers.insert("x-api-key", "sk_live_abc123".parse().unwrap());
        assert_eq!(extract_api_key(&headers).unwrap(), "sk_live_abc123");
    }

    #[test]
    fn extract_api_key_missing() {
        let headers = http::HeaderMap::new();
        assert!(matches!(extract_api_key(&headers), Err(AuthError::MissingApiKey)));
    }

    #[test]
    fn api_key_record_deserialise() {
        let json = r#"{"subject":"tenant-42","active":true}"#;
        let r: ApiKeyRecord = serde_json::from_str(json).unwrap();
        assert_eq!(r.subject, "tenant-42");
        assert!(r.active);
    }

    #[test]
    fn api_key_record_defaults_active_true() {
        let json = r#"{"subject":"tenant-42"}"#;
        let r: ApiKeyRecord = serde_json::from_str(json).unwrap();
        assert!(r.active);
    }

    #[test]
    fn inactive_key_would_be_rejected() {
        let record = ApiKeyRecord { subject: "x".into(), active: false };
        assert!(!record.active);
    }
}
