use std::fmt;

/// A fully-qualified, stable cache key for a request.
///
/// ## Structure
///
/// `{method}:{route_id}:{path_and_query}[:{vary_name}={vary_value}...]`
///
/// Examples:
/// ```
/// GET:api-v1:/users?page=2
/// GET:api-v1:/users?page=2:accept-encoding=gzip
/// ```
///
/// ## Vary handling
///
/// When the upstream response includes a `Vary` header, the values of those
/// request headers are appended to the key. This ensures that a `gzip`-encoded
/// response and a plain response for the same URL get separate cache entries.
///
/// The vary segment is sorted alphabetically so key derivation is
/// order-independent regardless of how headers arrive.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey(String);

impl CacheKey {
    /// Build a key from request components, without vary headers.
    ///
    /// Used on the first lookup (before we know the Vary header values from
    /// a cached entry). If a cached entry exists and specifies vary headers,
    /// call [`CacheKey::with_vary`] to re-derive the full key.
    pub fn new(
        method: &str,
        route_id: &str,
        path: &str,
        query: Option<&str>,
    ) -> Self {
        let path_query = match query {
            Some(q) if !q.is_empty() => format!("{path}?{q}"),
            _ => path.to_string(),
        };
        Self(format!(
            "stateform:cache:{}:{}:{}",
            method.to_ascii_uppercase(),
            route_id,
            path_query,
        ))
    }

    /// Extend the key with Vary header values from the request.
    ///
    /// `vary_names` comes from the stored cache entry's `vary_headers` field —
    /// the header names the upstream said matter for caching.
    pub fn with_vary(
        &self,
        vary_names: &[String],
        request_headers: &http::HeaderMap,
    ) -> Self {
        if vary_names.is_empty() {
            return self.clone();
        }

        // Collect (name, value) pairs, sorted by name for determinism
        let mut pairs: Vec<(String, String)> = vary_names
            .iter()
            .map(|name| {
                let value = request_headers
                    .get(name.as_str())
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                (name.clone(), value)
            })
            .collect();

        pairs.sort_by(|a, b| a.0.cmp(&b.0));

        let vary_suffix: String = pairs
            .iter()
            .map(|(k, v)| format!(":{k}={v}"))
            .collect();

        Self(format!("{}{vary_suffix}", self.0))
    }

    /// The raw string key, used as the Redis key and DashMap key.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ── Hop-by-hop header filter ──────────────────────────────────────────────────

/// Headers that must never be stored in the cache.
///
/// Hop-by-hop headers are connection-specific and meaningless outside the
/// original TCP connection. Storing them and replaying them on cache hits
/// would confuse HTTP clients.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// Filter a response's headers for safe storage in the cache.
///
/// Returns only headers that are safe to replay on cache hits.
pub fn storable_headers(headers: &http::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let name_str = name.as_str().to_ascii_lowercase();
            if HOP_BY_HOP.contains(&name_str.as_str()) {
                return None;
            }
            value.to_str().ok().map(|v| (name_str, v.to_string()))
        })
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_key_structure() {
        let key = CacheKey::new("GET", "api", "/users", Some("page=2"));
        assert_eq!(key.as_str(), "stateform:cache:GET:api:/users?page=2");
    }

    #[test]
    fn no_query_omits_question_mark() {
        let key = CacheKey::new("GET", "api", "/status", None);
        assert_eq!(key.as_str(), "stateform:cache:GET:api:/status");
    }

    #[test]
    fn vary_extends_key_sorted() {
        let base = CacheKey::new("GET", "api", "/data", None);
        let mut req_headers = http::HeaderMap::new();
        req_headers.insert("accept-encoding", "gzip".parse().unwrap());
        req_headers.insert("accept", "application/json".parse().unwrap());

        let vary_names = vec!["accept-encoding".to_string(), "accept".to_string()];
        let keyed = base.with_vary(&vary_names, &req_headers);

        // Sorted alphabetically: accept before accept-encoding
        assert!(keyed.as_str().contains(":accept=application/json"));
        assert!(keyed.as_str().contains(":accept-encoding=gzip"));
    }

    #[test]
    fn vary_missing_header_uses_empty_string() {
        let base = CacheKey::new("GET", "api", "/data", None);
        let vary_names = vec!["x-custom".to_string()];
        let keyed = base.with_vary(&vary_names, &http::HeaderMap::new());
        assert!(keyed.as_str().contains(":x-custom="));
    }

    #[test]
    fn storable_headers_strips_hop_by_hop() {
        let mut headers = http::HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("transfer-encoding", "chunked".parse().unwrap());
        headers.insert("connection", "keep-alive".parse().unwrap());

        let stored = storable_headers(&headers);
        assert!(stored.iter().any(|(k, _)| k == "content-type"));
        assert!(!stored.iter().any(|(k, _)| k == "transfer-encoding"));
        assert!(!stored.iter().any(|(k, _)| k == "connection"));
    }
}
