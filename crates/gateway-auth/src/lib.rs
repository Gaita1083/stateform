// gateway-auth: Middleware — JWT, API key, mTLS (composable trait)

pub mod api_key;
pub mod context;
pub mod error;
pub mod jwt;
pub mod mtls;
pub mod pipeline;

pub use context::{AuthContext, AuthMechanism, Claims, PeerIdentity};
pub use error::AuthError;
pub use pipeline::AuthPipeline;
