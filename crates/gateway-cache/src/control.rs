use std::time::Duration;

/// Parsed directives from a `Cache-Control` header.
///
/// We parse only the directives that affect gateway caching behaviour.
/// Unknown directives are ignored — forwards-compatible.
#[derive(Debug, Default, Clone)]
pub struct CacheControl {
    /// `no-store` — must not cache at all
    pub no_store: bool,
    /// `no-cache` — may cache but must revalidate; we treat as no-store
    pub no_cache: bool,
    /// `private` — must not store in shared (L2 Redis) cache
    pub private: bool,
    /// `max-age=N` — client/response freshness in seconds
    pub max_age: Option<u64>,
    /// `s-maxage=N` — shared cache freshness; overrides max-age for L2
    pub s_maxage: Option<u64>,
}

impl CacheControl {
    /// Parse a `Cache-Control` header value into directives.
    pub fn parse(value: &str) -> Self {
        let mut cc = Self::default();

        for directive in value.split(',') {
            let directive = directive.trim().to_ascii_lowercase();
            match directive.as_str() {
                "no-store"  => cc.no_store = true,
                "no-cache"  => cc.no_cache = true,
                "private"   => cc.private = true,
                "public"    => {} // explicit public — no special action needed
                _ => {
                    if let Some(secs) = parse_delta_seconds(&directive, "max-age=") {
                        cc.max_age = Some(secs);
                    } else if let Some(secs) = parse_delta_seconds(&directive, "s-maxage=") {
                        cc.s_maxage = Some(secs);
                    }
                }
            }
        }

        cc
    }

    /// Whether this response/request must not be stored.
    #[inline]
    pub fn is_uncacheable(&self) -> bool {
        self.no_store || self.no_cache
    }

    /// Effective TTL for the shared (L2) cache.
    /// `s-maxage` takes precedence over `max-age` per RFC 7234 §5.2.2.9.
    pub fn shared_ttl(&self) -> Option<Duration> {
        self.s_maxage
            .or(self.max_age)
            .map(Duration::from_secs)
    }

    /// Effective TTL for a private (L1 only) cache.
    pub fn private_ttl(&self) -> Option<Duration> {
        self.max_age.map(Duration::from_secs)
    }
}

fn parse_delta_seconds(directive: &str, prefix: &str) -> Option<u64> {
    directive
        .strip_prefix(prefix)
        .and_then(|v| v.parse::<u64>().ok())
}

/// Extract and parse `Cache-Control` from a header map.
/// Returns `None` if the header is absent or unparseable.
pub fn from_headers(headers: &http::HeaderMap) -> Option<CacheControl> {
    headers
        .get(http::header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .map(CacheControl::parse)
}

/// Determine the effective TTL to use when storing a response.
///
/// Priority:
/// 1. `no-store` / `no-cache` on the response → don't cache
/// 2. `s-maxage` on the response (shared cache)
/// 3. `max-age` on the response
/// 4. Per-route configured TTL (from [`CacheMiddlewareConfig`])
/// 5. Don't cache (None)
pub fn resolve_ttl(
    response_cc: Option<&CacheControl>,
    route_ttl: Duration,
) -> Option<Duration> {
    if let Some(cc) = response_cc {
        if cc.is_uncacheable() {
            return None;
        }
        // Take the most restrictive of upstream directive and route config
        if let Some(upstream_ttl) = cc.shared_ttl() {
            return Some(upstream_ttl.min(route_ttl));
        }
    }
    Some(route_ttl)
}

/// Extract the `Vary` header and return the list of header names.
/// An empty list means the response doesn't vary — one cache entry for all.
/// A `Vary: *` means the response varies on everything — treat as uncacheable.
pub fn parse_vary(headers: &http::HeaderMap) -> Option<Vec<String>> {
    let vary = headers
        .get(http::header::VARY)
        .and_then(|v| v.to_str().ok())?;

    if vary.trim() == "*" {
        // Varies on everything — not safely cacheable
        return None;
    }

    Some(
        vary.split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_no_store() {
        let cc = CacheControl::parse("no-store");
        assert!(cc.no_store);
        assert!(cc.is_uncacheable());
    }

    #[test]
    fn parses_max_age() {
        let cc = CacheControl::parse("public, max-age=300");
        assert_eq!(cc.max_age, Some(300));
        assert!(!cc.is_uncacheable());
    }

    #[test]
    fn s_maxage_takes_precedence() {
        let cc = CacheControl::parse("max-age=300, s-maxage=60");
        assert_eq!(cc.shared_ttl(), Some(Duration::from_secs(60)));
    }

    #[test]
    fn resolve_ttl_respects_no_store() {
        let cc = CacheControl::parse("no-store");
        let result = resolve_ttl(Some(&cc), Duration::from_secs(30));
        assert!(result.is_none());
    }

    #[test]
    fn resolve_ttl_takes_min_of_upstream_and_route() {
        let cc = CacheControl::parse("max-age=10");
        let result = resolve_ttl(Some(&cc), Duration::from_secs(30));
        assert_eq!(result, Some(Duration::from_secs(10)));
    }

    #[test]
    fn resolve_ttl_uses_route_when_no_directive() {
        let result = resolve_ttl(None, Duration::from_secs(30));
        assert_eq!(result, Some(Duration::from_secs(30)));
    }

    #[test]
    fn vary_star_returns_none() {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::VARY, "*".parse().unwrap());
        assert!(parse_vary(&headers).is_none());
    }

    #[test]
    fn vary_parses_header_names() {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::VARY, "Accept-Encoding, Accept".parse().unwrap());
        let vary = parse_vary(&headers).unwrap();
        assert_eq!(vary, vec!["accept-encoding", "accept"]);
    }
}
