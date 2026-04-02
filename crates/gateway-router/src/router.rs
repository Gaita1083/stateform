use std::sync::Arc;

use arc_swap::ArcSwap;
use http::{HeaderMap, Method};
use regex::Regex;
use tracing::trace;

use gateway_config::{GatewayConfig, PathMatch, RouteConfig};

use crate::{
    error::RouterError,
    matched::MatchedRoute,
    predicate::matches_predicates,
    trie::TrieNode,
};

// ── CompiledRoute ─────────────────────────────────────────────────────────────

/// A route with its regex pre-compiled.
///
/// Regex compilation happens once at router build time. The hot path only
/// calls `Regex::captures`, never `Regex::new`.
struct CompiledRoute {
    route: Arc<RouteConfig>,
    /// Only populated for `PathMatch::Regex` routes
    regex: Option<Regex>,
}

// ── Router ────────────────────────────────────────────────────────────────────

/// The matching engine. Built once per config version, then read-only.
///
/// Route evaluation order:
/// 1. Exact matches (checked first, O(path_length) trie lookup)
/// 2. Prefix matches (O(path_length) trie lookup, longest-match wins)
/// 3. Regex matches (linear scan, evaluated in declaration order)
///
/// Within each tier, method and header predicates are applied to
/// narrow candidates to the single winning route.
pub struct Router {
    /// Trie for exact path matches
    exact_trie: TrieNode,
    /// Trie for prefix path matches
    prefix_trie: TrieNode,
    /// Regex routes — evaluated in-order after the tries
    regex_routes: Vec<CompiledRoute>,
}

impl Router {
    /// Build a [`Router`] from a [`GatewayConfig`].
    ///
    /// Compiles all regex patterns and populates both tries.
    /// Returns an error if any regex pattern is invalid (should be caught
    /// by config validation first, but we double-check here).
    pub fn build(cfg: &GatewayConfig) -> Result<Self, RouterError> {
        let mut exact_trie  = TrieNode::default();
        let mut prefix_trie = TrieNode::default();
        let mut regex_routes = Vec::new();

        for route in &cfg.routes {
            let route = Arc::new(route.clone());
            match &route.match_.path {
                PathMatch::Exact { value } => {
                    let normalised = normalise_path(value);
                    exact_trie.insert_prefix(&normalised, Arc::clone(&route));
                }
                PathMatch::Prefix { value } => {
                    let normalised = normalise_path(value);
                    prefix_trie.insert_prefix(&normalised, Arc::clone(&route));
                }
                PathMatch::Regex { pattern } => {
                    let regex = Regex::new(pattern).map_err(|e| RouterError::InvalidRegex {
                        pattern: pattern.clone(),
                        source: e,
                    })?;
                    regex_routes.push(CompiledRoute {
                        route,
                        regex: Some(regex),
                    });
                }
            }
        }

        Ok(Self { exact_trie, prefix_trie, regex_routes })
    }

    /// Match an incoming request to a route.
    ///
    /// Returns `Ok(MatchedRoute)` on the first match, `Err(RouterError::NoMatch)`
    /// if no route applies. Evaluation is:
    /// 1. Exact → 2. Prefix (longest first) → 3. Regex (declaration order)
    pub fn match_request(
        &self,
        path: &str,
        method: &Method,
        headers: &HeaderMap,
    ) -> Result<MatchedRoute, RouterError> {
        let normalised = normalise_path(path);

        // ── Stage 1: exact ────────────────────────────────────────────────────
        if let Some(route) = self.exact_trie.find_exact(&normalised) {
            if matches_predicates(
                route.match_.method.as_deref(),
                &route.match_.headers,
                method,
                headers,
            ) {
                trace!(route_id = %route.id, "exact match");
                return Ok(MatchedRoute::new(route));
            }
        }

        // ── Stage 2: prefix (longest match first) ─────────────────────────────
        let mut prefix_candidates: Vec<Arc<RouteConfig>> = Vec::new();
        self.prefix_trie.collect_prefix_matches(&normalised, &mut prefix_candidates);

        // The trie collects shallow → deep; reverse for longest-first
        prefix_candidates.reverse();

        for route in prefix_candidates {
            if matches_predicates(
                route.match_.method.as_deref(),
                &route.match_.headers,
                method,
                headers,
            ) {
                trace!(route_id = %route.id, "prefix match");
                return Ok(MatchedRoute::new(route));
            }
        }

        // ── Stage 3: regex (declaration order) ────────────────────────────────
        for compiled in &self.regex_routes {
            let Some(re) = &compiled.regex else { continue };

            let Some(caps) = re.captures(path) else { continue };

            if !matches_predicates(
                compiled.route.match_.method.as_deref(),
                &compiled.route.match_.headers,
                method,
                headers,
            ) {
                continue;
            }

            // Extract named capture groups into path_params
            let params: Vec<(String, String)> = re
                .capture_names()
                .flatten()
                .filter_map(|name| {
                    caps.name(name)
                        .map(|m| (name.to_string(), m.as_str().to_string()))
                })
                .collect();

            trace!(route_id = %compiled.route.id, "regex match");
            return Ok(MatchedRoute::with_params(Arc::clone(&compiled.route), params));
        }

        Err(RouterError::NoMatch(path.to_string()))
    }
}

/// Strip leading `/`, lowercase, collapse duplicate slashes.
fn normalise_path(path: &str) -> String {
    let trimmed = path.trim_start_matches('/');
    // Collapse `//` sequences and lowercase
    let mut out = String::with_capacity(trimmed.len());
    let mut last_was_slash = false;
    for ch in trimmed.chars() {
        if ch == '/' {
            if !last_was_slash {
                out.push(ch);
            }
            last_was_slash = true;
        } else {
            out.push(ch.to_ascii_lowercase());
            last_was_slash = false;
        }
    }
    out
}

// ── RouterHandle ──────────────────────────────────────────────────────────────

/// A shareable, hot-reloadable handle to the active [`Router`].
///
/// Backed by `ArcSwap` — readers get a lock-free pointer load on every
/// request; the control plane atomically swaps in a new router on reload.
pub struct RouterHandle {
    inner: ArcSwap<Router>,
}

impl RouterHandle {
    /// Build and wrap the initial router.
    pub fn new(cfg: &GatewayConfig) -> Result<Self, RouterError> {
        let router = Router::build(cfg)?;
        Ok(Self {
            inner: ArcSwap::new(Arc::new(router)),
        })
    }

    /// Atomic lock-free read — call this on every request.
    #[inline(always)]
    pub fn current(&self) -> arc_swap::Guard<Arc<Router>> {
        self.inner.load()
    }

    /// Rebuild from new config and hot-swap. Old router stays alive until all
    /// in-flight requests holding a guard complete.
    pub fn rebuild(&self, cfg: &GatewayConfig) -> Result<(), RouterError> {
        let router = Router::build(cfg)?;
        self.inner.store(Arc::new(router));
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_config::*;
    use std::collections::HashMap;
    use std::time::Duration;

    fn build_cfg(routes: Vec<RouteConfig>) -> GatewayConfig {
        GatewayConfig {
            listener: ListenerConfig {
                bind: "0.0.0.0:8080".into(),
                tls: None,
                max_body_bytes: 4 * 1024 * 1024,
                keepalive: Duration::from_secs(75),
                max_concurrent_requests: 10_000,
                concurrency_acquire_timeout_ms: 500,
            },
            upstreams: {
                let mut m = HashMap::new();
                m.insert("backend".into(), UpstreamConfig {
                    endpoints: vec![EndpointConfig {
                        address: "127.0.0.1:9000".into(),
                        weight: 1,
                        active: true,
                    }],
                    load_balancing: LoadBalancingConfig::WeightedRoundRobin,
                    health_check: None,
                    timeouts: TimeoutConfig {
                        connect: Duration::from_secs(2),
                        request: Duration::from_secs(30),
                        idle: Duration::from_secs(90),
                    },
                    retry: None,
                });
                m
            },
            routes,
            rate_limiting: RateLimitConfig {
                redis_url: "redis://localhost".into(),
                policies: HashMap::new(),
            },
            cache: CacheConfig {
                redis_url: "redis://localhost".into(),
                l1_capacity: 1000,
            },
            auth: AuthConfig {
                jwt: JwtConfig {
                    jwks_url: "https://example.com/.well-known/jwks.json".into(),
                    audience: vec!["stateform".into()],
                    issuers: vec!["https://example.com".into()],
                    jwks_cache_ttl: Duration::from_secs(300),
                },
                api_keys: ApiKeyConfig {
                    redis_key_prefix: "stateform:apikey:".into(),
                    redis_url: "redis://localhost".into(),
                },
                mtls: MtlsConfig {
                    client_ca_path: "/certs/ca.pem".into(),
                    required: false,
                },
            },
            observability: ObservabilityConfig {
                metrics_bind: "0.0.0.0:9090".into(),
            },
            control_plane: ControlPlaneConfig {
                grpc_address: "http://localhost:50051".into(),
                reconnect_interval: Duration::from_secs(5),
            },
        }
    }

    fn route(id: &str, path: PathMatch) -> RouteConfig {
        RouteConfig {
            id: id.into(),
            match_: RouteMatch {
                path,
                method: None,
                headers: HashMap::new(),
            },
            upstream: "backend".into(),
            load_balancing: None,
            middleware: MiddlewareConfig::default(),
            headers: HeaderMutations::default(),
            max_concurrent_requests: None,
        }
    }

    #[test]
    fn exact_beats_prefix() {
        let cfg = build_cfg(vec![
            route("broad",  PathMatch::Prefix { value: "/api".into() }),
            route("exact",  PathMatch::Exact  { value: "/api/status".into() }),
        ]);
        let router = Router::build(&cfg).unwrap();
        let m = router.match_request("/api/status", &Method::GET, &HeaderMap::new()).unwrap();
        assert_eq!(m.route_id(), "exact");
    }

    #[test]
    fn longest_prefix_wins() {
        let cfg = build_cfg(vec![
            route("root",  PathMatch::Prefix { value: "/".into() }),
            route("api",   PathMatch::Prefix { value: "/api".into() }),
            route("users", PathMatch::Prefix { value: "/api/users".into() }),
        ]);
        let router = Router::build(&cfg).unwrap();
        let m = router.match_request("/api/users/123", &Method::GET, &HeaderMap::new()).unwrap();
        assert_eq!(m.route_id(), "users");
    }

    #[test]
    fn catch_all_prefix() {
        let cfg = build_cfg(vec![
            route("catch-all", PathMatch::Prefix { value: "/".into() }),
        ]);
        let router = Router::build(&cfg).unwrap();
        assert!(router.match_request("/anything/at/all", &Method::GET, &HeaderMap::new()).is_ok());
    }

    #[test]
    fn regex_with_named_captures() {
        let cfg = build_cfg(vec![
            route("resource", PathMatch::Regex {
                pattern: r"^/resources/(?P<id>[0-9]+)$".into(),
            }),
        ]);
        let router = Router::build(&cfg).unwrap();
        let m = router.match_request("/resources/42", &Method::GET, &HeaderMap::new()).unwrap();
        assert_eq!(m.route_id(), "resource");
        assert_eq!(m.path_params[0], ("id".into(), "42".into()));
    }

    #[test]
    fn no_match_returns_error() {
        let cfg = build_cfg(vec![
            route("api", PathMatch::Prefix { value: "/api".into() }),
        ]);
        let router = Router::build(&cfg).unwrap();
        assert!(matches!(
            router.match_request("/admin", &Method::GET, &HeaderMap::new()),
            Err(RouterError::NoMatch(_))
        ));
    }

    #[test]
    fn method_predicate_filters_candidates() {
        let routes = vec![
            RouteConfig {
                id: "get-users".into(),
                match_: RouteMatch {
                    path: PathMatch::Prefix { value: "/users".into() },
                    method: Some("GET".into()),
                    headers: HashMap::new(),
                },
                upstream: "backend".into(),
                load_balancing: None,
                middleware: MiddlewareConfig::default(),
                headers: HeaderMutations::default(),
                max_concurrent_requests: None,
            },
            RouteConfig {
                id: "post-users".into(),
                match_: RouteMatch {
                    path: PathMatch::Prefix { value: "/users".into() },
                    method: Some("POST".into()),
                    headers: HashMap::new(),
                },
                upstream: "backend".into(),
                load_balancing: None,
                middleware: MiddlewareConfig::default(),
                headers: HeaderMutations::default(),
                max_concurrent_requests: None,
            },
        ];
        let cfg = build_cfg(routes);
        let router = Router::build(&cfg).unwrap();

        let get = router.match_request("/users", &Method::GET, &HeaderMap::new()).unwrap();
        assert_eq!(get.route_id(), "get-users");

        let post = router.match_request("/users", &Method::POST, &HeaderMap::new()).unwrap();
        assert_eq!(post.route_id(), "post-users");
    }
}
