//! The thing an adapter talks to, as the test kit sees it.

use crate::driver::{Fault, Traffic};

/// A fake device, service or file tree an adapter under test talks to.
///
/// The suite creates a fresh upstream for every run (so faults and traffic
/// never leak between scenarios) and asks it for the instance settings that
/// point the adapter at it. Fault injection and traffic counts are optional;
/// scenarios that need them are skipped, and say so, when an upstream
/// cannot provide them.
pub trait Upstream: Send + Sync + 'static {
    /// The instance settings (the adapter's own keys) pointing at this
    /// upstream.
    fn settings(&self) -> serde_json::Value;

    /// Start simulating `fault`, or stop (`None`). Returns whether the
    /// upstream supports it.
    fn inject(&self, fault: Option<Fault>) -> bool {
        let _ = fault;
        false
    }

    /// What the upstream has seen, if it can tell.
    fn traffic(&self) -> Option<Traffic> {
        None
    }
}

/// An upstream that is only fixed settings: for adapters whose data is
/// synthetic, or whose fake lives inside the settings.
#[derive(Debug, Clone)]
pub struct StaticUpstream(pub serde_json::Value);

impl Upstream for StaticUpstream {
    fn settings(&self) -> serde_json::Value {
        self.0.clone()
    }
}
