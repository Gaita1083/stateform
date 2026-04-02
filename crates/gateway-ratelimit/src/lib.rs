// gateway-ratelimit: L1 token bucket + L2 Redis bridge

pub mod error;
pub mod key;
pub mod l1_local;
pub mod l2_redis;
pub mod limiter;

pub use error::RateLimitError;
pub use limiter::{DeniedBy, RateLimiter, RateLimitOutcome};
