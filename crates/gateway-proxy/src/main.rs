use std::env;
use std::sync::Arc;

use anyhow::Context;
use tracing::info;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use gateway_config::ConfigWatcher;
use gateway_core::GatewayServer;

/// Stateform API Gateway
///
/// Usage:
///   stateform [config-path]
///
/// Defaults to `./config.yaml` when no path is given.
/// Log level is controlled by the `RUST_LOG` environment variable:
///   RUST_LOG=stateform=debug,gateway_core=debug,info
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── Logging ───────────────────────────────────────────────────────────────
    tracing_subscriber::registry()
        .with(fmt::layer().json())          // JSON logs — Loki/Datadog friendly
        .with(EnvFilter::from_default_env())
        .init();

    // ── Config path ───────────────────────────────────────────────────────────
    let config_path = env::args().nth(1).unwrap_or_else(|| "config.yaml".into());

    info!(path = %config_path, version = env!("CARGO_PKG_VERSION"), "stateform starting");

    // ── Load config ───────────────────────────────────────────────────────────
    let config = ConfigWatcher::load(&config_path)
        .await
        .with_context(|| format!("failed to load config from `{config_path}`"))?;
    let config = Arc::new(config);

    // ── Build + run server ────────────────────────────────────────────────────
    GatewayServer::build(Arc::clone(&config))
        .await
        .context("server build failed")?
        .run()
        .await
        .context("server run failed")?;

    info!("stateform stopped");
    Ok(())
}
