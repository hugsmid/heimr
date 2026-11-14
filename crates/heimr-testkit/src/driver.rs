//! What a scenario may do to an adapter under test.
//!
//! Scenarios only talk to a [`Driver`]. The in-process driver
//! ([`InProcess`](crate::InProcess)) runs a Rust adapter inside the real
//! app; a runner for adapters in other processes implements the same trait
//! over its protocol and reuses the scenario list unchanged. Everything here
//! is plain data and synchronous calls, so no async runtime or Rust type of
//! the adapter leaks into the scenarios.

use heimr_core::{
    AdapterDescriptor, AdapterStats, AuditRecord, Capabilities, Command, Currency, Envelope, Mode,
    Outcome, Rejection, SeriesTemplate,
};
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

/// A failure an upstream (a device, a cloud API, a file tree) can be told
/// to simulate. Upstreams support what they can; see [`Driver::inject`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Fault {
    /// Unreachable or failing in a way a retry may fix (timeouts, 5xx, a
    /// device offline).
    Unavailable,
    /// Refusing the adapter's credentials or pairing: a restart cannot fix
    /// it.
    Rejected,
    /// Answering, but with garbage (truncated, wrong types, nonsense).
    Malformed,
}

/// What an upstream has seen from the adapter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Traffic {
    /// Every request or read attempt.
    pub requests: u64,
    /// Requests that change device state (for HTTP: anything but GET, HEAD
    /// and OPTIONS).
    pub writes: u64,
}

impl Traffic {
    /// Counts.
    pub fn new(requests: u64, writes: u64) -> Self {
        Self { requests, writes }
    }
}

/// Why preparing an instance did not produce one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "failure", rename_all = "snake_case")]
#[non_exhaustive]
pub enum PrepareFailure {
    /// The adapter refused the settings.
    Refused {
        /// Whether it was reported as a configuration error.
        config: bool,
        /// The key named, if any.
        key: Option<String>,
        /// Whether the error is fatal (a restart cannot help).
        fatal: bool,
        /// The message.
        message: String,
    },
    /// Preparing panicked.
    Panicked {
        /// The panic message, if it was a string.
        message: String,
    },
}

/// One supervisor-reported failure of the instance (from the app's
/// `adapter_failed` events).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Failure {
    /// When.
    pub time: Timestamp,
    /// The error, `panicked`, or why it is kept down.
    pub error: String,
    /// Whether the instance stays down.
    pub fatal: bool,
}

/// The operations scenarios use. Implementations own whatever runtime,
/// process or transport they need; every call returns once its effect is
/// complete.
pub trait Driver {
    /// The kind's descriptor.
    fn descriptor(&self) -> AdapterDescriptor;

    /// The instance name the driver runs the adapter as.
    fn instance(&self) -> &str;

    /// The settings [`Driver::start`] uses (the adapter's own keys).
    fn settings(&mut self) -> serde_json::Value;

    /// Validate `settings` and build (not run) an instance, returning its
    /// capabilities.
    fn prepare(&mut self, settings: &serde_json::Value) -> Result<Capabilities, PrepareFailure>;

    /// The series templates of an instance built from [`Driver::settings`]
    /// (the kind's, with the intervals its configuration sets). The
    /// default is the kind's.
    fn series(&mut self) -> Vec<SeriesTemplate> {
        self.descriptor().series
    }

    /// Check one instance with [`Driver::settings`] as the app would for a
    /// site quoting `currency` (the site's time zone unchanged), without
    /// running it: the app's own checks (a kind's declared source currency
    /// must match the site's) as well as the adapter's. `Err` carries the
    /// app's message.
    fn check_site(&mut self, currency: Currency) -> Result<(), String>;

    /// Start a fresh app (or host) running one instance with
    /// [`Driver::settings`] in `mode`, on a fake clock reading
    /// [`Driver::now`]. Ends any previous run first.
    fn start(&mut self, mode: Mode) -> Result<(), String>;

    /// Stop the current run and start the instance again in `mode` on the
    /// same state directory, upstream and clock (carrying on from now), as
    /// a restart of the whole app would. A driver that cannot keep them
    /// returns `Err`, and the restart scenario is skipped.
    fn restart(&mut self, mode: Mode) -> Result<(), String> {
        let _ = mode;
        Err("this driver cannot restart an instance on the same state".into())
    }

    /// Let `by` pass on the fake clock, running the instance meanwhile.
    fn advance(&mut self, by: SignedDuration);

    /// Let the instance run without moving the clock (reach its first
    /// await point).
    fn settle(&mut self);

    /// The fake clock's wall time.
    fn now(&self) -> Timestamp;

    /// Submit a command through the core's command gate, as a decider.
    fn submit(&mut self, command: Command) -> Result<Outcome, Rejection>;

    /// Every record the instance has emitted in the current (or last) run,
    /// in order, as the core stamped them.
    fn records(&self) -> Vec<Envelope>;

    /// The same records split by run of the instance: a new list starts
    /// each time the supervisor (re)starts it. The default is one list;
    /// a driver that can see restarts should split them, or repeats across
    /// a restart are judged as repeats within one run.
    fn runs(&self) -> Vec<Vec<Envelope>> {
        vec![self.records()]
    }

    /// Supervisor-reported failures in the current (or last) run.
    fn failures(&self) -> Vec<Failure>;

    /// The core's stats for the instance.
    fn stats(&self) -> AdapterStats;

    /// The audit trail.
    fn audit(&self) -> Vec<AuditRecord>;

    /// Make the upstream simulate `fault` (`None` clears it). Returns
    /// `false` if the upstream cannot.
    fn inject(&mut self, fault: Option<Fault>) -> bool;

    /// What the upstream has seen, if it can tell.
    fn traffic(&self) -> Option<Traffic>;

    /// Signal shutdown and wait until the run has ended (the app aborts an
    /// instance that overstays the shutdown deadline). Returns the final
    /// stats.
    fn stop(&mut self) -> AdapterStats;
}
