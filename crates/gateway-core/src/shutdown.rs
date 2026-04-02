use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tracing::{info, warn};

const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

// ── ShutdownSignal ────────────────────────────────────────────────────────────

/// Sent to all components when a shutdown is requested.
///
/// Components receive this via `watch::Receiver<bool>` and select on it
/// in their accept/wait loops. The signal is a simple broadcast — every
/// receiver sees it simultaneously.
#[derive(Clone)]
pub struct ShutdownSignal {
    rx: watch::Receiver<bool>,
}

impl ShutdownSignal {
    /// Returns true if shutdown has been requested.
    pub fn is_set(&self) -> bool {
        *self.rx.borrow()
    }

    /// Async wait until shutdown is signalled.
    pub async fn wait(&mut self) {
        // Wait for the value to change to `true`
        let _ = self.rx.wait_for(|v| *v).await;
    }
}

// ── DrainGuard ────────────────────────────────────────────────────────────────

/// Held by each in-flight request. When all guards are dropped, the drain
/// counter reaches zero and the shutdown can complete.
pub struct DrainGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for DrainGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

// ── ShutdownCoordinator ───────────────────────────────────────────────────────

/// Coordinates two-phase graceful shutdown across the server.
///
/// Phase 1 — signal: broadcast to all accept loops to stop accepting.
/// Phase 2 — drain: wait for in-flight request counter to reach zero
///            (or `DRAIN_TIMEOUT`, whichever comes first).
pub struct ShutdownCoordinator {
    tx:      watch::Sender<bool>,
    signal:  ShutdownSignal,
    counter: Arc<AtomicUsize>,
}

impl ShutdownCoordinator {
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(false);
        Self {
            tx,
            signal: ShutdownSignal { rx },
            counter: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Get a cloneable shutdown signal to hand to server components.
    pub fn signal(&self) -> ShutdownSignal {
        self.signal.clone()
    }

    /// Acquire a drain guard for one in-flight request.
    pub fn acquire(&self) -> DrainGuard {
        self.counter.fetch_add(1, Ordering::AcqRel);
        DrainGuard { counter: Arc::clone(&self.counter) }
    }

    /// Trigger shutdown and wait for all in-flight requests to drain.
    ///
    /// Call this from the OS signal handler (SIGTERM / Ctrl-C).
    pub async fn shutdown(&self) {
        info!("shutdown signal received — stopping accept loops");
        let _ = self.tx.send(true);

        info!("draining in-flight requests (timeout: {}s)", DRAIN_TIMEOUT.as_secs());

        let start = std::time::Instant::now();
        loop {
            if self.counter.load(Ordering::Acquire) == 0 {
                info!("all requests drained — shutdown complete");
                return;
            }
            if start.elapsed() >= DRAIN_TIMEOUT {
                let remaining = self.counter.load(Ordering::Acquire);
                warn!(remaining, "drain timeout reached — forcing shutdown");
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Default for ShutdownCoordinator {
    fn default() -> Self { Self::new() }
}
