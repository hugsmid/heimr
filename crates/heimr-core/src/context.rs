//! What an adapter is given to run: [`AdapterContext`].

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, mpsc};

use crate::clock::SharedClock;
use crate::domain::Command;
use crate::id::{self, CommandId, DeviceId, IdError};
use crate::record::{
    Basis, DEFAULT_FORECAST_HORIZON, Envelope, Event, Forecast, ForecastError, Reading, Record,
};
use crate::series::{SeriesRole, SeriesTemplate};
use crate::shutdown::Shutdown;
use crate::site::{Currency, Site};
use crate::telemetry::{HealthReporter, InstanceStats, Metrics, StatsHandle};
use crate::units::{Metric, Unit};

/// Emitting a record failed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum EmitError {
    /// The app is no longer receiving (shutting down).
    #[error("the app is no longer receiving records")]
    Closed,
    /// A reading named a different source than the emitting instance.
    #[error("reading source {found:?} does not match adapter instance {expected:?}")]
    ForeignSource {
        /// The emitting instance.
        expected: String,
        /// The source in the reading.
        found: String,
    },
    /// An event named a different actor than the emitting instance.
    #[error("event actor {found:?} does not match adapter instance {expected:?}")]
    ForeignActor {
        /// The emitting instance.
        expected: String,
        /// The actor in the event.
        found: String,
    },
    /// An adapter tried to emit an event of a kind reserved for the core.
    #[error("event kind {0:?} is reserved for the core")]
    ReservedKind(String),
    /// An event kind broke the id rules.
    #[error("invalid event kind {0:?}: {1}")]
    InvalidKind(String, IdError),
    /// A forecast failed validation ([`Forecast::validate`]).
    #[error(transparent)]
    InvalidForecast(#[from] ForecastError),
    /// A record in a money unit came from a kind that declares no source
    /// currency
    /// ([`AdapterDescriptor::source_currency`](crate::AdapterDescriptor::source_currency)).
    #[error("{unit} is a money unit, but the adapter kind declares no source currency")]
    UndeclaredCurrency {
        /// The unit.
        unit: Unit,
    },
    /// A reading or forecast's metric has no series template on the kind's
    /// descriptor ([`AdapterDescriptor::series`](crate::AdapterDescriptor::series)).
    #[error(
        "{} {metric} has no series template: declare one on the adapter kind's descriptor",
        if *forecast { "forecast" } else { "reading" }
    )]
    NoTemplate {
        /// The metric.
        metric: Metric,
        /// Whether it was a forecast.
        forecast: bool,
    },
    /// A forecast's basis does not match its template's role: a
    /// [`Basis::Published`] forecast needs a [`SeriesRole::Scheduled`]
    /// template, a [`Basis::Modelled`] one a [`SeriesRole::Forecast`]
    /// template.
    #[error(
        "forecast {metric}: a {} forecast needs a {} series template, not a {} one",
        basis.as_str(),
        needs.as_str(),
        role.as_str()
    )]
    RoleMismatch {
        /// Metric.
        metric: Metric,
        /// The forecast's basis.
        basis: Basis,
        /// The role it needs.
        needs: SeriesRole,
        /// The template's role.
        role: SeriesRole,
    },
    /// A reading or forecast of a versioned series carries no `changed_at`.
    #[error("{metric} on {device}: the series is versioned, but the record carries no changed_at")]
    MissingChangedAt {
        /// Device.
        device: DeviceId,
        /// Metric.
        metric: Metric,
    },
    /// A reading of a series that is not versioned carries a `changed_at`.
    #[error("{metric} on {device}: changed_at on a series that is not versioned")]
    UnexpectedChangedAt {
        /// Device.
        device: DeviceId,
        /// Metric.
        metric: Metric,
    },
    /// A reading (or a forecast, by `valid_from`) of an interval series is
    /// not stamped at an interval start.
    #[error("{metric} on {device}: {t} does not start an interval of {length:#}")]
    Misaligned {
        /// Device.
        device: DeviceId,
        /// Metric.
        metric: Metric,
        /// The record's time.
        t: Timestamp,
        /// The interval length.
        length: SignedDuration,
    },
    /// A reading or forecast's unit differs from its series template's.
    #[error("{metric} on {device}: unit {found} differs from its series template's {expected}")]
    UnitMismatch {
        /// Device.
        device: DeviceId,
        /// Metric.
        metric: Metric,
        /// The template's unit.
        expected: Unit,
        /// The record's unit.
        found: Unit,
    },
}

impl EmitError {
    /// Whether the record was refused for claiming someone else's identity
    /// or a core-reserved kind.
    pub fn is_provenance(&self) -> bool {
        matches!(
            self,
            Self::ForeignSource { .. } | Self::ForeignActor { .. } | Self::ReservedKind(_)
        )
    }
}

/// Sends records to the app, stamped with a fixed source. Cheap to clone
/// (e.g. into a sub-task of an adapter).
///
/// An adapter's emitter ([`Emitter::for_adapter`]) enforces provenance:
/// readings must name the instance as `source`, events must name it as
/// `actor` and must not use a core-reserved kind
/// ([`Event::is_reserved_kind`]). It also validates forecasts
/// ([`Forecast::validate`] against its horizon, and
/// [`Forecast::validate_issued`] against its clock if it has one), and
/// refuses money units unless the kind declared a source currency
/// ([`Emitter::with_source_currency`]). Every reading and forecast needs
/// a series template for its metric with the same unit
/// ([`Emitter::with_series`]): an adapter emitter without templates
/// refuses them all. A reading or forecast of a versioned series must
/// carry `changed_at`; one of any other series must not. Values are never checked: NaN and
/// ±∞ in readings and forecasts are forwarded unchanged (the store will record
/// them as bad quality). Refusals are counted in the
/// instance's stats. Only core emitters ([`Emitter::core`]) may emit
/// reserved kinds or other actors; adapters never get one.
#[derive(Debug, Clone)]
pub struct Emitter {
    source: Arc<str>,
    tx: mpsc::Sender<Envelope>,
    stats: Option<StatsHandle>,
    core: bool,
    currency: Option<Currency>,
    horizon: SignedDuration,
    clock: Option<SharedClock>,
    series: Arc<[SeriesTemplate]>,
}

impl Emitter {
    /// An adapter instance's emitter: stamps `instance` on everything sent
    /// into `tx` and enforces provenance.
    pub fn for_adapter(instance: impl Into<Arc<str>>, tx: mpsc::Sender<Envelope>) -> Self {
        Self {
            source: instance.into(),
            tx,
            stats: None,
            core: false,
            currency: None,
            horizon: DEFAULT_FORECAST_HORIZON,
            clock: None,
            series: Arc::new([]),
        }
    }

    /// A core emitter (the app, the actuator): may emit reserved event kinds
    /// on behalf of any actor. Readings must still name `source`.
    pub fn core(source: impl Into<Arc<str>>, tx: mpsc::Sender<Envelope>) -> Self {
        Self {
            source: source.into(),
            tx,
            stats: None,
            core: true,
            currency: None,
            horizon: DEFAULT_FORECAST_HORIZON,
            clock: None,
            series: Arc::new([]),
        }
    }

    /// Count everything emitted (and every failure) in `stats`.
    #[must_use]
    pub fn with_stats(mut self, stats: StatsHandle) -> Self {
        self.stats = Some(stats);
        self
    }

    /// Allow money units: the kind's declared source currency (`None`, the
    /// default, refuses them).
    #[must_use]
    pub fn with_source_currency(mut self, currency: Option<Currency>) -> Self {
        self.currency = currency;
        self
    }

    /// The kind's series templates
    /// ([`AdapterDescriptor::series`](crate::AdapterDescriptor::series)):
    /// a reading or forecast is accepted only if its metric has one (as a
    /// reading or as a forecast) with the same unit. Core emitters are not
    /// restricted.
    #[must_use]
    pub fn with_series(mut self, series: impl IntoIterator<Item = SeriesTemplate>) -> Self {
        self.series = series.into_iter().collect();
        self
    }

    /// The series templates readings and forecasts are checked against.
    pub fn series(&self) -> &[SeriesTemplate] {
        &self.series
    }

    /// Refuse forecasts reaching further than `horizon` past their issue
    /// time (default [`DEFAULT_FORECAST_HORIZON`]).
    #[must_use]
    pub fn with_forecast_horizon(mut self, horizon: SignedDuration) -> Self {
        self.horizon = horizon;
        self
    }

    /// Check forecasts' `issued_at` against this clock: none may be later
    /// than now plus [`MAX_FUTURE_SKEW`](crate::MAX_FUTURE_SKEW). Without
    /// a clock (the default) `issued_at` is not checked; the app and the
    /// test kit always set one.
    #[must_use]
    pub fn with_clock(mut self, clock: SharedClock) -> Self {
        self.clock = Some(clock);
        self
    }

    /// The source stamped on every record.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// How far past its issue time a forecast may reach.
    pub fn forecast_horizon(&self) -> SignedDuration {
        self.horizon
    }

    /// The source currency money units are allowed in, if any.
    pub fn source_currency(&self) -> Option<Currency> {
        self.currency
    }

    /// Validate and send a record, waiting if the app is behind
    /// (back-pressure).
    pub async fn emit(&self, record: impl Into<Record>) -> Result<(), EmitError> {
        self.emit_checked(record.into(), self.core).await
    }

    /// Emit an event of a reserved kind attributed to this emitter's source
    /// (health reports).
    pub(crate) async fn emit_reserved(&self, event: Event) -> Result<(), EmitError> {
        self.emit_checked(Record::Event(event), true).await
    }

    /// Send without waiting; drops the record (returning `false`) if the
    /// app is behind. For core copies that must never apply back-pressure.
    pub(crate) fn try_emit(&self, record: Record) -> bool {
        if self.check(&record, true).is_err() {
            return false;
        }
        self.tx
            .try_send(Envelope::new(self.source.to_string(), record))
            .is_ok()
    }

    async fn emit_checked(&self, record: Record, allow_reserved: bool) -> Result<(), EmitError> {
        let result = match self.check(&record, allow_reserved) {
            Ok(()) => {
                if let Some(stats) = &self.stats {
                    // Counted before sending so the count never lags sinks.
                    stats.emitted(&record);
                }
                self.tx
                    .send(Envelope::new(self.source.to_string(), record))
                    .await
                    .map_err(|_| EmitError::Closed)
            }
            Err(e) => Err(e),
        };
        if let Err(e) = &result {
            if let Some(stats) = &self.stats {
                stats.emit_failed(e);
            }
            if e.is_provenance() {
                tracing::warn!(source = %self.source, error = %e, "record refused");
            } else {
                tracing::debug!(source = %self.source, error = %e, "emit failed");
            }
        }
        result
    }

    fn check(&self, record: &Record, allow_reserved: bool) -> Result<(), EmitError> {
        match record {
            Record::Reading(r) => {
                if r.source != *self.source {
                    return Err(EmitError::ForeignSource {
                        expected: self.source.to_string(),
                        found: r.source.clone(),
                    });
                }
                self.check_unit(r.unit)?;
                let t = self.check_series(&r.device, &r.metric, r.unit, false, r.time)?;
                Self::check_changed_at(t, &r.device, &r.metric, r.changed_at)?;
            }
            Record::Forecast(f) => {
                f.validate(self.horizon)?;
                if let Some(clock) = &self.clock {
                    f.validate_issued(clock.now())?;
                }
                self.check_unit(f.unit)?;
                let t = self.check_series(&f.device, &f.metric, f.unit, true, f.valid_from)?;
                Self::check_role(t, f)?;
                Self::check_changed_at(t, &f.device, &f.metric, f.changed_at)?;
            }
            Record::Event(e) => {
                id::validate(&e.kind).map_err(|err| EmitError::InvalidKind(e.kind.clone(), err))?;
                if !self.core {
                    if e.actor != *self.source {
                        return Err(EmitError::ForeignActor {
                            expected: self.source.to_string(),
                            found: e.actor.clone(),
                        });
                    }
                    if !allow_reserved && Event::is_reserved_kind(&e.kind) {
                        return Err(EmitError::ReservedKind(e.kind.clone()));
                    }
                }
            }
            // State is keyed by the core-stamped source, so a zone or device
            // id can never address another instance's state.
            Record::Climate(_) | Record::Fan(_) | Record::Switch(_) => {}
        }
        Ok(())
    }
}

impl Emitter {
    /// Money only from a kind that declared its currency (core emitters
    /// are not adapters and are not restricted).
    fn check_unit(&self, unit: Unit) -> Result<(), EmitError> {
        if unit.is_money() && self.currency.is_none() && !self.core {
            Err(EmitError::UndeclaredCurrency { unit })
        } else {
            Ok(())
        }
    }
}

impl Emitter {
    /// A published forecast is scheduled; a modelled one is a forecast.
    fn check_role(template: Option<&SeriesTemplate>, f: &Forecast) -> Result<(), EmitError> {
        let Some(t) = template else { return Ok(()) };
        let needs = match f.basis {
            Basis::Published => SeriesRole::Scheduled,
            Basis::Modelled => SeriesRole::Forecast,
        };
        if t.role == needs {
            Ok(())
        } else {
            Err(EmitError::RoleMismatch {
                metric: f.metric.clone(),
                basis: f.basis,
                needs,
                role: t.role,
            })
        }
    }

    /// A versioned series needs `changed_at`; any other must not have one.
    fn check_changed_at(
        template: Option<&SeriesTemplate>,
        device: &DeviceId,
        metric: &Metric,
        changed_at: Option<u64>,
    ) -> Result<(), EmitError> {
        match (template.map(|t| t.versioned), changed_at) {
            (Some(true), None) => Err(EmitError::MissingChangedAt {
                device: device.clone(),
                metric: metric.clone(),
            }),
            (Some(false), Some(_)) => Err(EmitError::UnexpectedChangedAt {
                device: device.clone(),
                metric: metric.clone(),
            }),
            _ => Ok(()),
        }
    }

    /// An adapter's reading or forecast needs a template with its unit.
    /// Returns the template, if any (core emitters are not restricted).
    fn check_series(
        &self,
        device: &DeviceId,
        metric: &Metric,
        unit: Unit,
        forecast: bool,
        t: Timestamp,
    ) -> Result<Option<&SeriesTemplate>, EmitError> {
        let template = crate::adapter::series_for(&self.series, metric, forecast);
        if self.core {
            return Ok(template);
        }
        let template = template.ok_or_else(|| EmitError::NoTemplate {
            metric: metric.clone(),
            forecast,
        })?;
        if template.unit != unit {
            return Err(EmitError::UnitMismatch {
                device: device.clone(),
                metric: metric.clone(),
                expected: template.unit,
                found: unit,
            });
        }
        if let Some(length) = template.interval_length()
            && !template.is_aligned(t)
        {
            return Err(EmitError::Misaligned {
                device: device.clone(),
                metric: metric.clone(),
                t,
                length,
            });
        }
        Ok(Some(template))
    }
}

/// A command delivered to an adapter instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CommandEnvelope {
    /// Actuator-assigned id; matches the `id` in the audit record.
    pub id: CommandId,
    /// When the actuator accepted it (wall time).
    pub issued_at: Timestamp,
    /// The command, including its [`Hold`](crate::Hold).
    pub command: Command,
}

impl CommandEnvelope {
    /// An envelope (for unit-testing an adapter's command handling; only the
    /// actuator can deliver one).
    pub fn new(id: CommandId, issued_at: Timestamp, command: Command) -> Self {
        Self {
            id,
            issued_at,
            command,
        }
    }
}

#[derive(Debug)]
struct MailboxState {
    queue: VecDeque<CommandEnvelope>,
    reserved: usize,
    capacity: usize,
    open: bool,
}

/// One live instance run's command queue. Created by the actuator when the
/// instance starts and closed (and drained) when it stops, so commands are
/// never carried over to a later run.
#[derive(Debug)]
pub(crate) struct Mailbox {
    state: Mutex<MailboxState>,
    notify: Notify,
}

pub(crate) enum ReserveError {
    Full,
    Closed,
}

impl Mailbox {
    pub(crate) fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(MailboxState {
                queue: VecDeque::new(),
                reserved: 0,
                capacity,
                open: true,
            }),
            notify: Notify::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, MailboxState> {
        // Every mutation is a push, pop or flag write: still sound if poisoned.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reserve a slot (checked and taken atomically).
    pub(crate) fn try_reserve(self: &Arc<Self>) -> Result<Reservation, ReserveError> {
        let mut s = self.lock();
        if !s.open {
            return Err(ReserveError::Closed);
        }
        if s.queue.len() + s.reserved >= s.capacity {
            return Err(ReserveError::Full);
        }
        s.reserved += 1;
        Ok(Reservation {
            mailbox: Some(self.clone()),
        })
    }

    /// Close and return everything not yet received.
    pub(crate) fn close(&self) -> Vec<CommandEnvelope> {
        let drained = {
            let mut s = self.lock();
            s.open = false;
            s.queue.drain(..).collect()
        };
        self.notify.notify_waiters();
        self.notify.notify_one();
        drained
    }
}

/// A reserved slot in a [`Mailbox`]; released if dropped unused.
pub(crate) struct Reservation {
    mailbox: Option<Arc<Mailbox>>,
}

impl Reservation {
    /// Deliver into the reserved slot, unless the mailbox closed meanwhile.
    pub(crate) fn send(mut self, env: CommandEnvelope) -> Result<(), CommandEnvelope> {
        let Some(mb) = self.mailbox.take() else {
            return Err(env);
        };
        {
            let mut s = mb.lock();
            s.reserved = s.reserved.saturating_sub(1);
            if !s.open {
                return Err(env);
            }
            s.queue.push_back(env);
        }
        mb.notify.notify_one();
        Ok(())
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(mb) = self.mailbox.take() {
            let mut s = mb.lock();
            s.reserved = s.reserved.saturating_sub(1);
        }
    }
}

/// An adapter instance's incoming commands.
///
/// Only the [`Actuator`](crate::Actuator) creates a connected `Commands`,
/// and only for an instance that has write capability *and* is configured
/// `live`, once per run. In shadow mode no queue exists at all, so there is
/// nothing an adapter (or a bug in the app) could receive from. Adapters
/// cannot build a connected one themselves:
///
/// ```compile_fail
/// let commands = heimr_core::Commands::live(todo!()); // private
/// ```
#[derive(Debug)]
pub struct Commands {
    mailbox: Option<Arc<Mailbox>>,
}

impl Commands {
    pub(crate) fn live(mailbox: Arc<Mailbox>) -> Self {
        Self {
            mailbox: Some(mailbox),
        }
    }

    /// A handle that never yields a command (shadow mode, read-only
    /// adapters, tests).
    pub fn disconnected() -> Self {
        Self { mailbox: None }
    }

    /// Whether commands can ever arrive.
    pub fn is_live(&self) -> bool {
        self.mailbox.is_some()
    }

    /// The next command. Never resolves for a disconnected handle (so it is
    /// safe in a `tokio::select!` loop); returns `None` once this run's queue
    /// is closed (the instance is being stopped). Cancel-safe.
    pub async fn recv(&mut self) -> Option<CommandEnvelope> {
        let Some(mb) = &self.mailbox else {
            return std::future::pending().await;
        };
        loop {
            let notified = mb.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut s = mb.lock();
                if let Some(cmd) = s.queue.pop_front() {
                    return Some(cmd);
                }
                if !s.open {
                    return None;
                }
            }
            notified.await;
        }
    }
}

/// The pieces a host hands to an [`AdapterInstance`](crate::AdapterInstance).
#[derive(Debug)]
#[non_exhaustive]
pub struct ContextParts {
    /// The instance's emitter ([`Emitter::for_adapter`]).
    pub emitter: Emitter,
    /// Commands from [`Actuator::attach`](crate::Actuator::attach).
    pub commands: Commands,
    /// Clock.
    pub clock: SharedClock,
    /// Shutdown signal.
    pub shutdown: Shutdown,
    /// Private, persistent directory for this instance (tokens, caches).
    pub state_dir: PathBuf,
    /// The instance's stats, restricted to what an adapter may change
    /// ([`StatsHandle::for_instance`]).
    pub stats: InstanceStats,
    /// The site's time zone and currency.
    pub site: Site,
}

impl ContextParts {
    /// Parts with a disconnected command handle. The emitter checks
    /// forecasts' issue times against `clock` ([`Emitter::with_clock`]),
    /// so the adapter's clock and the emitter's are always the same.
    pub fn new(
        emitter: Emitter,
        clock: SharedClock,
        shutdown: Shutdown,
        state_dir: PathBuf,
        stats: InstanceStats,
        site: Site,
    ) -> Self {
        Self {
            emitter: emitter.with_clock(clock.clone()),
            commands: Commands::disconnected(),
            clock,
            shutdown,
            state_dir,
            stats,
            site,
        }
    }

    /// Use this command handle.
    #[must_use]
    pub fn with_commands(mut self, commands: Commands) -> Self {
        self.commands = commands;
        self
    }
}

/// Everything an adapter may use. Built by the app; see
/// [`Adapter`](crate::Adapter).
#[derive(Debug)]
pub struct AdapterContext<C> {
    config: Arc<C>,
    parts: ContextParts,
    live: bool,
}

impl<C> AdapterContext<C> {
    /// Assemble a context.
    pub fn new(parts: ContextParts, config: Arc<C>) -> Self {
        let live = parts.commands.is_live();
        Self {
            config,
            parts,
            live,
        }
    }

    /// Instance name: the `source` of every record and the `actor` of every
    /// event this instance emits.
    pub fn name(&self) -> &str {
        self.parts.emitter.source()
    }

    /// The typed config.
    pub fn config(&self) -> &C {
        &self.config
    }

    /// The clock.
    pub fn clock(&self) -> &SharedClock {
        &self.parts.clock
    }

    /// Current wall time from the context clock.
    pub fn now(&self) -> Timestamp {
        self.parts.clock.now()
    }

    /// Sleep until `deadline` (wall time) on the context clock.
    pub async fn sleep_until(&self, deadline: Timestamp) {
        self.parts.clock.sleep_until(deadline).await;
    }

    /// Sleep for `duration` (monotonic) on the context clock.
    pub async fn sleep(&self, duration: SignedDuration) {
        self.parts.clock.sleep(duration).await;
    }

    /// A [`Reading`] stamped with this instance as source and the current
    /// time; adjust it with `with_time` or `with_quality` before emitting.
    pub fn reading(&self, device: DeviceId, metric: Metric, value: f64, unit: Unit) -> Reading {
        Reading::new(self.now(), self.name(), device, metric, value, unit)
    }

    /// A [`Forecast`] issued now (the context clock), valid at this
    /// instant; set its period with `valid_between` or `valid_at`, and its
    /// issue time with `with_issued_at` if the source reports one. The
    /// emitter checks it against [`AdapterContext::forecast_horizon`].
    pub fn forecast(
        &self,
        device: DeviceId,
        metric: Metric,
        value: f64,
        unit: Unit,
        basis: Basis,
    ) -> Forecast {
        Forecast::new(self.now(), device, metric, value, unit, basis)
    }

    /// How far past its issue time a forecast may reach (the app's
    /// `forecast_horizon`). Drop or trim forecasts beyond it: the emitter
    /// refuses them.
    pub fn forecast_horizon(&self) -> SignedDuration {
        self.parts.emitter.forecast_horizon()
    }

    /// An [`Event`] stamped with this instance as actor and the current
    /// time. Kinds follow the id rules and must not be core-reserved
    /// ([`Event::is_reserved_kind`]).
    pub fn event(&self, kind: impl Into<String>, payload: serde_json::Value) -> Event {
        Event::new(self.now(), kind, self.name(), payload)
    }

    /// Send a record to the app.
    pub async fn emit(&self, record: impl Into<Record>) -> Result<(), EmitError> {
        self.parts.emitter.emit(record).await
    }

    /// A clone of the emitter, for sub-tasks.
    pub fn emitter(&self) -> Emitter {
        self.parts.emitter.clone()
    }

    /// Take the incoming command handle (never yields unless live; see
    /// [`Commands`]). Owning it lets an adapter `select!` on commands and
    /// the context at once. A second call returns a disconnected handle.
    pub fn take_commands(&mut self) -> Commands {
        std::mem::replace(&mut self.parts.commands, Commands::disconnected())
    }

    /// Whether this instance receives commands (write-capable and `live`).
    pub fn is_live(&self) -> bool {
        self.live
    }

    /// The shutdown signal.
    pub fn shutdown(&self) -> &Shutdown {
        &self.parts.shutdown
    }

    /// Private, persistent state directory.
    pub fn state_dir(&self) -> &Path {
        &self.parts.state_dir
    }

    /// The site's time zone (for calendar days and presentation; records
    /// stay UTC) and currency (prices in major units).
    pub fn site(&self) -> &Site {
        &self.parts.site
    }

    /// Report health in one line; see [`HealthReporter`].
    pub fn health(&self) -> HealthReporter {
        HealthReporter::new(
            self.parts.stats.handle().clone(),
            self.parts.emitter.clone(),
            self.parts.clock.clone(),
        )
    }

    /// Custom counters and gauges; see [`Metrics`].
    pub fn metrics(&self) -> Metrics {
        Metrics::new(self.parts.stats.handle().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actuator::Mode;
    use crate::domain::{ClimateMode, ClimateState};
    use crate::id::ZoneId;
    use crate::record::MAX_FUTURE_SKEW;
    use crate::series::SeriesTemplate;
    use proptest::prelude::*;

    /// Templates for the temperature readings and `unit_rate` forecasts
    /// (in `unit`) these tests emit.
    fn series(unit: Unit) -> [SeriesTemplate; 2] {
        [
            SeriesTemplate::gauge(
                Metric::TEMPERATURE,
                Unit::Celsius,
                SignedDuration::from_mins(1),
            ),
            SeriesTemplate::scheduled(
                Metric::new("unit_rate").unwrap(),
                unit,
                SignedDuration::from_hours(1),
            )
            .versioned(),
        ]
    }

    fn t0() -> Timestamp {
        "2030-01-01T00:00:00Z".parse().unwrap()
    }

    #[tokio::test]
    async fn mailbox_close_drains_and_ends_recv() {
        let mb = Mailbox::new(2);
        let mut cmds = Commands::live(mb.clone());
        let env = |seq| {
            CommandEnvelope::new(
                CommandId::new(1, seq).unwrap(),
                t0(),
                crate::ClimateCommand::resume_schedule(ZoneId::new("z").unwrap()).into(),
            )
        };
        let Ok(r1) = mb.try_reserve() else { panic!() };
        let Ok(r2) = mb.try_reserve() else { panic!() };
        assert!(matches!(mb.try_reserve(), Err(ReserveError::Full)));
        drop(r2); // released
        assert!(r1.send(env(1)).is_ok());
        assert_eq!(cmds.recv().await.unwrap().id.seq(), 1);
        let Ok(r3) = mb.try_reserve() else { panic!() };
        assert!(r3.send(env(2)).is_ok());
        let Ok(r4) = mb.try_reserve() else { panic!() };
        let drained = mb.close();
        assert_eq!(drained.len(), 1);
        assert!(r4.send(env(3)).is_err(), "nothing enters a closed mailbox");
        assert!(matches!(mb.try_reserve(), Err(ReserveError::Closed)));
        assert_eq!(cmds.recv().await, None);
    }

    #[derive(Debug, Clone)]
    enum Attempt {
        Reading(String),
        Event(String, String),
        State(String),
    }

    fn attempt() -> impl Strategy<Value = Attempt> {
        let name = prop_oneof![
            Just("inst".to_owned()),
            Just("other".to_owned()),
            Just("actuator".to_owned()),
            Just("heimr".to_owned()),
            "[a-z]{1,6}",
        ];
        let kind = prop_oneof![
            Just("command".to_owned()),
            Just("command_dropped".to_owned()),
            Just("adapter_health".to_owned()),
            Just("adapter_failed".to_owned()),
            Just("heimr.x".to_owned()),
            Just("door_opened".to_owned()),
            "[a-z_]{1,12}",
        ];
        prop_oneof![
            name.clone().prop_map(Attempt::Reading),
            (name.clone(), kind).prop_map(|(a, k)| Attempt::Event(a, k)),
            "[a-z]{1,6}".prop_map(Attempt::State),
        ]
    }

    proptest! {
        /// Whatever an adapter tries to emit, everything that gets through
        /// is stamped with its own instance name (as source, and as actor of
        /// events), no reserved kind gets through, and every refusal is
        /// counted.
        #[test]
        fn provenance_cannot_be_forged(attempts in proptest::collection::vec(attempt(), 1..60)) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let (tx, mut rx) = mpsc::channel(1024);
                let stats = StatsHandle::new("inst", "test", Mode::Shadow);
                let em = Emitter::for_adapter("inst", tx).with_stats(stats.clone()).with_series(series(Unit::Celsius));
                let mut refused = 0u64;
                for a in &attempts {
                    let record: Record = match a {
                        Attempt::Reading(src) => Reading::new(t0(), src.clone(), DeviceId::new("d").unwrap(), Metric::TEMPERATURE, 1.0, Unit::Celsius).into(),
                        Attempt::Event(actor, kind) => Event::new(t0(), kind.clone(), actor.clone(), serde_json::Value::Null).into(),
                        Attempt::State(zone) => ClimateState::new(ZoneId::new(zone.clone()).unwrap(), t0(), ClimateMode::Schedule).into(),
                    };
                    if em.emit(record).await.is_err() {
                        refused += 1;
                    }
                }
                drop(em);
                while let Some(env) = rx.recv().await {
                    prop_assert_eq!(&env.source, "inst");
                    match &env.record {
                        Record::Reading(r) => prop_assert_eq!(&r.source, "inst"),
                        Record::Event(e) => {
                            prop_assert_eq!(&e.actor, "inst");
                            prop_assert!(!Event::is_reserved_kind(&e.kind));
                        }
                        Record::Climate(_) => {
                            prop_assert_eq!(env.state_key().unwrap().source, "inst");
                        }
                        _ => {}
                    }
                }
                let s = stats.snapshot();
                prop_assert_eq!(s.provenance_rejections, refused);
                prop_assert_eq!(s.emit_errors, refused);
                Ok(())
            })?;
        }
    }

    fn forecast(issued: i64, from: i64, to: i64, value: f64, unit: Unit) -> Forecast {
        let at = |m: i64| t0() + SignedDuration::from_mins(m);
        Forecast::new(
            at(issued),
            DeviceId::new("d").unwrap(),
            Metric::new("unit_rate").unwrap(),
            value,
            unit,
            Basis::Published,
        )
        .valid_between(at(from), at(to))
        .with_changed_at(1)
    }

    fn value() -> impl Strategy<Value = f64> {
        prop_oneof![
            8 => -100.0f64..100.0,
            1 => Just(f64::NAN),
            1 => Just(f64::INFINITY),
            1 => Just(f64::NEG_INFINITY),
        ]
    }

    fn unit() -> impl Strategy<Value = Unit> {
        proptest::sample::select(Unit::ALL)
    }

    proptest! {
        /// The emitter accepts a forecast exactly when its period is
        /// ordered and it ends within the horizon of its issue time, and
        /// money only from a kind with a source currency, whatever its
        /// value (NaN and ±∞ included); everything accepted arrives
        /// unchanged, bit for bit, and every refusal is counted with its
        /// reason.
        #[test]
        fn forecast_rules_are_enforced(
            cases in proptest::collection::vec(
                (-3000i64..3000, -3000i64..3000, -3000i64..3000, value(), unit()),
                1..40,
            ),
            horizon_mins in 0i64..2000,
            declared in any::<bool>(),
        ) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let horizon = SignedDuration::from_mins(horizon_mins);
                let (tx, mut rx) = mpsc::channel(1024);
                let stats = StatsHandle::new("inst", "test", Mode::Shadow);
                let currency = declared.then(|| Currency::of("EUR"));
                let em = |unit| Emitter::for_adapter("inst", tx.clone())
                    .with_stats(stats.clone())
                    .with_forecast_horizon(horizon)
                    .with_source_currency(currency)
                    .with_series(series(unit));
                let mut accepted = Vec::new();
                for &(issued, from, to, v, unit) in &cases {
                    let f = forecast(issued, from, to, v, unit);
                    let em = em(unit);
                    let ok = from <= to
                        && to - issued <= horizon_mins
                        && (declared || !unit.is_money());
                    let got = em.emit(f.clone()).await;
                    prop_assert_eq!(got.is_ok(), ok, "{:?}: {:?}", f, got);
                    if let Err(e) = got {
                        prop_assert!(!e.is_provenance());
                        prop_assert_eq!(stats.snapshot().last_emit_error, Some(e.to_string()));
                    } else {
                        accepted.push(f);
                    }
                }
                drop(tx);
                let mut arrived = Vec::new();
                while let Some(env) = rx.recv().await {
                    prop_assert_eq!(&env.source, "inst");
                    match env.record {
                        Record::Forecast(f) => arrived.push(f),
                        other => prop_assert!(false, "unexpected {:?}", other),
                    }
                }
                prop_assert_eq!(arrived.len(), accepted.len());
                for (a, b) in arrived.iter().zip(&accepted) {
                    // Bit for bit: NaN is never equal to itself.
                    prop_assert_eq!(a.value.to_bits(), b.value.to_bits());
                    let (mut a, mut b) = (a.clone(), b.clone());
                    a.value = 0.0;
                    b.value = 0.0;
                    prop_assert_eq!(a, b);
                }
                let s = stats.snapshot();
                prop_assert_eq!(s.forecasts, accepted.len() as u64);
                prop_assert_eq!(s.emit_errors, (cases.len() - accepted.len()) as u64);
                Ok(())
            })?;
        }

        /// A forecast issued up to the allowed skew after the clock passes;
        /// one nanosecond more does not.
        #[test]
        fn issued_at_is_bounded_by_the_clock(
            ahead_ns in prop_oneof![
                Just(MAX_FUTURE_SKEW.as_nanos() as i64),
                Just(MAX_FUTURE_SKEW.as_nanos() as i64 + 1),
                Just(0i64),
                -10_000_000_000i64..10_000_000_000,
            ],
        ) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let (tx, _rx) = mpsc::channel(4);
                let clock = crate::ManualClock::new(t0());
                let em = Emitter::for_adapter("inst", tx).with_clock(Arc::new(clock)).with_series(series(Unit::Celsius));
                let issued = t0() + SignedDuration::from_nanos(ahead_ns);
                let f = forecast(0, 0, 30, 1.0, Unit::Celsius)
                    .with_issued_at(issued)
                    .valid_between(issued, issued + SignedDuration::from_mins(30));
                let got = em.emit(f).await;
                let ok = SignedDuration::from_nanos(ahead_ns) <= MAX_FUTURE_SKEW;
                prop_assert_eq!(got.is_ok(), ok, "{:?}", got);
                if let Err(e) = got {
                    prop_assert!(e.to_string().contains("is after the clock's"), "{}", e);
                }
                Ok(())
            })?;
        }

        /// Readings in a money unit pass only with a declared currency.
        #[test]
        fn money_readings_need_a_currency(unit in unit(), declared in any::<bool>()) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let (tx, _rx) = mpsc::channel(4);
                let em = Emitter::for_adapter("inst", tx)
                    .with_source_currency(declared.then(|| Currency::of("USD")))
                    .with_series([SeriesTemplate::gauge(Metric::new("price").unwrap(), unit, SignedDuration::from_hours(1))]);
                let r = Reading::new(t0(), "inst", DeviceId::new("d").unwrap(), Metric::new("price").unwrap(), 0.2, unit);
                let got = em.emit(r).await;
                prop_assert_eq!(got.is_ok(), declared || !unit.is_money());
                if let Err(e) = got {
                    prop_assert_eq!(e, EmitError::UndeclaredCurrency { unit });
                }
                Ok(())
            })?;
        }
    }

    /// NaN and ±∞ pass through the emitter unchanged, bit for bit, in
    /// readings and forecasts: the store will record them as bad quality.
    #[tokio::test]
    async fn non_finite_values_pass_through_unchanged() {
        let (tx, mut rx) = mpsc::channel(16);
        let stats = StatsHandle::new("inst", "test", Mode::Shadow);
        let em = Emitter::for_adapter("inst", tx)
            .with_stats(stats.clone())
            .with_series(series(Unit::Celsius));
        // A NaN with a payload, to show the bits are not normalised.
        let odd_nan = f64::from_bits(0x7ff8_0000_0000_beef);
        let values = [f64::NAN, odd_nan, f64::INFINITY, f64::NEG_INFINITY];
        for v in values {
            let r = Reading::new(
                t0(),
                "inst",
                DeviceId::new("d").unwrap(),
                Metric::TEMPERATURE,
                v,
                Unit::Celsius,
            );
            em.emit(r).await.unwrap();
            em.emit(forecast(0, 0, 30, v, Unit::Celsius)).await.unwrap();
        }
        drop(em);
        let mut got = Vec::new();
        while let Some(env) = rx.recv().await {
            got.push(match env.record {
                Record::Reading(r) => r.value.to_bits(),
                Record::Forecast(f) => f.value.to_bits(),
                other => panic!("unexpected {other:?}"),
            });
        }
        let want: Vec<u64> = values
            .iter()
            .flat_map(|v| [v.to_bits(), v.to_bits()])
            .collect();
        assert_eq!(got, want);
        let s = stats.snapshot();
        assert_eq!((s.readings, s.forecasts, s.emit_errors), (4, 4, 0));
    }

    #[tokio::test]
    async fn forecast_refusals_name_the_rule() {
        let (tx, _rx) = mpsc::channel(4);
        let em = Emitter::for_adapter("inst", tx)
            .with_forecast_horizon(SignedDuration::from_hours(1))
            .with_series(series(Unit::Celsius));
        let e = em
            .emit(forecast(0, 30, 0, 1.0, Unit::Celsius))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("is after valid_to"), "{e}");
        let e = em
            .emit(forecast(0, 0, 61, 1.0, Unit::Celsius))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("more than the 1h horizon"), "{e}");
        let e = em
            .emit(forecast(0, 0, 30, 0.2, Unit::CurrencyPerKilowattHour))
            .await
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "¤/kWh is a money unit, but the adapter kind declares no source currency"
        );
        assert!(AdapterErrorProbe::fatal(e));
        assert_eq!(em.forecast_horizon(), SignedDuration::from_hours(1));
    }

    /// Refused records are fatal to the instance (only `Closed` is not).
    struct AdapterErrorProbe;
    impl AdapterErrorProbe {
        fn fatal(e: EmitError) -> bool {
            crate::AdapterError::from(e).is_fatal()
        }
    }

    fn metric_name() -> impl Strategy<Value = Metric> {
        proptest::sample::select(vec!["temperature", "unit_rate", "power"])
            .prop_map(|m| Metric::new(m).unwrap())
    }

    proptest! {
        /// An adapter's reading or forecast is accepted exactly when its
        /// kind declares a template for the metric, as a reading or as a
        /// forecast respectively, with the same unit; refusals name the
        /// rule. Core emitters are not restricted.
        #[test]
        fn series_templates_are_enforced(
            declared in proptest::collection::vec((metric_name(), any::<bool>(), unit()), 0..4),
            records in proptest::collection::vec((metric_name(), any::<bool>(), unit()), 1..20),
        ) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                // First template per (metric, forecast) wins, as in a valid descriptor.
                let mut templates: Vec<SeriesTemplate> = Vec::new();
                for (m, fc, unit) in &declared {
                    if templates.iter().any(|t| t.metric == *m && t.for_forecasts() == *fc) {
                        continue;
                    }
                    let hour = SignedDuration::from_hours(1);
                    templates.push(if *fc {
                        SeriesTemplate::forecast(m.clone(), *unit, hour)
                    } else {
                        SeriesTemplate::gauge(m.clone(), *unit, hour)
                    });
                }
                let (tx, _rx) = mpsc::channel(64);
                let em = Emitter::for_adapter("inst", tx.clone())
                    .with_source_currency(Some(Currency::of("EUR")))
                    .with_series(templates.clone());
                let core = Emitter::core("inst", tx);
                let dev = DeviceId::new("d").unwrap();
                for (m, fc, unit) in &records {
                    let record: Record = if *fc {
                        Forecast::new(t0(), dev.clone(), m.clone(), 1.0, *unit, Basis::Modelled).with_changed_at(1).into()
                    } else {
                        Reading::new(t0(), "inst", dev.clone(), m.clone(), 1.0, *unit).into()
                    };
                    let template = templates.iter().find(|t| t.metric == *m && t.for_forecasts() == *fc);
                    let got = em.emit(record.clone()).await;
                    match template {
                        None => prop_assert_eq!(got, Err(EmitError::NoTemplate { metric: m.clone(), forecast: *fc })),
                        Some(t) if t.unit != *unit => prop_assert_eq!(got, Err(EmitError::UnitMismatch {
                            device: dev.clone(), metric: m.clone(), expected: t.unit, found: *unit,
                        })),
                        Some(_) => prop_assert_eq!(got, Ok(())),
                    }
                    prop_assert_eq!(core.emit(record).await, Ok(()));
                }
                Ok(())
            })?;
        }
    }

    proptest! {
        /// A reading or forecast is accepted exactly when it carries
        /// `changed_at` if and only if its series is versioned; the marker
        /// arrives unchanged.
        #[test]
        fn changed_at_follows_versioning(
            versioned in any::<bool>(),
            forecast_record in any::<bool>(),
            changed_at in proptest::option::of(any::<u64>()),
        ) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let hour = SignedDuration::from_hours(1);
                let m = Metric::new("rate").unwrap();
                // Forecast templates are always versioned.
                let template = if forecast_record {
                    SeriesTemplate::forecast(m.clone(), Unit::Watt, hour)
                } else if versioned {
                    SeriesTemplate::gauge(m.clone(), Unit::Watt, hour).versioned()
                } else {
                    SeriesTemplate::gauge(m.clone(), Unit::Watt, hour)
                };
                let versioned = template.versioned;
                let (tx, mut rx) = mpsc::channel(4);
                let em = Emitter::for_adapter("inst", tx).with_series([template]);
                let dev = DeviceId::new("d").unwrap();
                let record: Record = if forecast_record {
                    let mut f = Forecast::new(t0(), dev.clone(), m.clone(), 1.0, Unit::Watt, Basis::Modelled);
                    f.changed_at = changed_at;
                    f.into()
                } else {
                    let mut r = Reading::new(t0(), "inst", dev.clone(), m.clone(), 1.0, Unit::Watt);
                    r.changed_at = changed_at;
                    r.into()
                };
                let got = em.emit(record).await;
                match (versioned, changed_at) {
                    (true, None) => prop_assert_eq!(got, Err(EmitError::MissingChangedAt { device: dev, metric: m })),
                    (false, Some(_)) => prop_assert_eq!(got, Err(EmitError::UnexpectedChangedAt { device: dev, metric: m })),
                    _ => {
                        prop_assert_eq!(got, Ok(()));
                        let arrived = match rx.recv().await.unwrap().record {
                            Record::Reading(r) => r.changed_at,
                            Record::Forecast(f) => f.changed_at,
                            _ => None,
                        };
                        prop_assert_eq!(arrived, changed_at);
                    }
                }
                Ok(())
            })?;
        }
    }

    /// Published forecasts need a scheduled template, modelled ones a
    /// forecast template; readings a measured one.
    #[tokio::test]
    async fn roles_follow_the_basis() {
        let hour = SignedDuration::from_hours(1);
        let m = |s: &str| Metric::new(s).unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let em = Emitter::for_adapter("inst", tx).with_series([
            SeriesTemplate::scheduled(m("price"), Unit::Watt, hour),
            SeriesTemplate::forecast(m("wind"), Unit::Watt, hour),
            SeriesTemplate::gauge(m("wind"), Unit::Watt, hour),
        ]);
        let dev = DeviceId::new("d").unwrap();
        let f = |metric: &str, basis| {
            let f = Forecast::new(t0(), dev.clone(), m(metric), 1.0, Unit::Watt, basis);
            if metric == "wind" {
                f.with_changed_at(1)
            } else {
                f
            }
        };
        em.emit(f("price", Basis::Published)).await.unwrap();
        em.emit(f("wind", Basis::Modelled)).await.unwrap();
        em.emit(Reading::new(
            t0(),
            "inst",
            dev.clone(),
            m("wind"),
            2.0,
            Unit::Watt,
        ))
        .await
        .unwrap();
        let e = em.emit(f("price", Basis::Modelled)).await.unwrap_err();
        assert_eq!(
            e.to_string(),
            "forecast price: a modelled forecast needs a forecast series template, not a scheduled one"
        );
        assert!(AdapterErrorProbe::fatal(e));
        let e = em.emit(f("wind", Basis::Published)).await.unwrap_err();
        assert_eq!(
            e,
            EmitError::RoleMismatch {
                metric: m("wind"),
                basis: Basis::Published,
                needs: SeriesRole::Scheduled,
                role: SeriesRole::Forecast,
            }
        );
        // A reading of a metric with only a scheduled template has none.
        let e = em
            .emit(Reading::new(t0(), "inst", dev, m("price"), 1.0, Unit::Watt))
            .await
            .unwrap_err();
        assert!(
            matches!(
                e,
                EmitError::NoTemplate {
                    forecast: false,
                    ..
                }
            ),
            "{e}"
        );
        drop(em);
        let mut n = 0;
        while rx.recv().await.is_some() {
            n += 1;
        }
        assert_eq!(n, 3);
    }

    /// An interval series' readings and forecasts must start an interval.
    #[tokio::test]
    async fn misaligned_interval_records_are_refused() {
        let hour = SignedDuration::from_hours(1);
        let rain = Metric::new("rain").unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let mut forecast = SeriesTemplate::forecast(rain.clone(), Unit::Millimetre, hour);
        forecast.kind = crate::SeriesKind::Interval { length: hour };
        let em = Emitter::for_adapter("inst", tx).with_series([
            SeriesTemplate::interval(rain.clone(), Unit::Millimetre, hour, hour),
            forecast,
        ]);
        let dev = DeviceId::new("d").unwrap();
        let at = |m: i64| t0() + SignedDuration::from_mins(m);
        em.emit(Reading::new(
            at(60),
            "inst",
            dev.clone(),
            rain.clone(),
            0.4,
            Unit::Millimetre,
        ))
        .await
        .unwrap();
        let e = em
            .emit(Reading::new(
                at(61),
                "inst",
                dev.clone(),
                rain.clone(),
                0.4,
                Unit::Millimetre,
            ))
            .await
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "rain on d: 2030-01-01T01:01:00Z does not start an interval of 1h"
        );
        assert!(AdapterErrorProbe::fatal(e));
        let f = |from: i64| {
            Forecast::new(
                t0(),
                dev.clone(),
                rain.clone(),
                0.1,
                Unit::Millimetre,
                Basis::Modelled,
            )
            .valid_between(at(from), at(from + 60))
            .with_changed_at(1)
        };
        em.emit(f(120)).await.unwrap();
        assert!(matches!(
            em.emit(f(150)).await,
            Err(EmitError::Misaligned { .. })
        ));
        drop(em);
        let mut n = 0;
        while rx.recv().await.is_some() {
            n += 1;
        }
        assert_eq!(n, 2);
    }

    #[tokio::test]
    async fn changed_at_refusals_read_well() {
        let (tx, _rx) = mpsc::channel(4);
        let hour = SignedDuration::from_hours(1);
        let em = Emitter::for_adapter("inst", tx).with_series([
            SeriesTemplate::gauge(Metric::TEMPERATURE, Unit::Celsius, hour),
            SeriesTemplate::gauge(Metric::ENERGY, Unit::KilowattHour, hour).versioned(),
        ]);
        let dev = DeviceId::new("meter").unwrap();
        let e = em
            .emit(Reading::new(
                t0(),
                "inst",
                dev.clone(),
                Metric::ENERGY,
                1.0,
                Unit::KilowattHour,
            ))
            .await
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "energy on meter: the series is versioned, but the record carries no changed_at"
        );
        assert!(AdapterErrorProbe::fatal(e));
        let e = em
            .emit(
                Reading::new(t0(), "inst", dev, Metric::TEMPERATURE, 1.0, Unit::Celsius)
                    .with_changed_at(5),
            )
            .await
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "temperature on meter: changed_at on a series that is not versioned"
        );
    }

    #[tokio::test]
    async fn template_refusals_read_well() {
        let (tx, _rx) = mpsc::channel(4);
        let em = Emitter::for_adapter("inst", tx).with_series(series(Unit::Celsius));
        let dev = DeviceId::new("d").unwrap();
        let e = em
            .emit(Reading::new(
                t0(),
                "inst",
                dev.clone(),
                Metric::HUMIDITY,
                50.0,
                Unit::Percent,
            ))
            .await
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "reading humidity has no series template: declare one on the adapter kind's descriptor"
        );
        assert!(AdapterErrorProbe::fatal(e));
        let e = em
            .emit(Reading::new(
                t0(),
                "inst",
                dev,
                Metric::TEMPERATURE,
                50.0,
                Unit::Percent,
            ))
            .await
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "temperature on d: unit % differs from its series template's °C"
        );
        // A forecast template does not cover readings of the metric.
        let e = em
            .emit(Reading::new(
                t0(),
                "inst",
                DeviceId::new("d").unwrap(),
                Metric::new("unit_rate").unwrap(),
                1.0,
                Unit::Celsius,
            ))
            .await
            .unwrap_err();
        assert!(
            matches!(
                e,
                EmitError::NoTemplate {
                    forecast: false,
                    ..
                }
            ),
            "{e}"
        );
        assert_eq!(em.series().len(), 2);
    }

    /// The context's clock is the emitter's: a forecast issued later than
    /// the context clock allows is refused, though the emitter was built
    /// without a clock.
    #[tokio::test]
    async fn context_parts_give_the_emitter_their_clock() {
        let (tx, _rx) = mpsc::channel(4);
        let em = Emitter::for_adapter("inst", tx).with_series(series(Unit::Celsius));
        let clock = crate::ManualClock::new(t0());
        let (_trigger, shutdown) = crate::shutdown_channel();
        let parts = ContextParts::new(
            em,
            Arc::new(clock.clone()),
            shutdown,
            std::env::temp_dir(),
            StatsHandle::new("inst", "test", Mode::Shadow).for_instance(),
            Site::new("UTC", "EUR").unwrap(),
        );
        let ctx = AdapterContext::new(parts, Arc::new(()));
        let ahead = forecast(0, 0, 30, 1.0, Unit::Celsius)
            .with_issued_at(t0() + SignedDuration::from_secs(5))
            .valid_between(
                t0() + SignedDuration::from_secs(5),
                t0() + SignedDuration::from_mins(30),
            );
        let e = ctx.emit(ahead.clone()).await.unwrap_err();
        assert!(e.to_string().contains("is after the clock's"), "{e}");
        clock.advance(SignedDuration::from_secs(5));
        ctx.emit(ahead).await.unwrap();
    }

    #[tokio::test]
    async fn core_emitters_may_emit_reserved_kinds() {
        let (tx, mut rx) = mpsc::channel(4);
        let core = Emitter::core("actuator", tx);
        core.emit(Event::new(
            t0(),
            Event::COMMAND,
            "planner",
            serde_json::Value::Null,
        ))
        .await
        .unwrap();
        let env = rx.recv().await.unwrap();
        assert_eq!(env.source, "actuator");
    }
}
