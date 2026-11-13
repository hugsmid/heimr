//! Per-instance stats, read from [`App::telemetry`](crate::App::telemetry).
//!
//! Counting happens in `heimr-core` at the seams the host owns (emitter,
//! actuator) plus runs and restarts in the supervisor; see
//! [`heimr_core::telemetry`].
//
// Export (OpenTelemetry, a `system` adapter publishing these stats as
// readings): not yet implemented. Exporters should read
// `Telemetry::snapshot()` periodically rather than hook the hot path.

use std::collections::BTreeMap;
use std::sync::Arc;

use heimr_core::{AdapterStats, StatsHandle};

/// Handles to every enabled instance's stats. Cheap to clone; snapshots are
/// plain, serialisable data.
#[derive(Debug, Clone, Default)]
pub struct Telemetry {
    adapters: Arc<BTreeMap<String, StatsHandle>>,
}

impl Telemetry {
    pub(crate) fn new(adapters: impl IntoIterator<Item = (String, StatsHandle)>) -> Self {
        Self {
            adapters: Arc::new(adapters.into_iter().collect()),
        }
    }

    /// One instance's stats.
    pub fn get(&self, instance: &str) -> Option<AdapterStats> {
        self.adapters.get(instance).map(StatsHandle::snapshot)
    }

    /// Every enabled instance's stats, by name.
    pub fn snapshot(&self) -> Vec<AdapterStats> {
        self.adapters.values().map(StatsHandle::snapshot).collect()
    }
}
