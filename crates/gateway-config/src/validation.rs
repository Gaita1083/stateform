use crate::{error::ConfigError, schema::*};

/// Validate a parsed [`GatewayConfig`] for semantic correctness.
///
/// `serde` handles structural validity (required fields, type mismatches).
/// This function catches *semantic* errors: routes pointing at unknown
/// upstreams, empty endpoint lists, invalid regex patterns, etc.
pub fn validate(cfg: &GatewayConfig) -> Result<(), ConfigError> {
    validate_upstreams(cfg)?;
    validate_routes(cfg)?;
    validate_rate_limit_refs(cfg)?;
    Ok(())
}

fn validate_upstreams(cfg: &GatewayConfig) -> Result<(), ConfigError> {
    for (name, upstream) in &cfg.upstreams {
        if upstream.endpoints.is_empty() {
            return Err(ConfigError::Validation(format!(
                "upstream `{name}` must have at least one endpoint"
            )));
        }

        for ep in &upstream.endpoints {
            if ep.weight == 0 {
                return Err(ConfigError::Validation(format!(
                    "upstream `{name}` endpoint `{}` has weight 0 — use `active: false` to disable",
                    ep.address
                )));
            }
        }

        if let LoadBalancingConfig::ConsistentHashing { hash_header, .. } = &upstream.load_balancing {
            if hash_header.trim().is_empty() {
                return Err(ConfigError::Validation(format!(
                    "upstream `{name}`: consistent_hashing requires a non-empty `hash_header`"
                )));
            }
        }
    }
    Ok(())
}

fn validate_routes(cfg: &GatewayConfig) -> Result<(), ConfigError> {
    for route in &cfg.routes {
        if route.id.trim().is_empty() {
            return Err(ConfigError::Validation(
                "every route must have a non-empty `id`".into(),
            ));
        }

        if !cfg.upstreams.contains_key(&route.upstream) {
            return Err(ConfigError::Validation(format!(
                "route `{}` references unknown upstream `{}`",
                route.id, route.upstream
            )));
        }

        // Validate regex patterns early so startup fails loudly, not at runtime
        if let PathMatch::Regex { pattern } = &route.match_.path {
            regex::Regex::new(pattern).map_err(|e| {
                ConfigError::Validation(format!(
                    "route `{}` has invalid path regex `{pattern}`: {e}",
                    route.id
                ))
            })?;
        }
    }

    // Warn about duplicate route IDs (still valid, but ambiguous in metrics)
    let mut ids = std::collections::HashSet::new();
    for route in &cfg.routes {
        if !ids.insert(&route.id) {
            return Err(ConfigError::Validation(format!(
                "duplicate route id `{}` — each route must have a unique id",
                route.id
            )));
        }
    }

    Ok(())
}

fn validate_rate_limit_refs(cfg: &GatewayConfig) -> Result<(), ConfigError> {
    for route in &cfg.routes {
        if let Some(rl) = &route.middleware.rate_limit {
            if !cfg.rate_limiting.policies.contains_key(&rl.policy) {
                return Err(ConfigError::Validation(format!(
                    "route `{}` references unknown rate-limit policy `{}`",
                    route.id, rl.policy
                )));
            }
        }
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    fn minimal_config() -> GatewayConfig {
        GatewayConfig {
            listener: ListenerConfig {
                bind: "0.0.0.0:8080".into(),
                tls: None,
                max_body_bytes: 4 * 1024 * 1024,
                keepalive: Duration::from_secs(75),
            },
            upstreams: {
                let mut m = HashMap::new();
                m.insert(
                    "backend".into(),
                    UpstreamConfig {
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
                    },
                );
                m
            },
            routes: vec![RouteConfig {
                id: "root".into(),
                match_: RouteMatch {
                    path: PathMatch::Prefix { value: "/".into() },
                    method: None,
                    headers: HashMap::new(),
                },
                upstream: "backend".into(),
                load_balancing: None,
                middleware: MiddlewareConfig::default(),
                headers: HeaderMutations::default(),
            }],
            rate_limiting: RateLimitConfig {
                redis_url: "redis://localhost".into(),
                policies: HashMap::new(),
            },
            cache: CacheConfig {
                redis_url: "redis://localhost".into(),
                l1_capacity: 10_000,
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
                    required: true,
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

    #[test]
    fn valid_config_passes() {
        assert!(validate(&minimal_config()).is_ok());
    }

    #[test]
    fn rejects_empty_endpoints() {
        let mut cfg = minimal_config();
        cfg.upstreams.get_mut("backend").unwrap().endpoints.clear();
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn rejects_unknown_upstream_reference() {
        let mut cfg = minimal_config();
        cfg.routes[0].upstream = "does_not_exist".into();
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn rejects_duplicate_route_ids() {
        let mut cfg = minimal_config();
        cfg.routes.push(cfg.routes[0].clone());
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn rejects_invalid_regex() {
        let mut cfg = minimal_config();
        cfg.routes[0].match_.path = PathMatch::Regex { pattern: "[invalid".into() };
        assert!(validate(&cfg).is_err());
    }
}
