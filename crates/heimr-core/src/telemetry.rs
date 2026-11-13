//! Per-adapter telemetry that adapters get for free.
//!
//! The core counts at the seams it owns: the [`Emitter`] (readings, state,
//! events, emit errors, provenance refusals, last record time), the
//! [`Actuator`](crate::Actuator) (commands offered, delivered, shadowed,
//! rejected, dropped) and the supervisor (runs, restarts, failures).
//! Adapters add one-line health reports ([`HealthReporter`]) and, if they
//! want, custom counters and gauges ([`Metrics`]). Everything lands in one
//! [`AdapterStats`] per instance.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::actuator::{Mode, Outcome, Rejection};
use crate::clock::SharedClock;
use crate::context::{EmitError, Emitter};
use crate::record::{Event, Record};

/// What an adapter last said about itself (or the supervisor, on failure).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HealthStatus {
    /// Nothing reported yet.
    #[default]
    Unknown,
    /// Working normally.
    Ok,
    /// Working, but impaired (e.g. retrying, partial data).
    Degraded,
    /// Not working.
    Failed,
}

impl HealthStatus {
    /// `"unknown"`, `"ok"`, `"degraded"` or `"failed"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Ok => "ok",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
        }
    }
}

/// Where an instance is in its supervised life.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RunState {
    /// Not started yet.
    #[default]
    Pending,
    /// `run` is executing.
    Running,
    /// Failed; waiting to restart.
    Backoff,
    /// Failed with a fatal error; stays down until the app restarts.
    Down,
    /// Stopped at shutdown.
    Stopped,
}

/// A snapshot of one instance's telemetry. Plain data; cheap to clone.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct AdapterStats {
    /// Instance name.
    pub name: String,
    /// Adapter kind.
    pub kind: String,
    /// Shadow or live.
    pub mode: Mode,
    /// Supervision state.
    pub run_state: RunState,
    /// When `run` was last started.
    pub started_at: Option<Timestamp>,
    /// When `run` last returned.
    pub exited_at: Option<Timestamp>,
    /// The error the last run ended with, if any.
    pub exit_error: Option<String>,
    /// Times the instance was restarted after a failure.
    pub restarts: u32,
    /// Failures since the instance last ran healthily for the reset period.
    pub consecutive_failures: u32,
    /// When the next restart is due (wall time, informational).
    pub next_restart_at: Option<Timestamp>,
    /// Readings emitted.
    pub readings: u64,
    /// State records emitted.
    pub states: u64,
    /// Events emitted (including health reports).
    pub events: u64,
    /// Failed emits (invalid record, refused provenance, app closed).
    pub emit_errors: u64,
    /// Records refused for naming another source or actor, or a reserved
    /// event kind (also counted in `emit_errors`).
    pub provenance_rejections: u64,
    /// Timestamp of the newest record emitted.
    pub last_emit_at: Option<Timestamp>,
    /// Commands submitted for this instance.
    pub commands_offered: u64,
    /// Commands delivered (live).
    pub commands_delivered: u64,
    /// Commands logged but withheld (shadow mode).
    pub commands_shadowed: u64,
    /// Commands refused by capability or safety policy.
    pub commands_rejected_policy: u64,
    /// Commands refused by a rate limit.
    pub commands_rejected_rate_limit: u64,
    /// Commands withheld because the audit record could not be made durable
    /// in time.
    pub commands_audit_failed: u64,
    /// Commands that could not be queued (queue full, instance not running).
    pub commands_failed: u64,
    /// Accepted commands that never reached the instance (it stopped first)
    /// and were audited as dropped.
    pub commands_dropped: u64,
    /// Last reported health.
    pub health: HealthStatus,
    /// Reason given with the last health report.
    pub health_reason: Option<String>,
    /// When health last changed.
    pub health_since: Option<Timestamp>,
    /// Custom counters ([`Metrics::counter`]).
    pub counters: BTreeMap<String, u64>,
    /// Custom gauges ([`Metrics::gauge`]).
    pub gauges: BTreeMap<String, f64>,
}

/// Shared, mutable stats for one instance. The core and the host's
/// supervisor update it; [`StatsHandle::snapshot`] reads it. Adapters never
/// get one: they get an [`InstanceStats`] ([`StatsHandle::for_instance`]).
#[derive(Debug, Clone)]
pub struct StatsHandle(Arc<Mutex<AdapterStats>>);

/// The view of an instance's stats handed to the adapter itself: it can
/// report health and keep custom metrics (through
/// [`AdapterContext::health`](crate::AdapterContext::health) and
/// [`AdapterContext::metrics`](crate::AdapterContext::metrics)) and read a
/// snapshot, but cannot change its run state, restarts or command counts.
///
/// ```compile_fail
/// fn forge(stats: &heimr_core::InstanceStats) {
///     stats.run_started("2030-01-01T00:00:00Z".parse().unwrap()); // no such method
/// }
/// ```
#[derive(Debug, Clone)]
pub struct InstanceStats(StatsHandle);

impl InstanceStats {
    /// A copy of the current stats.
    pub fn snapshot(&self) -> AdapterStats {
        self.0.snapshot()
    }

    pub(crate) fn handle(&self) -> &StatsHandle {
        &self.0
    }
}

impl StatsHandle {
    /// The restricted view to hand to the instance's adapter.
    pub fn for_instance(&self) -> InstanceStats {
        InstanceStats(self.clone())
    }

    /// Fresh stats for instance `name` of `kind` in `mode`.
    pub fn new(name: impl Into<String>, kind: impl Into<String>, mode: Mode) -> Self {
        Self(Arc::new(Mutex::new(AdapterStats {
            name: name.into(),
            kind: kind.into(),
            mode,
            ..AdapterStats::default()
        })))
    }

    fn lock(&self) -> MutexGuard<'_, AdapterStats> {
        // Every update is a few field writes; a poisoned lock is still sound.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A copy of the current stats.
    pub fn snapshot(&self) -> AdapterStats {
        self.lock().clone()
    }

    /// Record that `run` started (a restart if it ran before).
    pub fn run_started(&self, at: Timestamp) {
        let mut s = self.lock();
        if s.started_at.is_some() {
            s.restarts += 1;
        }
        s.run_state = RunState::Running;
        s.started_at = Some(at);
        s.exited_at = None;
        s.exit_error = None;
        s.next_restart_at = None;
    }

    /// Record that `run` returned at shutdown (`error` if it failed).
    pub fn run_stopped(&self, at: Timestamp, error: Option<String>) {
        let mut s = self.lock();
        s.run_state = RunState::Stopped;
        s.exited_at = Some(at);
        s.exit_error = error;
    }

    /// Record a failure: health becomes failed. With `restart_at`, the
    /// instance is in backoff; without, it stays down.
    pub fn run_failed(
        &self,
        at: Timestamp,
        error: String,
        consecutive: u32,
        restart_at: Option<Timestamp>,
    ) {
        let mut s = self.lock();
        s.run_state = if restart_at.is_some() {
            RunState::Backoff
        } else {
            RunState::Down
        };
        s.exited_at = Some(at);
        if s.health != HealthStatus::Failed {
            s.health_since = Some(at);
        }
        s.health = HealthStatus::Failed;
        s.health_reason = Some(error.clone());
        s.exit_error = Some(error);
        s.consecutive_failures = consecutive;
        s.next_restart_at = restart_at;
    }

    pub(crate) fn emitted(&self, record: &Record) {
        let mut s = self.lock();
        match record {
            Record::Reading(_) => s.readings += 1,
            Record::Climate(_) | Record::Fan(_) | Record::Switch(_) => s.states += 1,
            Record::Event(_) => s.events += 1,
        }
        let time = record.time();
        if s.last_emit_at.is_none_or(|t| t < time) {
            s.last_emit_at = Some(time);
        }
    }

    pub(crate) fn emit_failed(&self, provenance: bool) {
        let mut s = self.lock();
        s.emit_errors += 1;
        if provenance {
            s.provenance_rejections += 1;
        }
    }

    pub(crate) fn command(&self, result: &Result<Outcome, Rejection>) {
        let mut s = self.lock();
        s.commands_offered += 1;
        match result {
            Ok(Outcome::Delivered { .. }) => s.commands_delivered += 1,
            Ok(Outcome::Shadowed { .. }) => s.commands_shadowed += 1,
            Err(r) if r.is_policy() => s.commands_rejected_policy += 1,
            Err(Rejection::RateLimited { .. }) => s.commands_rejected_rate_limit += 1,
            Err(Rejection::AuditUnavailable | Rejection::AuditTimedOut) => {
                s.commands_audit_failed += 1;
            }
            Err(_) => s.commands_failed += 1,
        }
    }

    pub(crate) fn command_dropped(&self) {
        self.lock().commands_dropped += 1;
    }

    /// Set health; returns the previous status and reason if it changed.
    fn set_health(
        &self,
        at: Timestamp,
        status: HealthStatus,
        reason: Option<String>,
    ) -> Option<(HealthStatus, Option<String>)> {
        let mut s = self.lock();
        if s.health == status && s.health_reason == reason {
            return None;
        }
        let previous = (s.health, s.health_reason.take());
        s.health = status;
        s.health_reason = reason;
        s.health_since = Some(at);
        Some(previous)
    }
}

/// Reports an adapter's health in one line. Changes are recorded in its
/// [`AdapterStats`] and emitted as an [`Event::ADAPTER_HEALTH`] event;
/// repeating the same status and reason does nothing.
///
/// ```no_run
/// # async fn f(ctx: &heimr_core::AdapterContext<()>) -> Result<(), heimr_core::EmitError> {
/// ctx.health().degraded("cloud API returned 503; retrying").await?;
/// ctx.health().ok().await?;
/// # Ok(()) }
/// ```
#[derive(Debug, Clone)]
pub struct HealthReporter {
    stats: StatsHandle,
    emitter: Emitter,
    clock: SharedClock,
}

impl HealthReporter {
    pub(crate) fn new(stats: StatsHandle, emitter: Emitter, clock: SharedClock) -> Self {
        Self {
            stats,
            emitter,
            clock,
        }
    }

    /// Working normally.
    pub async fn ok(&self) -> Result<(), EmitError> {
        self.report(HealthStatus::Ok, None).await
    }

    /// Working, but impaired.
    pub async fn degraded(&self, reason: impl Into<String>) -> Result<(), EmitError> {
        self.report(HealthStatus::Degraded, Some(reason.into()))
            .await
    }

    /// Not working.
    pub async fn failed(&self, reason: impl Into<String>) -> Result<(), EmitError> {
        self.report(HealthStatus::Failed, Some(reason.into())).await
    }

    async fn report(&self, status: HealthStatus, reason: Option<String>) -> Result<(), EmitError> {
        let now = self.clock.now();
        let Some((previous, _)) = self.stats.set_health(now, status, reason.clone()) else {
            return Ok(());
        };
        match status {
            HealthStatus::Ok => tracing::info!(health = status.as_str(), "adapter health"),
            _ => tracing::warn!(
                health = status.as_str(),
                reason = reason.as_deref().unwrap_or(""),
                "adapter health"
            ),
        }
        let event = Event::new(
            now,
            Event::ADAPTER_HEALTH,
            self.emitter.source(),
            json!({ "status": status, "reason": reason, "previous": previous }),
        );
        self.emitter.emit_reserved(event).await
    }
}

/// Custom counters and gauges, kept in the instance's [`AdapterStats`].
/// Names follow the id rules (ASCII letters, digits, `.` `_` `:` `-`);
/// invalid names are ignored with a warning.
#[derive(Debug, Clone)]
pub struct Metrics {
    stats: StatsHandle,
}

impl Metrics {
    pub(crate) fn new(stats: StatsHandle) -> Self {
        Self { stats }
    }

    fn valid(name: &str) -> bool {
        let ok = crate::id::validate(name).is_ok();
        if !ok {
            tracing::warn!(metric = name, "ignoring metric with an invalid name");
        }
        ok
    }

    /// Add `by` to counter `name`.
    pub fn counter(&self, name: &str, by: u64) {
        if Self::valid(name) {
            let mut s = self.stats.lock();
            let c = s.counters.entry(name.to_owned()).or_default();
            *c = c.saturating_add(by);
        }
    }

    /// Set gauge `name` to `value`.
    pub fn gauge(&self, name: &str, value: f64) {
        if Self::valid(name) {
            self.stats.lock().gauges.insert(name.to_owned(), value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use crate::id::DeviceId;
    use crate::record::Reading;
    use crate::units::{Metric, Unit};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn t0() -> Timestamp {
        "2030-01-01T00:00:00Z".parse().unwrap()
    }

    #[tokio::test]
    async fn emitter_counts_and_health_dedupes() {
        let (tx, mut rx) = mpsc::channel(16);
        let stats = StatsHandle::new("a", "test", Mode::Shadow);
        let em = Emitter::for_adapter("a", tx).with_stats(stats.clone());
        let dev = DeviceId::new("d").unwrap();
        let ok = Reading::new(
            t0(),
            "a",
            dev.clone(),
            Metric::TEMPERATURE,
            20.0,
            Unit::Celsius,
        );
        em.emit(ok).await.unwrap();
        let foreign = Reading::new(
            t0(),
            "b",
            dev.clone(),
            Metric::TEMPERATURE,
            20.0,
            Unit::Celsius,
        );
        assert!(em.emit(foreign).await.is_err());
        let nan = Reading::new(t0(), "a", dev, Metric::TEMPERATURE, f64::NAN, Unit::Celsius);
        assert!(em.emit(nan).await.is_err());

        let clock = ManualClock::new(t0());
        let health = HealthReporter::new(stats.clone(), em.clone(), Arc::new(clock.clone()));
        health.degraded("slow").await.unwrap();
        health.degraded("slow").await.unwrap(); // no change, no event
        clock.advance(jiff::SignedDuration::from_mins(1));
        health.ok().await.unwrap();

        let s = stats.snapshot();
        assert_eq!((s.readings, s.events, s.emit_errors), (1, 2, 2));
        assert_eq!(s.provenance_rejections, 1);
        assert_eq!(s.health, HealthStatus::Ok);
        assert_eq!(
            s.health_since,
            Some(t0() + jiff::SignedDuration::from_mins(1))
        );
        let mut kinds = vec![];
        while let Ok(env) = rx.try_recv() {
            if let Record::Event(e) = env.record {
                assert_eq!(e.actor, "a");
                kinds.push(e.payload["status"].as_str().unwrap().to_owned());
            }
        }
        assert_eq!(kinds, ["degraded", "ok"]);
    }

    #[test]
    fn run_lifecycle_and_metrics() {
        let stats = StatsHandle::new("a", "test", Mode::Live);
        stats.run_started(t0());
        stats.run_failed(t0(), "boom".into(), 1, Some(t0()));
        let s = stats.snapshot();
        assert_eq!(
            (s.health, s.run_state),
            (HealthStatus::Failed, RunState::Backoff)
        );
        stats.run_started(t0());
        let m = Metrics::new(stats.clone());
        m.counter("polls", 2);
        m.counter("polls", 3);
        m.counter("bad name", 1);
        m.gauge("queue", 4.5);
        let s = stats.snapshot();
        assert_eq!(s.restarts, 1);
        assert_eq!(s.run_state, RunState::Running);
        assert_eq!(s.exit_error, None);
        assert_eq!(s.counters.len(), 1);
        assert_eq!(s.counters["polls"], 5);
        assert_eq!(s.gauges["queue"], 4.5);
        stats.run_failed(t0(), "fatal".into(), 2, None);
        assert_eq!(stats.snapshot().run_state, RunState::Down);
    }
}
