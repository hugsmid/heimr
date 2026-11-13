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
use crate::record::{Envelope, Event, Reading, ReadingError, Record};
use crate::shutdown::Shutdown;
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
    /// A reading failed validation.
    #[error(transparent)]
    Invalid(#[from] ReadingError),
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
/// ([`Event::is_reserved_kind`]). Refusals are counted in the instance's
/// stats. Only core emitters ([`Emitter::core`]) may emit reserved kinds or
/// other actors; adapters never get one.
#[derive(Debug, Clone)]
pub struct Emitter {
    source: Arc<str>,
    tx: mpsc::Sender<Envelope>,
    stats: Option<StatsHandle>,
    core: bool,
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
        }
    }

    /// Count everything emitted (and every failure) in `stats`.
    #[must_use]
    pub fn with_stats(mut self, stats: StatsHandle) -> Self {
        self.stats = Some(stats);
        self
    }

    /// The source stamped on every record.
    pub fn source(&self) -> &str {
        &self.source
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
                stats.emit_failed(e.is_provenance());
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
                r.validate()?;
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
}

impl ContextParts {
    /// Parts with a disconnected command handle.
    pub fn new(
        emitter: Emitter,
        clock: SharedClock,
        shutdown: Shutdown,
        state_dir: PathBuf,
        stats: InstanceStats,
    ) -> Self {
        Self {
            emitter,
            commands: Commands::disconnected(),
            clock,
            shutdown,
            state_dir,
            stats,
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
    use proptest::prelude::*;

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
                let em = Emitter::for_adapter("inst", tx).with_stats(stats.clone());
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
