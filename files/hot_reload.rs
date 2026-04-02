use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::{error::ConfigError, schema::GatewayConfig, validation};

/// A shareable, hot-reloadable handle to the active [`GatewayConfig`].
///
/// # Design
///
/// The inner config is stored in an [`ArcSwap`], which means:
/// - **Readers** (the hot path) call `.load()` — a single atomic pointer load,
///   no locking, no contention.
/// - **Writers** (reload path) call `.store()` — swaps the pointer atomically.
///   Existing readers holding their `Arc` finish their work against the old
///   config; new readers immediately see the new one.
///
/// This makes hot-reloads completely invisible to in-flight requests.
///
/// # Usage
///
/// ```rust,ignore
/// // At startup:
/// let watcher = ConfigWatcher::load("/etc/stateform/config.yaml").await?;
/// let watcher = Arc::new(watcher);
///
/// // In any handler, zero-cost config access:
/// let cfg = watcher.current();
/// let bind = &cfg.listener.bind;
///
/// // On a reload signal from the control plane:
/// watcher.reload().await?;
/// ```
pub struct ConfigWatcher {
    path: PathBuf,
    inner: ArcSwap<GatewayConfig>,
    /// Broadcast channel so other subsystems can react to reload events
    notify_tx: watch::Sender<Arc<GatewayConfig>>,
    pub notify_rx: watch::Receiver<Arc<GatewayConfig>>,
}

impl ConfigWatcher {
    /// Load and validate config from `path`, returning a ready watcher.
    pub async fn load(path: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        let path = path.into();
        let cfg = Self::parse_and_validate(&path).await?;
        let cfg = Arc::new(cfg);

        let (notify_tx, notify_rx) = watch::channel(Arc::clone(&cfg));

        Ok(Self {
            path,
            inner: ArcSwap::new(cfg),
            notify_tx,
            notify_rx,
        })
    }

    /// Return the current config snapshot.
    ///
    /// This is a single atomic load — safe to call from every request.
    #[inline(always)]
    pub fn current(&self) -> Arc<GatewayConfig> {
        self.inner.load_full()
    }

    /// Re-read the config file, validate, and hot-swap if valid.
    ///
    /// On validation error the old config stays active and an error is returned.
    pub async fn reload(&self) -> Result<(), ConfigError> {
        let new_cfg = match Self::parse_and_validate(&self.path).await {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "config reload failed — keeping current config");
                return Err(e);
            }
        };

        let new_cfg = Arc::new(new_cfg);
        self.inner.store(Arc::clone(&new_cfg));

        // Notify subsystems that care about specific config changes
        let _ = self.notify_tx.send(new_cfg);

        info!(path = %self.path.display(), "config reloaded successfully");
        Ok(())
    }

    /// Subscribe to reload notifications.
    ///
    /// Callers receive the new `Arc<GatewayConfig>` whenever a reload succeeds.
    pub fn subscribe(&self) -> watch::Receiver<Arc<GatewayConfig>> {
        self.notify_rx.clone()
    }

    async fn parse_and_validate(path: &PathBuf) -> Result<GatewayConfig, ConfigError> {
        let raw = tokio::fs::read_to_string(path).await?;
        let cfg: GatewayConfig = serde_yaml::from_str(&raw)?;
        validation::validate(&cfg)?;
        Ok(cfg)
    }
}
