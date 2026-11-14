//! The in-process [`Driver`]: a Rust adapter factory run inside the real
//! app (supervisor, actuator, emitter and stats), on a fake clock, with an
//! in-memory sink and audit log.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use heimr::{APP_SOURCE, App, Config, MemorySink, Telemetry};
use heimr_core::{
    AdapterDescriptor, AdapterError, AdapterFactory, AdapterInstance, AdapterStats, AuditRecord,
    BoxFuture, Capabilities, Command, CommandRequest, CommandSubmitter, Currency, Envelope, Event,
    ManualClock, MemoryAuditLog, Mode, Outcome, Record, Rejection, SharedClock, ShutdownTrigger,
    Site, TokioClock,
};
use jiff::{SignedDuration, Timestamp};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

use crate::driver::{Driver, Failure, Fault, PrepareFailure, Traffic};
use crate::upstream::Upstream;
use crate::util::{TempDir, panic_message};

/// The instance name adapters run as under the in-process driver.
pub const INSTANCE: &str = "under-test";
/// The actor that submits commands.
pub const ACTOR: &str = "conformance";

/// Where every fake clock starts.
pub fn t0() -> Timestamp {
    Timestamp::from_second(1_893_456_000).unwrap_or(Timestamp::UNIX_EPOCH) // 2030-01-01T00:00:00Z
}

/// How the fake clock moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClockMode {
    /// Tokio's paused clock: deterministic and instant. Time jumps ahead
    /// whenever every task is idle, which also fires timers inside client
    /// libraries doing real I/O (for example HTTP timeouts). Use it for
    /// adapters whose upstream is in-process or a file tree.
    Paused,
    /// A [`ManualClock`] advanced in steps, with a little real time between
    /// steps for real I/O to complete. Use it for adapters talking to a
    /// local server such as [`ReplayServer`](crate::replay::ReplayServer).
    Manual {
        /// Fake time per step.
        step: SignedDuration,
        /// Real time allowed per step.
        settle: Duration,
    },
}

impl ClockMode {
    /// Manual stepping with a 1 s step and 3 ms of real time per step.
    pub fn manual() -> Self {
        Self::Manual {
            step: SignedDuration::from_secs(1),
            settle: Duration::from_millis(3),
        }
    }
}

/// Makes a fresh upstream for each run.
pub type UpstreamFactory =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Box<dyn Upstream>> + Send>> + Send + Sync>;

/// Settings for the in-process driver.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DriverOptions {
    /// How the fake clock moves.
    pub clock: ClockMode,
    /// The shutdown grace given to the instance.
    pub shutdown_deadline: SignedDuration,
    /// The site the app runs for.
    pub site: Site,
    /// The app's `forecast_horizon`.
    pub forecast_horizon: SignedDuration,
}

impl DriverOptions {
    /// Paused clock, 1 s deadline, `site`, the default forecast horizon.
    pub fn new(site: Site) -> Self {
        Self {
            clock: ClockMode::Paused,
            shutdown_deadline: SignedDuration::from_secs(1),
            site,
            forecast_horizon: heimr_core::DEFAULT_FORECAST_HORIZON,
        }
    }
}

#[derive(Debug)]
struct SharedFactory(Arc<dyn AdapterFactory>);

impl AdapterFactory for SharedFactory {
    fn descriptor(&self) -> AdapterDescriptor {
        self.0.descriptor()
    }
    fn prepare(
        &self,
        settings: &serde_json::Value,
        site: &Site,
    ) -> BoxFuture<Result<Box<dyn AdapterInstance>, AdapterError>> {
        self.0.prepare(settings, site)
    }
}

#[derive(Clone)]
enum Clocks {
    Paused(TokioClock),
    Manual(ManualClock),
}

struct Run {
    sink: MemorySink,
    audit: MemoryAuditLog,
    telemetry: Telemetry,
    submitter: CommandSubmitter,
    trigger: ShutdownTrigger,
    handle: Option<JoinHandle<Result<(), heimr::Error>>>,
    clock: Clocks,
    _dir: TempDir,
}

/// Runs one adapter kind inside the app. See the [module docs](self).
pub struct InProcess {
    factory: Arc<dyn AdapterFactory>,
    upstreams: UpstreamFactory,
    options: DriverOptions,
    rt: Runtime,
    upstream: Option<Box<dyn Upstream>>,
    run: Option<Run>,
}

impl std::fmt::Debug for InProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcess")
            .field("factory", &self.factory)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

fn to_std(d: SignedDuration) -> Duration {
    Duration::try_from(d).unwrap_or_default()
}

impl InProcess {
    /// A driver for `factory`, making a fresh upstream per run.
    pub fn new(
        factory: Arc<dyn AdapterFactory>,
        upstreams: UpstreamFactory,
        options: DriverOptions,
    ) -> std::io::Result<Self> {
        let mut b = tokio::runtime::Builder::new_current_thread();
        b.enable_all();
        if options.clock == ClockMode::Paused {
            b.start_paused(true);
        }
        Ok(Self {
            factory,
            upstreams,
            options,
            rt: b.build()?,
            upstream: None,
            run: None,
        })
    }

    fn upstream(&mut self) -> &dyn Upstream {
        if self.upstream.is_none() {
            let make = self.upstreams.clone();
            self.upstream = Some(self.rt.block_on(make()));
        }
        // Just set.
        self.upstream.as_deref().expect("upstream present")
    }

    fn config(
        &self,
        settings: &serde_json::Value,
        mode: Mode,
        dir: &TempDir,
        currency: Currency,
    ) -> Result<Config, String> {
        let toml::Value::Table(mut instance) = toml::Value::try_from(settings)
            .map_err(|e| format!("settings cannot be written as TOML: {e}"))?
        else {
            return Err("settings must be a JSON object".into());
        };
        let kind = self.factory.descriptor().kind;
        instance.insert("type".into(), kind.into());
        instance.insert("mode".into(), mode.as_str().into());
        let mut adapters = toml::Table::new();
        adapters.insert(INSTANCE.into(), instance.into());
        let table = |pairs: Vec<(&str, toml::Value)>| {
            toml::Value::Table(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
        };
        let site = &self.options.site;
        let root = table(vec![
            (
                "site",
                table(vec![
                    ("time_zone", site.time_zone_name().into()),
                    ("currency", currency.as_str().into()),
                ]),
            ),
            (
                "heimr",
                table(vec![
                    ("state_dir", dir.path().display().to_string().into()),
                    (
                        "shutdown_grace",
                        self.options.shutdown_deadline.to_string().into(),
                    ),
                    (
                        "forecast_horizon",
                        self.options.forecast_horizon.to_string().into(),
                    ),
                ]),
            ),
            ("sinks", table(vec![("logging", false.into())])),
            (
                "safety",
                table(vec![
                    ("min_command_interval", "1s".into()),
                    ("max_commands_per_hour", 1000.into()),
                    ("allow_heating_off", true.into()),
                ]),
            ),
            (
                "supervision",
                table(vec![
                    ("backoff_initial", "1s".into()),
                    ("backoff_max", "30s".into()),
                    ("reset_after", "10m".into()),
                ]),
            ),
            ("adapters", toml::Value::Table(adapters)),
        ]);
        let text = toml::to_string(&root).map_err(|e| e.to_string())?;
        Config::from_toml_str(&text).map_err(|e| e.to_string())
    }

    fn settle_now(&self) {
        let real = match self.options.clock {
            ClockMode::Paused => None,
            ClockMode::Manual { settle, .. } => Some(settle),
        };
        self.rt.block_on(async move {
            for _ in 0..64 {
                tokio::task::yield_now().await;
            }
            if let Some(d) = real {
                tokio::time::sleep(d).await;
                for _ in 0..16 {
                    tokio::task::yield_now().await;
                }
            }
        });
    }

    /// Build and run a fresh app on `dir`, with `keep`'s clock if given
    /// (else a new one at [`t0`]), on the current upstream.
    fn launch(&mut self, mode: Mode, dir: TempDir, keep: Option<Clocks>) -> Result<(), String> {
        let settings = self.upstream().settings();
        let config = self.config(&settings, mode, &dir, self.options.site.currency())?;
        let sink = MemorySink::new(1_000_000);
        let audit = MemoryAuditLog::new();
        let factory = SharedFactory(self.factory.clone());
        let clock_mode = self.options.clock;
        let built = self.rt.block_on({
            let sink = sink.clone();
            let audit = audit.clone();
            async move {
                let (clocks, shared): (Clocks, SharedClock) = match (keep, clock_mode) {
                    (Some(Clocks::Paused(c)), _) => (Clocks::Paused(c.clone()), Arc::new(c)),
                    (Some(Clocks::Manual(c)), _) => (Clocks::Manual(c.clone()), Arc::new(c)),
                    (None, ClockMode::Paused) => {
                        let c = TokioClock::starting_at(t0());
                        (Clocks::Paused(c.clone()), Arc::new(c))
                    }
                    (None, ClockMode::Manual { .. }) => {
                        let c = ManualClock::new(t0());
                        (Clocks::Manual(c.clone()), Arc::new(c))
                    }
                };
                let app = App::builder(config)
                    .factory(factory)
                    .sink(sink)
                    .audit_log(Arc::new(audit))
                    .clock(shared)
                    .build()
                    .await
                    .map_err(|e| e.to_string())?;
                Ok::<_, String>((app, clocks))
            }
        });
        let (app, clock) = built?;
        let telemetry = app.telemetry();
        let submitter = app.submitter(ACTOR).map_err(|e| e.to_string())?;
        let trigger = app.shutdown_trigger();
        let handle = self.rt.spawn(app.run_until(std::future::pending()));
        self.run = Some(Run {
            sink,
            audit,
            telemetry,
            submitter,
            trigger,
            handle: Some(handle),
            clock,
            _dir: dir,
        });
        Ok(())
    }
}

impl Driver for InProcess {
    fn descriptor(&self) -> AdapterDescriptor {
        self.factory.descriptor()
    }

    fn instance(&self) -> &str {
        INSTANCE
    }

    fn settings(&mut self) -> serde_json::Value {
        self.upstream().settings()
    }

    fn prepare(&mut self, settings: &serde_json::Value) -> Result<Capabilities, PrepareFailure> {
        let factory = self.factory.clone();
        let site = self.options.site.clone();
        let fut = std::panic::catch_unwind(AssertUnwindSafe(|| factory.prepare(settings, &site)))
            .map_err(|p| PrepareFailure::Panicked {
            message: panic_message(&*p),
        })?;
        let handle = self
            .rt
            .spawn(async move { fut.await.map(|instance| instance.capabilities()) });
        let joined = self.rt.block_on(handle);
        match joined {
            Ok(Ok(caps)) => Ok(caps),
            Ok(Err(e)) => Err(PrepareFailure::Refused {
                config: matches!(e, AdapterError::Config { .. }),
                key: match &e {
                    AdapterError::Config { key, .. } => key.clone(),
                    _ => None,
                },
                fatal: e.is_fatal(),
                message: e.to_string(),
            }),
            Err(e) => Err(PrepareFailure::Panicked {
                message: if e.is_panic() {
                    panic_message(&*e.into_panic())
                } else {
                    "cancelled".into()
                },
            }),
        }
    }

    fn series(&mut self) -> Vec<heimr_core::SeriesTemplate> {
        let settings = self.settings();
        let site = self.options.site.clone();
        let fallback = self.factory.descriptor().series;
        let fut = self.factory.prepare(&settings, &site);
        let handle = self.rt.spawn(async move { fut.await.map(|i| i.series()) });
        match self.rt.block_on(handle) {
            Ok(Ok(Some(series))) => series,
            _ => fallback,
        }
    }

    fn check_site(&mut self, currency: Currency) -> Result<(), String> {
        let settings = self.upstream().settings();
        let dir = TempDir::new("site-check").map_err(|e| e.to_string())?;
        let config = self.config(&settings, Mode::Shadow, &dir, currency)?;
        let factory = SharedFactory(self.factory.clone());
        // In a task, so a panicking adapter is reported, not propagated.
        let handle = self.rt.spawn(async move {
            App::builder(config)
                .factory(factory)
                .check()
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        match self.rt.block_on(handle) {
            Ok(r) => r,
            Err(e) if e.is_panic() => Err(format!("panicked: {}", panic_message(&*e.into_panic()))),
            Err(e) => Err(e.to_string()),
        }
    }

    fn start(&mut self, mode: Mode) -> Result<(), String> {
        if self.run.as_ref().is_some_and(|r| r.handle.is_some()) {
            self.stop();
        }
        self.run = None;
        let make = self.upstreams.clone();
        self.upstream = Some(self.rt.block_on(make()));
        let dir = TempDir::new("conformance").map_err(|e| e.to_string())?;
        self.launch(mode, dir, None)
    }

    fn restart(&mut self, mode: Mode) -> Result<(), String> {
        let Some(mut run) = self.run.take() else {
            return self.start(mode);
        };
        run.trigger.trigger();
        if let Some(handle) = run.handle.take() {
            let _ = self.rt.block_on(handle);
        }
        // Same upstream, state directory and clock: a restart of the app.
        self.launch(mode, run._dir, Some(run.clock))
    }

    fn advance(&mut self, by: SignedDuration) {
        if by <= SignedDuration::ZERO {
            return self.settle();
        }
        match (self.options.clock, self.run.as_ref().map(|r| &r.clock)) {
            (ClockMode::Manual { step, .. }, Some(Clocks::Manual(clock))) => {
                let clock = clock.clone();
                let step = step.max(SignedDuration::from_millis(1));
                let mut left = by;
                while left > SignedDuration::ZERO {
                    let d = left.min(step);
                    clock.advance(d);
                    left -= d;
                    self.settle_now();
                }
            }
            _ => {
                let d = to_std(by);
                self.rt.block_on(async move { tokio::time::sleep(d).await });
            }
        }
    }

    fn settle(&mut self) {
        self.settle_now();
    }

    fn now(&self) -> Timestamp {
        use heimr_core::Clock;
        match self.run.as_ref().map(|r| &r.clock) {
            Some(Clocks::Paused(c)) => {
                let _guard = self.rt.enter();
                c.now()
            }
            Some(Clocks::Manual(c)) => c.now(),
            None => t0(),
        }
    }

    fn submit(&mut self, command: Command) -> Result<Outcome, Rejection> {
        let Some(run) = &self.run else {
            return Err(Rejection::UnknownAdapter {
                adapter: INSTANCE.into(),
            });
        };
        let submitter = run.submitter.clone();
        self.rt.block_on(async move {
            submitter
                .submit(CommandRequest::new(INSTANCE, command, "conformance test"))
                .await
        })
    }

    fn records(&self) -> Vec<Envelope> {
        self.run.as_ref().map_or_else(Vec::new, |r| {
            r.sink
                .snapshot()
                .into_iter()
                .filter(|e| e.source == INSTANCE)
                .collect()
        })
    }

    fn runs(&self) -> Vec<Vec<Envelope>> {
        let Some(run) = &self.run else {
            return Vec::new();
        };
        let mut runs: Vec<Vec<Envelope>> = Vec::new();
        for e in run.sink.snapshot() {
            if e.source == APP_SOURCE {
                if let Record::Event(ev) = &e.record
                    && ev.kind == Event::ADAPTER_STARTED
                    && ev.payload["adapter"] == INSTANCE
                {
                    runs.push(Vec::new());
                }
            } else if e.source == INSTANCE {
                match runs.last_mut() {
                    Some(r) => r.push(e),
                    None => runs.push(vec![e]),
                }
            }
        }
        runs
    }

    fn failures(&self) -> Vec<Failure> {
        let Some(run) = &self.run else {
            return Vec::new();
        };
        run.sink
            .snapshot()
            .into_iter()
            .filter(|e| e.source == APP_SOURCE)
            .filter_map(|e| match e.record {
                Record::Event(ev)
                    if ev.kind == Event::ADAPTER_FAILED && ev.payload["adapter"] == INSTANCE =>
                {
                    let d = &ev.payload["detail"];
                    Some(Failure {
                        time: ev.time,
                        error: d["error"].as_str().unwrap_or_default().to_owned(),
                        fatal: d["fatal"].as_bool().unwrap_or(false),
                    })
                }
                _ => None,
            })
            .collect()
    }

    fn stats(&self) -> AdapterStats {
        self.run
            .as_ref()
            .and_then(|r| r.telemetry.get(INSTANCE))
            .unwrap_or_default()
    }

    fn audit(&self) -> Vec<AuditRecord> {
        self.run
            .as_ref()
            .map_or_else(Vec::new, |r| r.audit.records())
    }

    fn inject(&mut self, fault: Option<Fault>) -> bool {
        self.upstream().inject(fault)
    }

    fn traffic(&self) -> Option<Traffic> {
        self.upstream.as_deref().and_then(|u| u.traffic())
    }

    fn stop(&mut self) -> AdapterStats {
        let Some(run) = &mut self.run else {
            return AdapterStats::default();
        };
        run.trigger.trigger();
        if let Some(handle) = run.handle.take() {
            match self.rt.block_on(handle) {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("heimr-testkit: the app failed: {e}"),
                Err(e) => eprintln!("heimr-testkit: the app task failed: {e}"),
            }
        }
        self.stats()
    }
}

impl Drop for InProcess {
    fn drop(&mut self) {
        if self.run.as_ref().is_some_and(|r| r.handle.is_some()) {
            self.stop();
        }
    }
}
