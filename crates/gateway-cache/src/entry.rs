use std::time::Duration;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

/// A cached HTTP response stored in L1 (in-process) or L2 (Redis).
///
/// Serialisable so that it can be round-tripped through Redis as JSON.
/// The `stored_at_secs` field is used by L1 for soft-expiry checks; Redis
/// handles TTL expiry authoritatively for L2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedResponse {
    /// HTTP status code of the cached response.
    pub status: u16,
    /// Serialised response headers (name → value pairs).
    pub headers: Vec<(String, String)>,
    /// Raw response body bytes.
    #[serde(with = "serde_bytes")]
    pub body: Vec<u8>,
    /// Effective TTL in seconds (derived from Cache-Control / config).
    pub ttl_secs: u64,
    /// Unix timestamp (seconds) when this entry was stored.
    pub stored_at_secs: u64,
    /// Header names declared in the upstream `Vary` response header.
    /// Used to re-derive the full vary-keyed cache key on subsequent lookups.
    #[serde(default)]
    pub vary_headers: Vec<String>,
}

impl CachedResponse {
    /// Build a new entry from upstream response parts.
    ///
    /// `headers` should already be filtered for safe storage (hop-by-hop
    /// headers removed) — use [`crate::key::storable_headers`].
    pub fn new(
        status: u16,
        headers: Vec<(String, String)>,
        body: Bytes,
        ttl: Duration,
        vary_headers: Vec<String>,
    ) -> Self {
        let stored_at_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Self {
            status,
            headers,
            body: body.to_vec(),
            ttl_secs: ttl.as_secs(),
            stored_at_secs,
            vary_headers,
        }
    }

    /// Seconds since this entry was stored — used to generate the `Age` header.
    pub fn age_header(&self) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now.saturating_sub(self.stored_at_secs).to_string()
    }

    /// Returns true if the entry has exceeded its TTL.
    pub fn is_expired(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now.saturating_sub(self.stored_at_secs) >= self.ttl_secs
    }
}
