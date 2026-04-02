use std::sync::Arc;

use gateway_config::RouteConfig;

/// The result of a successful route match.
///
/// Carries an `Arc` to the resolved route config — cheap to clone, safe to
/// hold across await points. The upstream name, middleware stack, and header
/// mutations are all accessible without any further locking.
///
/// ## Captured path parameters
///
/// For regex routes, any named capture groups are extracted into
/// `path_params` so that header rewrite templates can reference them later
/// (e.g. `X-Resource-Id: {{id}}`).
#[derive(Debug, Clone)]
pub struct MatchedRoute {
    /// The resolved route. Everything downstream reads from this.
    pub route: Arc<RouteConfig>,

    /// Named capture groups from regex path patterns.
    /// Empty for prefix and exact matches.
    pub path_params: Vec<(String, String)>,
}

impl MatchedRoute {
    pub fn new(route: Arc<RouteConfig>) -> Self {
        Self {
            route,
            path_params: Vec::new(),
        }
    }

    pub fn with_params(route: Arc<RouteConfig>, params: Vec<(String, String)>) -> Self {
        Self { route, path_params: params }
    }

    /// Convenience: the upstream name this request should be forwarded to.
    #[inline]
    pub fn upstream(&self) -> &str {
        &self.route.upstream
    }

    /// Convenience: the route identifier used in metrics and logs.
    #[inline]
    pub fn route_id(&self) -> &str {
        &self.route.id
    }
}
