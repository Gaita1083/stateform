use http::{HeaderMap, Method};
use std::collections::HashMap;

/// Evaluate per-route method and header predicates against a live request.
///
/// Called after the trie narrows candidates to a small set. At this point
/// we're comparing strings against strings — no allocations unless a header
/// value needs to be decoded.
pub fn matches_predicates(
    route_method: Option<&str>,
    route_headers: &HashMap<String, String>,
    req_method: &Method,
    req_headers: &HeaderMap,
) -> bool {
    if !method_matches(route_method, req_method) {
        return false;
    }
    if !headers_match(route_headers, req_headers) {
        return false;
    }
    true
}

/// `None` means any method is accepted.
fn method_matches(route_method: Option<&str>, req_method: &Method) -> bool {
    match route_method {
        None => true,
        Some(m) => m.eq_ignore_ascii_case(req_method.as_str()),
    }
}

/// All declared route headers must be present on the request and their values
/// must match (treated as a regex for flexibility).
fn headers_match(route_headers: &HashMap<String, String>, req_headers: &HeaderMap) -> bool {
    for (name, pattern) in route_headers {
        let Some(value) = req_headers.get(name.as_str()) else {
            return false;
        };
        let Ok(value_str) = value.to_str() else {
            return false;
        };
        // Patterns are pre-validated during config load, so this won't panic.
        // We use a simple contains check for the common case, and compile the
        // pattern only when it contains regex metacharacters.
        if !value_matches(value_str, pattern) {
            return false;
        }
    }
    true
}

/// Fast path: if the pattern has no metacharacters use a plain string check.
/// Otherwise compile it. The results aren't cached here because these patterns
/// are pre-compiled in the route's compiled form — see `CompiledRoute`.
fn value_matches(value: &str, pattern: &str) -> bool {
    if is_plain_string(pattern) {
        value.contains(pattern)
    } else {
        regex::Regex::new(pattern)
            .map(|re| re.is_match(value))
            .unwrap_or(false)
    }
}

/// Heuristic: if the pattern contains no regex metacharacters treat it as
/// a literal substring match — much faster than compiling a regex.
fn is_plain_string(s: &str) -> bool {
    !s.chars().any(|c| matches!(c, '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Method;

    #[test]
    fn any_method_when_none() {
        let headers = HashMap::new();
        let req_headers = HeaderMap::new();
        assert!(matches_predicates(None, &headers, &Method::POST, &req_headers));
        assert!(matches_predicates(None, &headers, &Method::DELETE, &req_headers));
    }

    #[test]
    fn method_mismatch_fails() {
        let headers = HashMap::new();
        let req_headers = HeaderMap::new();
        assert!(!matches_predicates(Some("GET"), &headers, &Method::POST, &req_headers));
    }

    #[test]
    fn method_case_insensitive() {
        let headers = HashMap::new();
        let req_headers = HeaderMap::new();
        assert!(matches_predicates(Some("get"), &headers, &Method::GET, &req_headers));
    }

    #[test]
    fn missing_required_header_fails() {
        let mut route_headers = HashMap::new();
        route_headers.insert("X-Service-Name".into(), ".*".into());
        let req_headers = HeaderMap::new();
        assert!(!matches_predicates(None, &route_headers, &Method::GET, &req_headers));
    }

    #[test]
    fn matching_header_passes() {
        let mut route_headers = HashMap::new();
        route_headers.insert("X-Service-Name".into(), "billing".into());
        let mut req_headers = HeaderMap::new();
        req_headers.insert("x-service-name", "billing-service".parse().unwrap());
        assert!(matches_predicates(None, &route_headers, &Method::GET, &req_headers));
    }
}
