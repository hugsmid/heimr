//! A direct-drive harness for unit-testing an adapter without the app: the
//! adapter runs with a real context (core emitter, stats and actuator) on a
//! [`ManualClock`], and every record lands in memory.
//!
//! ```
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() {
//! use heimr_testkit::Harness;
//! use jiff::SignedDuration;
//!
//! let mut h = Harness::start::<heimr::demo::Demo>(serde_json::json!({ "interval": "1m" }))
//!     .await
//!     .unwrap();
//! h.advance(SignedDuration::from_mins(1)).await;
//! assert_eq!(h.readings().len(), 4); // two ticks × temperature and humidity
//! assert_eq!(h.stats().readings, 4);
//! h.stop().await.unwrap();
//! # }
//! ```
//!
//! Use it under `#[tokio::test]` (the default single-threaded runtime). The
//! clock only moves when told to; [`Harness::advance`] then lets the adapter
//! run until it is idle, with a few milliseconds of real time for real I/O
//! such as a local [`ReplayServer`](crate::replay::ReplayServer).

use std::sync::Arc;
use std::time::Duration;

use heimr_core::{
    Actuator, Adapter, AdapterError, AdapterFactory, AdapterStats, AuditRecord, Command,
    CommandRequest, CommandSubmitter, ContextParts, Emitter, Envelope, Event, Forecast,
    InstanceSpec, ManualClock, MemoryAuditLog, Mode, Outcome, Reading, Record, Rejection,
    SafetyPolicy, ShutdownTrigger, Site, StatsHandle, TypedFactory, shutdown_channel,
};
use jiff::SignedDuration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::inprocess::{ACTOR, INSTANCE, t0};
use crate::suite::test_site;
use crate::util::TempDir;

/// How long [`Harness::stop`] waits (real time) for `run` to return.
const STOP_WAIT: Duration = Duration::from_secs(5);
/// Real time allowed per settle for I/O to complete.
const SETTLE_IO: Duration = Duration::from_millis(5);

/// One adapter instance running under test. See the [module docs](self).
pub struct Harness {
    factory: Arc<dyn AdapterFactory>,
    settings: serde_json::Value,
    site: Site,
    clock: ManualClock,
    tx: mpsc::Sender<Envelope>,
    rx: mpsc::Receiver<Envelope>,
    seen: Vec<Envelope>,
    /// Where each run after the first starts in `seen`.
    run_starts: Vec<usize>,
    stats: StatsHandle,
    trigger: ShutdownTrigger,
    task: Option<JoinHandle<Result<(), AdapterError>>>,
    actuator: Actuator,
    audit: MemoryAuditLog,
    submitter: CommandSubmitter,
    dir: TempDir,
}

impl std::fmt::Debug for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Harness")
            .field("records", &self.seen.len())
            .finish_non_exhaustive()
    }
}

fn setup(e: impl std::fmt::Display) -> AdapterError {
    AdapterError::fatal(format!("harness set-up: {e}"))
}

impl Harness {
    /// Run `A` with `settings` in shadow mode for [`test_site`].
    pub async fn start<A: Adapter>(settings: serde_json::Value) -> Result<Self, AdapterError> {
        Self::start_with(
            TypedFactory::<A>::new(),
            settings,
            Mode::Shadow,
            test_site(),
        )
        .await
    }

    /// Run `A` with `settings` in live mode (commands are delivered).
    pub async fn start_live<A: Adapter>(settings: serde_json::Value) -> Result<Self, AdapterError> {
        Self::start_with(TypedFactory::<A>::new(), settings, Mode::Live, test_site()).await
    }

    /// Run any factory's instance in `mode` for `site`. Fails as the app
    /// would if the settings are refused, or if the kind quotes a currency
    /// other than the site's.
    pub async fn start_with(
        factory: impl AdapterFactory,
        settings: serde_json::Value,
        mode: Mode,
        site: Site,
    ) -> Result<Self, AdapterError> {
        let factory: Arc<dyn AdapterFactory> = Arc::new(factory);
        let descriptor = factory.descriptor();
        descriptor
            .check_site(&site)
            .map_err(|e| AdapterError::config(e.to_string()))?;
        let instance = factory.prepare(&settings, &site).await?;
        let clock = ManualClock::new(t0());
        let audit = MemoryAuditLog::new();
        let policy = SafetyPolicy::default()
            .with_min_interval(SignedDuration::ZERO)
            .with_max_per_hour(10_000)
            .with_heating_off(true);
        let actuator = Actuator::builder(policy, Arc::new(clock.clone()), Arc::new(audit.clone()))
            .without_boot_holdoff()
            .build()
            .await
            .map_err(setup)?;
        actuator
            .register(InstanceSpec::new(
                INSTANCE,
                &descriptor.kind,
                instance.capabilities(),
                mode,
            ))
            .map_err(|e| AdapterError::config(e.to_string()))?;
        let stats = actuator.stats(INSTANCE).ok_or_else(|| setup("no stats"))?;
        let (tx, rx) = mpsc::channel(65_536);
        let dir = TempDir::new("harness").map_err(setup)?;
        let submitter = actuator.submitter(ACTOR).map_err(setup)?;
        let (trigger, task) = launch(
            instance,
            &actuator,
            &descriptor,
            &tx,
            &stats,
            &clock,
            &dir,
            &site,
        )
        .await?;
        let mut h = Self {
            factory,
            settings,
            site,
            clock,
            tx,
            rx,
            seen: Vec::new(),
            run_starts: Vec::new(),
            stats,
            trigger,
            task: Some(task),
            actuator,
            audit,
            submitter,
            dir,
        };
        h.settle().await;
        Ok(h)
    }

    /// The fake clock (starts at 2030-01-01T00:00:00Z).
    pub fn clock(&self) -> &ManualClock {
        &self.clock
    }

    /// The instance's state directory.
    pub fn state_dir(&self) -> &std::path::Path {
        self.dir.path()
    }

    /// Let the adapter run until it is idle, without moving the clock.
    pub async fn settle(&mut self) {
        for _ in 0..64 {
            tokio::task::yield_now().await;
            self.drain();
        }
        // A little real time for real I/O (a local replay server), then
        // let the adapter act on it.
        tokio::time::sleep(SETTLE_IO).await;
        for _ in 0..64 {
            tokio::task::yield_now().await;
            self.drain();
        }
    }

    /// Move the clock by `by` in one jump, then settle.
    pub async fn advance(&mut self, by: SignedDuration) {
        self.clock.advance(by);
        self.settle().await;
    }

    /// Move the clock by `total` in steps of `step`, settling after each.
    pub async fn advance_by_steps(&mut self, total: SignedDuration, step: SignedDuration) {
        let step = step.max(SignedDuration::from_millis(1));
        let mut left = total;
        while left > SignedDuration::ZERO {
            let d = left.min(step);
            self.advance(d).await;
            left -= d;
        }
    }

    fn drain(&mut self) {
        while let Ok(env) = self.rx.try_recv() {
            self.seen.push(env);
        }
    }

    /// Everything emitted so far, in order.
    pub fn records(&mut self) -> &[Envelope] {
        self.drain();
        &self.seen
    }

    /// Everything emitted so far, one list per run ([`Harness::restart`]
    /// starts a new one), for [`checks::monotonic`](crate::checks::monotonic).
    pub fn runs(&mut self) -> Vec<Vec<Envelope>> {
        self.drain();
        let mut bounds = vec![0];
        bounds.extend(&self.run_starts);
        bounds.push(self.seen.len());
        bounds
            .windows(2)
            .map(|w| self.seen[w[0]..w[1]].to_vec())
            .collect()
    }

    /// Forecasts emitted so far.
    pub fn forecasts(&mut self) -> Vec<Forecast> {
        self.records()
            .iter()
            .filter_map(|e| match &e.record {
                Record::Forecast(f) => Some(f.clone()),
                _ => None,
            })
            .collect()
    }

    /// Readings emitted so far.
    pub fn readings(&mut self) -> Vec<Reading> {
        self.records()
            .iter()
            .filter_map(|e| match &e.record {
                Record::Reading(r) => Some(r.clone()),
                _ => None,
            })
            .collect()
    }

    /// Events emitted so far (health reports included).
    pub fn events(&mut self) -> Vec<Event> {
        self.records()
            .iter()
            .filter_map(|e| match &e.record {
                Record::Event(ev) => Some(ev.clone()),
                _ => None,
            })
            .collect()
    }

    /// The core's stats for the instance.
    pub fn stats(&self) -> AdapterStats {
        self.stats.snapshot()
    }

    /// The audit trail.
    pub fn audit(&self) -> Vec<AuditRecord> {
        self.audit.records()
    }

    /// Whether `run` has returned.
    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Submit a command through the actuator (no rate-limit interval, heating
    /// off allowed), then settle.
    pub async fn submit(&mut self, command: impl Into<Command>) -> Result<Outcome, Rejection> {
        let out = self
            .submitter
            .submit(CommandRequest::new(INSTANCE, command, "harness"))
            .await;
        self.settle().await;
        out
    }

    /// Signal shutdown and wait (up to 5 s real time) for `run` to return.
    /// Returns its result; a run that overstays or panics is an error.
    pub async fn stop(mut self) -> Result<Vec<Envelope>, AdapterError> {
        self.end_run().await?;
        Ok(std::mem::take(&mut self.seen))
    }

    /// Restart the instance as a restart of the whole app would: stop the
    /// run as [`Harness::stop`] does (returning its error, if any, without
    /// starting again), then build the instance afresh from the same
    /// settings and run it with the same state directory. The clock and
    /// the records seen so far carry on, so a test can check what is
    /// re-sent across the restart ([`Harness::runs`]).
    pub async fn restart(&mut self) -> Result<(), AdapterError> {
        self.end_run().await?;
        self.run_starts.push(self.seen.len());
        let descriptor = self.factory.descriptor();
        let instance = self.factory.prepare(&self.settings, &self.site).await?;
        let (trigger, task) = launch(
            instance,
            &self.actuator,
            &descriptor,
            &self.tx,
            &self.stats,
            &self.clock,
            &self.dir,
            &self.site,
        )
        .await?;
        self.trigger = trigger;
        self.task = Some(task);
        self.settle().await;
        Ok(())
    }

    async fn end_run(&mut self) -> Result<(), AdapterError> {
        self.trigger.trigger();
        let result = match self.task.take() {
            None => Ok(()),
            Some(task) => match tokio::time::timeout(STOP_WAIT, task).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) if e.is_panic() => Err(AdapterError::contract("run panicked")),
                Ok(Err(e)) => Err(AdapterError::contract(e.to_string())),
                Err(_) => Err(AdapterError::contract(
                    "run did not return within 5 s of shutdown",
                )),
            },
        };
        self.actuator.detach(INSTANCE).await;
        self.drain();
        result
    }
}

/// Attach a command queue and start one run of `instance`, with an
/// emitter wired as the app wires it (source currency, default forecast
/// horizon, the clock).
#[allow(clippy::too_many_arguments)]
async fn launch(
    instance: Box<dyn heimr_core::AdapterInstance>,
    actuator: &Actuator,
    descriptor: &heimr_core::AdapterDescriptor,
    tx: &mpsc::Sender<Envelope>,
    stats: &StatsHandle,
    clock: &ManualClock,
    dir: &TempDir,
    site: &Site,
) -> Result<(ShutdownTrigger, JoinHandle<Result<(), AdapterError>>), AdapterError> {
    let series = instance
        .series()
        .unwrap_or_else(|| descriptor.series.clone());
    // As the app does: only the intervals may differ from the kind's.
    descriptor
        .check_instance_series(&series)
        .map_err(|e| AdapterError::config(e.to_string()))?;
    let commands = actuator.attach(INSTANCE).await.map_err(setup)?;
    let emitter = Emitter::for_adapter(INSTANCE, tx.clone())
        .with_stats(stats.clone())
        .with_source_currency(descriptor.source_currency)
        .with_series(series)
        .with_clock(Arc::new(clock.clone()));
    let (trigger, shutdown) = shutdown_channel();
    let parts = ContextParts::new(
        emitter,
        Arc::new(clock.clone()),
        shutdown,
        dir.path().to_owned(),
        stats.for_instance(),
        site.clone(),
    )
    .with_commands(commands);
    Ok((trigger, tokio::spawn(instance.run(parts))))
}
