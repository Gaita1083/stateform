// gateway-router: Route matching engine (radix tree, regex, rewrite)

pub mod error;
pub mod matched;
pub mod predicate;
pub mod router;
pub mod trie;

pub use router::{Router, RouterHandle};
pub use matched::MatchedRoute;
pub use error::RouterError;
