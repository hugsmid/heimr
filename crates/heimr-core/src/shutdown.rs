//! Cooperative shutdown signal.

use std::sync::Arc;

use tokio::sync::watch;

/// Create a linked trigger and signal.
pub fn shutdown_channel() -> (ShutdownTrigger, Shutdown) {
    let (tx, rx) = watch::channel(false);
    (ShutdownTrigger(Arc::new(tx)), Shutdown(rx))
}

/// Fires the shutdown signal. Dropping every trigger also counts as
/// shutdown, so a lost owner never leaves adapters running unsupervised.
#[derive(Debug, Clone)]
pub struct ShutdownTrigger(Arc<watch::Sender<bool>>);

impl ShutdownTrigger {
    /// Signal shutdown to every [`Shutdown`] handle.
    pub fn trigger(&self) {
        self.0.send_replace(true);
    }
}

/// Observes the shutdown signal. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Shutdown(watch::Receiver<bool>);

impl Shutdown {
    /// Whether shutdown has been signalled.
    pub fn is_triggered(&self) -> bool {
        *self.0.borrow() || self.0.has_changed().is_err()
    }

    /// Resolves once shutdown is signalled. Use it in a `tokio::select!`
    /// next to the adapter's work.
    pub async fn wait(&self) {
        let mut rx = self.0.clone();
        let _ = rx.wait_for(|stop| *stop).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn trigger_and_drop_both_signal() {
        let (trigger, s) = shutdown_channel();
        assert!(!s.is_triggered());
        trigger.trigger();
        s.wait().await;
        assert!(s.is_triggered());

        let (trigger, s) = shutdown_channel();
        drop(trigger);
        s.wait().await;
        assert!(s.is_triggered());
    }
}
