// gateway-core: Tokio runtime, HTTP lifecycle, server entrypoint

pub mod error;
pub mod pipeline;
pub mod server;
pub mod shutdown;
pub mod tls;
pub mod upstream;

pub use error::CoreError;
pub use server::GatewayServer;
pub use shutdown::ShutdownCoordinator;
