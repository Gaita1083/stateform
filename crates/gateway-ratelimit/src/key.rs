use gateway_auth::AuthContext;

/// Derive a stable, opaque rate-limit key for a request.
///
/// The key is `{route_id}:{identity}` where identity resolves (in order):
///
/// 1. `AuthContext::subject` — per-identity limit (most precise)
/// 2. `X-Forwarded-For` first hop — per-IP for public routes behind a proxy
/// 3. `X-Real-IP` — common single-IP proxy header
/// 4. `"anonymous"` — absolute fallback (rare; most deployments have at least a proxy)
///
/// Namespacing by route ID means the same identity gets independent buckets on
/// different routes — a heavy /search user doesn't eat into their /upload quota.
pub fn derive_key(
    route_id: &str,
    auth_ctx: Option<&AuthContext>,
    headers: &http::HeaderMap,
) -> String {
    let identity = auth_ctx
        .map(|ctx| ctx.subject.as_str())
        .or_else(|| extract_forwarded_for(headers))
        .or_else(|| extract_real_ip(headers))
        .unwrap_or("anonymous");

    format!("stateform:rl:{route_id}:{identity}")
}

fn extract_forwarded_for<'h>(headers: &'h http::HeaderMap) -> Option<&'h str> {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        // XFF is comma-separated; take the first (original client) IP
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn extract_real_ip<'h>(headers: &'h http::HeaderMap) -> Option<&'h str> {
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_subject_when_authed() {
        let ctx = AuthContext {
            subject: "user-42".into(),
            mechanisms: vec![],
            claims: None,
            peer_identity: None,
        };
        let key = derive_key("api", Some(&ctx), &http::HeaderMap::new());
        assert_eq!(key, "stateform:rl:api:user-42");
    }

    #[test]
    fn falls_back_to_xff() {
        let mut headers = http::HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4, 10.0.0.1".parse().unwrap());
        let key = derive_key("api", None, &headers);
        assert_eq!(key, "stateform:rl:api:1.2.3.4");
    }

    #[test]
    fn falls_back_to_real_ip() {
        let mut headers = http::HeaderMap::new();
        headers.insert("x-real-ip", "5.6.7.8".parse().unwrap());
        let key = derive_key("api", None, &headers);
        assert_eq!(key, "stateform:rl:api:5.6.7.8");
    }

    #[test]
    fn anonymous_when_no_identity() {
        let key = derive_key("chat", None, &http::HeaderMap::new());
        assert_eq!(key, "stateform:rl:chat:anonymous");
    }

    #[test]
    fn route_id_namespaces_key() {
        let ctx = AuthContext {
            subject: "user-1".into(),
            mechanisms: vec![],
            claims: None,
            peer_identity: None,
        };
        let k1 = derive_key("search", Some(&ctx), &http::HeaderMap::new());
        let k2 = derive_key("upload", Some(&ctx), &http::HeaderMap::new());
        assert_ne!(k1, k2);
    }
}
