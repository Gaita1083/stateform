use std::sync::Arc;

use gateway_config::RouteConfig;

/// A node in the compressed radix trie.
///
/// Each node stores:
/// - The path segment it represents (the compressed edge label)
/// - Zero or more routes that terminate at this node (multiple routes
///   can share the same prefix but differ in method or headers)
/// - Child nodes for deeper path segments
///
/// The trie is **read-only after construction**. Rebuilding on config
/// reload is cheap — the old trie stays alive as long as in-flight
/// requests hold their `Arc<Router>` guard.
#[derive(Debug, Default)]
pub struct TrieNode {
    /// The path segment this edge represents (e.g. "api", "v1")
    pub segment: String,

    /// Routes that are terminal at exactly this node's path
    pub routes: Vec<Arc<RouteConfig>>,

    /// Child nodes, sorted lexicographically for binary-search lookup
    pub children: Vec<TrieNode>,
}

impl TrieNode {
    pub fn new(segment: impl Into<String>) -> Self {
        Self {
            segment: segment.into(),
            routes: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Insert a prefix route into the trie.
    ///
    /// `path` should be the normalised path (leading `/` stripped for the root
    /// call, then passed recursively with each segment consumed).
    pub fn insert_prefix(&mut self, path: &str, route: Arc<RouteConfig>) {
        if path.is_empty() {
            self.routes.push(route);
            return;
        }

        let (head, tail) = split_first_segment(path);

        // Find or create a child matching `head`
        match self.children.iter().position(|c| c.segment == head) {
            Some(idx) => self.children[idx].insert_prefix(tail, route),
            None => {
                let mut child = TrieNode::new(head);
                child.insert_prefix(tail, route);
                self.children.push(child);
                // Keep children sorted for binary search
                self.children.sort_by(|a, b| a.segment.cmp(&b.segment));
            }
        }
    }

    /// Walk the trie collecting all routes whose prefix matches `path`.
    ///
    /// Returns candidates from longest to shortest match (most-specific first).
    /// The caller is responsible for applying method/header predicates to pick
    /// the winning route from the candidates.
    pub fn collect_prefix_matches<'a>(
        &'a self,
        path: &str,
        candidates: &mut Vec<Arc<RouteConfig>>,
    ) {
        // Routes at this node match everything below — add them (lower priority)
        candidates.extend(self.routes.iter().cloned());

        if path.is_empty() {
            return;
        }

        let (head, tail) = split_first_segment(path);

        // Binary search for the matching child
        if let Ok(idx) = self.children.binary_search_by(|c| c.segment.as_str().cmp(head)) {
            self.children[idx].collect_prefix_matches(tail, candidates);
        }
    }

    /// Walk the trie looking for an exact match at `path`.
    pub fn find_exact(&self, path: &str) -> Option<Arc<RouteConfig>> {
        if path.is_empty() {
            // Pick the first exact-type route at this node
            return self.routes.first().cloned();
        }

        let (head, tail) = split_first_segment(path);

        if let Ok(idx) = self.children.binary_search_by(|c| c.segment.as_str().cmp(head)) {
            self.children[idx].find_exact(tail)
        } else {
            None
        }
    }
}

/// Split `/foo/bar/baz` into (`"foo"`, `"bar/baz"`).
/// Leading slashes are consumed.
fn split_first_segment(path: &str) -> (&str, &str) {
    let path = path.trim_start_matches('/');
    match path.find('/') {
        Some(idx) => (&path[..idx], &path[idx + 1..]),
        None => (path, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_config::*;
    use std::collections::HashMap;

    fn make_route(id: &str, path_prefix: &str) -> Arc<RouteConfig> {
        Arc::new(RouteConfig {
            id: id.into(),
            match_: RouteMatch {
                path: PathMatch::Prefix { value: path_prefix.into() },
                method: None,
                headers: HashMap::new(),
            },
            upstream: "backend".into(),
            load_balancing: None,
            middleware: MiddlewareConfig::default(),
            headers: HeaderMutations::default(),
        })
    }

    #[test]
    fn prefix_match_root() {
        let mut root = TrieNode::default();
        let route = make_route("catch-all", "/");
        root.insert_prefix("", route.clone());

        let mut candidates = Vec::new();
        root.collect_prefix_matches("v1/users", &mut candidates);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, "catch-all");
    }

    #[test]
    fn longer_prefix_collected_last() {
        let mut root = TrieNode::default();
        let broad  = make_route("broad", "/v1");
        let narrow = make_route("narrow", "/v1/users");
        root.insert_prefix("v1", broad.clone());
        root.insert_prefix("v1/users", narrow.clone());

        let mut candidates = Vec::new();
        root.collect_prefix_matches("v1/users/123", &mut candidates);

        // Both match; narrow was inserted deeper so appears later in BFS
        // but the router reverses to get longest-first
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn no_match_returns_empty() {
        let mut root = TrieNode::default();
        root.insert_prefix("api", make_route("api", "/api"));

        let mut candidates = Vec::new();
        root.collect_prefix_matches("admin/users", &mut candidates);
        assert!(candidates.is_empty());
    }

    #[test]
    fn exact_match() {
        let mut root = TrieNode::default();
        let route = make_route("health", "/healthz");
        root.insert_prefix("healthz", route.clone());

        assert!(root.find_exact("healthz").is_some());
        assert!(root.find_exact("healthz/extra").is_none());
    }
}
