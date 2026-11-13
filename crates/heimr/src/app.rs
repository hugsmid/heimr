//! The application: registered adapter kinds + config → supervised
//! instances.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use heimr_core::{
    ACTUATOR, ActorError, Actuator, ActuatorError, Adapter, AdapterDescriptor, AdapterError,
    AdapterFactory, AdapterInstance, AuditLog, Capabilities, CommandSubmitter, ContextParts,
    Emitter, Envelope, Event, InstanceSpec, Mode, RegisterError, SharedClock, Shutdown,
    ShutdownTrigger, Site, StatsHandle, SystemClock, TypedFactory, shutdown_channel,
};
use jiff::SignedDuration;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinError, JoinSet};
use tracing::Instrument;

use crate::config::{Config, ConfigError, Supervision};
use crate::journal::{JournalOptions, JsonlJournal};
use crate::sink::{LoggingSink, Sink};
use crate::telemetry::Telemetry;

/// Source name on records the app itself emits (adapter lifecycle).
pub const APP_SOURCE: &str = "heimr";
/// Source name on the actuator's audit records forwarded to sinks.
pub const ACTUATOR_SOURCE: &str = ACTUATOR;
const RECORD_QUEUE: usize = 1024;

/// Errors from building or running the app. Adapter failures are not among
/// them: a failing instance is restarted or kept down, and the app runs on.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The configuration is invalid.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// An adapter kind was registered twice, or with an invalid descriptor.
    #[error("adapter registration: {0}")]
    Registration(String),
    /// The actuator refused an instance.
    #[error(transparent)]
    Register(#[from] RegisterError),
    /// The actuator could not start (e.g. the audit trail is unreadable).
    #[error(transparent)]
    Actuator(#[from] ActuatorError),
    /// A decider's actor name was refused.
    #[error(transparent)]
    Actor(#[from] ActorError),
    /// A supervisor task panicked (a bug in heimr, not in an adapter).
    #[error("supervisor for {0} panicked")]
    SupervisorPanicked(String),
    /// Filesystem setup failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// Cause.
        source: std::io::Error,
    },
}

/// What [`AppBuilder::check`] found for one configured instance.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AdapterPlan {
    /// Instance name.
    pub name: String,
    /// The kind's descriptor.
    pub descriptor: AdapterDescriptor,
    /// What this instance can do.
    pub capabilities: Capabilities,
    /// Whether the instance is enabled.
    pub enabled: bool,
    /// Shadow or live.
    pub mode: Mode,
}

struct Prepared {
    plan: AdapterPlan,
    factory: Arc<dyn AdapterFactory>,
    settings: serde_json::Value,
    instance: Box<dyn AdapterInstance>,
}

/// Collects adapter kinds, sinks, a clock and an audit log, then builds an
/// [`App`].
pub struct AppBuilder {
    config: Config,
    kinds: BTreeMap<String, Arc<dyn AdapterFactory>>,
    problems: Vec<String>,
    sinks: Vec<Arc<dyn Sink>>,
    clock: Option<SharedClock>,
    audit: Option<Arc<dyn AuditLog>>,
}

impl std::fmt::Debug for AppBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppBuilder")
            .field("kinds", &self.kinds.keys().collect::<Vec<_>>())
            .field(
                "sinks",
                &self.sinks.iter().map(|s| s.name()).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

fn adapter_error(instance: &str, e: AdapterError) -> ConfigError {
    match e {
        AdapterError::Config {
            key: Some(key),
            message,
            ..
        } => ConfigError::invalid(format!("adapters.{instance}.{key}"), message),
        other => ConfigError::invalid(format!("adapters.{instance}"), other.to_string()),
    }
}

impl AppBuilder {
    /// Register a typed adapter kind (wrapped in a [`TypedFactory`]).
    #[must_use]
    pub fn adapter<A: Adapter>(self) -> Self {
        self.factory(TypedFactory::<A>::new())
    }

    /// Register an adapter kind through its object-safe factory (e.g. one
    /// whose descriptor arrives at run time from another process).
    #[must_use]
    pub fn factory(mut self, factory: impl AdapterFactory) -> Self {
        let d = factory.descriptor();
        if let Err(e) = d.validate() {
            self.problems
                .push(format!("adapter kind {:?}: {e}", d.kind));
        } else {
            match self.kinds.entry(d.kind) {
                std::collections::btree_map::Entry::Occupied(e) => {
                    let msg = format!("adapter kind {:?} registered twice", e.key());
                    self.problems.push(msg);
                }
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert(Arc::new(factory));
                }
            }
        }
        self
    }

    /// Register the adapter kinds that ship with heimr: `demo`
    /// ([`Demo`](crate::demo::Demo)) and `system` (host metrics,
    /// [`heimr_system::System`]).
    #[must_use]
    pub fn public_adapters(self) -> Self {
        self.adapter::<crate::demo::Demo>()
            .adapter::<heimr_system::System>()
    }

    /// Add a sink (in addition to the logging sink, if enabled in config).
    #[must_use]
    pub fn sink(mut self, sink: impl Sink) -> Self {
        self.sinks.push(Arc::new(sink));
        self
    }

    /// Use this clock instead of the system clock (tests).
    #[must_use]
    pub fn clock(mut self, clock: SharedClock) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Use this audit log instead of the journal in `<state_dir>/audit/`.
    #[must_use]
    pub fn audit_log(mut self, log: Arc<dyn AuditLog>) -> Self {
        self.audit = Some(log);
        self
    }

    /// The registered kinds' descriptors.
    pub fn kinds(&self) -> Vec<AdapterDescriptor> {
        self.kinds.values().map(|f| f.descriptor()).collect()
    }

    /// Validate the configuration against the registered kinds: unknown
    /// kinds, unknown keys, adapter-specific checks, `live` on read-only
    /// instances. Disabled instances are validated too. Touches no state;
    /// typed adapters do no I/O here, while an out-of-process adapter's
    /// factory may ask its adapter (see [`AdapterFactory::prepare`]).
    pub async fn check(&self) -> Result<Vec<AdapterPlan>, Error> {
        Ok(self.prepare().await?.into_iter().map(|p| p.plan).collect())
    }

    async fn prepare(&self) -> Result<Vec<Prepared>, Error> {
        if let Some(p) = self.problems.first() {
            return Err(Error::Registration(p.clone()));
        }
        self.config.validate()?;
        let site = self.config.site()?;
        let mut out = Vec::new();
        for (name, section) in &self.config.adapters {
            let Some(factory) = self.kinds.get(&section.kind) else {
                let known: Vec<_> = self.kinds.keys().map(String::as_str).collect();
                return Err(ConfigError::invalid(
                    format!("adapters.{name}.type"),
                    format!(
                        "no adapter kind {:?} in this build (available: {})",
                        section.kind,
                        known.join(", ")
                    ),
                )
                .into());
            };
            let descriptor = factory.descriptor();
            let instance = factory
                .prepare(&section.settings, &site)
                .await
                .map_err(|e| adapter_error(name, e))?;
            let capabilities = instance.capabilities();
            capabilities
                .validate()
                .map_err(|e| ConfigError::invalid(format!("adapters.{name}"), e.to_string()))?;
            if !descriptor.capabilities.includes(&capabilities) {
                return Err(ConfigError::invalid(
                    format!("adapters.{name}"),
                    format!(
                        "instance capabilities exceed those of kind {:?}",
                        descriptor.kind
                    ),
                )
                .into());
            }
            if section.mode == Mode::Live && !capabilities.can_write() {
                return Err(ConfigError::invalid(
                    format!("adapters.{name}.mode"),
                    "this adapter is read-only and cannot be live",
                )
                .into());
            }
            out.push(Prepared {
                plan: AdapterPlan {
                    name: name.clone(),
                    descriptor,
                    capabilities,
                    enabled: section.enabled,
                    mode: section.mode,
                },
                factory: factory.clone(),
                settings: section.settings.clone(),
                instance,
            });
        }
        Ok(out)
    }

    /// Validate and wire everything: opens (creating if needed) the state
    /// directory and the audit journal, and restores the actuator's rate
    /// limits and id sequence from it. Adapters start in [`App::run`].
    pub async fn build(self) -> Result<App, Error> {
        let prepared = self.prepare().await?;
        let config = &self.config;
        let clock = self.clock.clone().unwrap_or_else(|| Arc::new(SystemClock));
        let state_dir = config.heimr.state_dir.clone();
        let audit: Arc<dyn AuditLog> = match self.audit.clone() {
            Some(a) => a,
            None => {
                let dir = state_dir.join("audit");
                let opts =
                    JournalOptions::new(config.audit.max_file_bytes, config.audit.retain_files);
                Arc::new(JsonlJournal::open(&dir, opts).map_err(|source| Error::Io {
                    context: format!("opening the audit journal in {}", dir.display()),
                    source,
                })?)
            }
        };
        let (records_tx, records_rx) = mpsc::channel(RECORD_QUEUE);
        let actuator = Actuator::builder(config.safety.policy()?, clock.clone(), audit)
            .audit_timeout(
                Duration::try_from(config.audit.append_timeout).unwrap_or(Duration::from_secs(2)),
            )
            .events(Emitter::core(ACTUATOR_SOURCE, records_tx.clone()))
            .build()
            .await?;
        let mut supervised = Vec::new();
        for p in prepared.into_iter().filter(|p| p.plan.enabled) {
            let name = p.plan.name.clone();
            let spec = InstanceSpec::new(
                &name,
                &p.plan.descriptor.kind,
                p.plan.capabilities.clone(),
                p.plan.mode,
            )
            .with_safety(config.instance_safety(&name)?);
            actuator.register(spec)?;
            let stats = actuator
                .stats(&name)
                .ok_or_else(|| Error::Registration(format!("no stats for {name}")))?;
            supervised.push(Supervised {
                state_dir: state_dir.join("adapters").join(&name),
                name,
                kind: p.plan.descriptor.kind,
                mode: p.plan.mode,
                capabilities: p.plan.capabilities,
                factory: p.factory,
                settings: p.settings,
                stats,
                first: Some(p.instance),
            });
        }
        let telemetry =
            Telemetry::new(supervised.iter().map(|s| (s.name.clone(), s.stats.clone())));
        let mut sinks = self.sinks;
        if self.config.sinks.logging {
            sinks.insert(0, Arc::new(LoggingSink));
        }
        let (trigger, shutdown) = shutdown_channel();
        Ok(App {
            site: config.site()?,
            grace: self.config.heimr.shutdown_grace,
            supervision: self.config.supervision,
            supervised,
            actuator,
            telemetry,
            sinks,
            clock,
            records_tx,
            records_rx,
            trigger,
            shutdown,
        })
    }
}

/// One instance and everything needed to (re)build and run it.
struct Supervised {
    name: String,
    kind: String,
    mode: Mode,
    capabilities: Capabilities,
    factory: Arc<dyn AdapterFactory>,
    settings: serde_json::Value,
    stats: StatsHandle,
    state_dir: PathBuf,
    /// The instance prepared at build time, used for the first run.
    first: Option<Box<dyn AdapterInstance>>,
}

/// A built application. Get handles ([`App::submitter`],
/// [`App::telemetry`], [`App::shutdown_trigger`]) before calling
/// [`App::run`].
pub struct App {
    site: Site,
    grace: SignedDuration,
    supervision: Supervision,
    supervised: Vec<Supervised>,
    actuator: Actuator,
    telemetry: Telemetry,
    sinks: Vec<Arc<dyn Sink>>,
    clock: SharedClock,
    records_tx: mpsc::Sender<Envelope>,
    records_rx: mpsc::Receiver<Envelope>,
    trigger: ShutdownTrigger,
    shutdown: Shutdown,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("adapters", &self.adapters())
            .finish_non_exhaustive()
    }
}

impl App {
    /// Start building an app from a validated config.
    pub fn builder(config: Config) -> AppBuilder {
        AppBuilder {
            config,
            kinds: BTreeMap::new(),
            problems: Vec::new(),
            sinks: Vec::new(),
            clock: None,
            audit: None,
        }
    }

    /// A submit-only handle to the command gate for one decider (the
    /// planner, the assistant, a front-end user, a test), recorded as the
    /// actor of every command it submits. Names reserved for the core
    /// (`actuator`, `core`, `heimr…`) are refused. The handle cannot
    /// register instances or touch stats.
    pub fn submitter(&self, actor: impl Into<String>) -> Result<CommandSubmitter, Error> {
        Ok(self.actuator.submitter(actor)?)
    }

    /// Per-instance stats (counts, health, runs and restarts). Keep a clone
    /// to read them while and after the app runs.
    pub fn telemetry(&self) -> Telemetry {
        self.telemetry.clone()
    }

    /// Stops a running app, like SIGTERM.
    pub fn shutdown_trigger(&self) -> ShutdownTrigger {
        self.trigger.clone()
    }

    /// The site's time zone and currency, from `[site]`.
    pub fn site(&self) -> &Site {
        &self.site
    }

    /// Enabled instances and their modes.
    pub fn adapters(&self) -> Vec<(String, Mode)> {
        self.supervised
            .iter()
            .map(|a| (a.name.clone(), a.mode))
            .collect()
    }

    /// Run until SIGINT or SIGTERM (or [`App::shutdown_trigger`]).
    pub async fn run(self) -> Result<(), Error> {
        self.run_until(shutdown_signal()).await
    }

    /// Run until `stop` resolves (or [`App::shutdown_trigger`] fires). Each
    /// instance runs under its own supervisor: a failing instance is
    /// restarted with backoff, or kept down after a fatal error or repeated
    /// panics, while the rest keep running. At the end, signal shutdown,
    /// give instances the configured grace period, abort stragglers and
    /// flush every record to the sinks. Only core failures make this return
    /// an error.
    pub async fn run_until(self, stop: impl Future<Output = ()> + Send) -> Result<(), Error> {
        let Self {
            site,
            grace,
            supervision,
            supervised,
            actuator,
            telemetry: _,
            sinks,
            clock,
            records_tx,
            records_rx,
            trigger,
            shutdown,
        } = self;

        let (pump_stop_tx, pump_stop_rx) = oneshot::channel();
        let pump = tokio::spawn(pump(records_rx, sinks, pump_stop_rx));

        let mut failure = None;
        for s in &supervised {
            if let Err(source) = std::fs::create_dir_all(&s.state_dir) {
                failure = Some(Error::Io {
                    context: format!("creating state directory {}", s.state_dir.display()),
                    source,
                });
                break;
            }
        }
        let env = Env {
            actuator: actuator.clone(),
            clock,
            shutdown: shutdown.clone(),
            records: records_tx.clone(),
            events: Emitter::core(APP_SOURCE, records_tx),
            grace: Duration::try_from(grace).unwrap_or_default(),
            policy: supervision,
            site,
        };
        let mut tasks = JoinSet::new();
        let mut names = BTreeMap::new();
        if failure.is_none() {
            for s in supervised {
                let name = s.name.clone();
                let handle = tasks.spawn(supervise(s, env.clone()));
                names.insert(handle.id(), name);
            }
        }
        drop(env);

        if failure.is_none() {
            tokio::pin!(stop);
            loop {
                tokio::select! {
                    () = &mut stop => { tracing::info!("shutdown requested"); break; }
                    () = shutdown.wait() => { tracing::info!("shutdown triggered"); break; }
                    Some(joined) = tasks.join_next() => {
                        if let Err(e) = joined {
                            let name = names.get(&e.id()).cloned().unwrap_or_default();
                            tracing::error!(adapter = %name, "supervisor panicked; stopping");
                            failure = Some(Error::SupervisorPanicked(name));
                            break;
                        }
                        // A supervisor that returns leaves its instance down
                        // (fatal error); the app keeps running.
                    }
                }
            }
        }

        trigger.trigger();
        while let Some(joined) = tasks.join_next().await {
            if let Err(e) = joined
                && failure.is_none()
            {
                let name = names.get(&e.id()).cloned().unwrap_or_default();
                failure = Some(Error::SupervisorPanicked(name));
            }
        }
        // No instance runs any more; flush.
        drop(actuator);
        let _ = pump_stop_tx.send(());
        let _ = pump.await;
        tracing::info!("stopped");
        failure.map_or(Ok(()), Err)
    }
}

#[derive(Clone)]
struct Env {
    actuator: Actuator,
    clock: SharedClock,
    shutdown: Shutdown,
    records: mpsc::Sender<Envelope>,
    events: Emitter,
    grace: Duration,
    policy: Supervision,
    site: Site,
}

impl Env {
    async fn lifecycle(&self, kind: &'static str, name: &str, detail: serde_json::Value) {
        let ev = Event::new(
            self.clock.now(),
            kind,
            APP_SOURCE,
            json!({ "adapter": name, "detail": detail }),
        );
        let _ = self.events.emit(ev).await;
    }
}

enum Exit {
    Returned,
    Failed(AdapterError),
    Panicked,
    Aborted,
}

impl Exit {
    fn from_join(joined: Result<Result<(), AdapterError>, JoinError>) -> Self {
        match joined {
            Ok(Ok(())) => Self::Returned,
            Ok(Err(e)) => Self::Failed(e),
            Err(e) if e.is_panic() => Self::Panicked,
            Err(_) => Self::Aborted,
        }
    }
}

/// FNV-1a, to give each instance its own deterministic jitter sequence.
fn seed(name: &str) -> u64 {
    name.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// xorshift64*: deterministic jitter.
struct Jitter(u64);

impl Jitter {
    fn unit(&mut self) -> f64 {
        let mut x = self.0 | 1;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Delay before restart number `failures` (1-based): `initial · 2^(n-1)`,
/// capped at `max`, then jittered uniformly into its upper half.
fn backoff(policy: &Supervision, failures: u32, jitter: &mut Jitter) -> SignedDuration {
    let exp = failures.saturating_sub(1).min(30);
    let base = policy
        .backoff_initial
        .checked_mul(1 << exp)
        .unwrap_or(policy.backoff_max)
        .min(policy.backoff_max);
    let nanos = base.as_nanos() as f64;
    let jittered = nanos / 2.0 + jitter.unit() * nanos / 2.0;
    SignedDuration::from_nanos(jittered as i64).min(base)
}

/// Run one instance until shutdown, restarting it after transient failures.
/// A fatal error, or `max_consecutive_panics` panics without a healthy run
/// in between, keeps it down.
async fn supervise(mut s: Supervised, env: Env) {
    let mut jitter = Jitter(seed(&s.name));
    let mut failures: u32 = 0;
    let mut panics: u32 = 0;
    loop {
        if env.shutdown.is_triggered() {
            s.stats.run_stopped(env.clock.now(), None);
            return;
        }
        let instance = match s.first.take() {
            Some(i) => Ok(i),
            None => s.factory.prepare(&s.settings, &env.site).await,
        };
        let instance = match instance {
            Ok(i) if s.capabilities.includes(&i.capabilities()) => i,
            Ok(_) => {
                return stay_down(
                    &s.name,
                    &s.stats,
                    &env,
                    failures + 1,
                    "capabilities changed on rebuild".into(),
                )
                .await;
            }
            Err(e) => {
                return stay_down(
                    &s.name,
                    &s.stats,
                    &env,
                    failures + 1,
                    format!("rebuilding failed: {e}"),
                )
                .await;
            }
        };
        let commands = match env.actuator.attach(&s.name).await {
            Ok(c) => c,
            Err(e) => return stay_down(&s.name, &s.stats, &env, failures + 1, e.to_string()).await,
        };
        let emitter =
            Emitter::for_adapter(s.name.as_str(), env.records.clone()).with_stats(s.stats.clone());
        let parts = ContextParts::new(
            emitter,
            env.clock.clone(),
            env.shutdown.clone(),
            s.state_dir.clone(),
            // The adapter may report health and metrics, not run state.
            s.stats.for_instance(),
            env.site.clone(),
        )
        .with_commands(commands);
        s.stats.run_started(env.clock.now());
        let restarts = s.stats.snapshot().restarts;
        tracing::info!(adapter = %s.name, kind = %s.kind, mode = s.mode.as_str(), restarts, "starting adapter");
        env.lifecycle(
            Event::ADAPTER_STARTED,
            &s.name,
            json!({ "kind": s.kind, "mode": s.mode.as_str(), "restarts": restarts }),
        )
        .await;
        let started = env.clock.monotonic();
        // Everything the instance logs is attributed to it.
        let span = tracing::info_span!("adapter", adapter = %s.name, kind = %s.kind, mode = s.mode.as_str());
        let mut task = tokio::spawn(instance.run(parts).instrument(span));
        let (exit, stopping) = tokio::select! {
            joined = &mut task => (Exit::from_join(joined), env.shutdown.is_triggered()),
            () = env.shutdown.wait() => {
                let exit = match tokio::time::timeout(env.grace, &mut task).await {
                    Ok(joined) => Exit::from_join(joined),
                    Err(_) => {
                        tracing::warn!(adapter = %s.name, "grace period over; aborting");
                        task.abort();
                        let _ = task.await;
                        Exit::Aborted
                    }
                };
                (exit, true)
            }
        };
        // Close this run's queue: undelivered commands are audited as
        // dropped, never replayed to a later run.
        env.actuator.detach(&s.name).await;
        let now = env.clock.now();
        if stopping {
            let error = match exit {
                Exit::Returned => None,
                Exit::Failed(e) => Some(e.to_string()),
                Exit::Panicked => Some("panicked".to_owned()),
                Exit::Aborted => Some("aborted after the shutdown grace period".to_owned()),
            };
            s.stats.run_stopped(now, error.clone());
            env.lifecycle(Event::ADAPTER_STOPPED, &s.name, json!({ "error": error }))
                .await;
            return;
        }
        if env.clock.monotonic() - started >= env.policy.reset_after {
            // A healthy period: start counting afresh.
            failures = 0;
            panics = 0;
        }
        failures = failures.saturating_add(1);
        let (message, fatal) = match exit {
            Exit::Returned => ("returned while the app was running".to_owned(), false),
            Exit::Failed(e) => (e.to_string(), e.is_fatal()),
            Exit::Panicked => {
                panics = panics.saturating_add(1);
                if panics >= env.policy.max_consecutive_panics {
                    (
                        format!("panicked {panics} times without a healthy run; kept down"),
                        true,
                    )
                } else {
                    ("panicked".to_owned(), false)
                }
            }
            Exit::Aborted => ("aborted".to_owned(), false),
        };
        if fatal {
            return stay_down(&s.name, &s.stats, &env, failures, message).await;
        }
        let delay = backoff(&env.policy, failures, &mut jitter);
        let restart_at = now.saturating_add(delay).ok();
        tracing::error!(adapter = %s.name, error = %message, failures, restart_in = %format!("{delay:#}"), "adapter failed; restarting");
        s.stats
            .run_failed(now, message.clone(), failures, restart_at);
        env.lifecycle(
            Event::ADAPTER_FAILED,
            &s.name,
            json!({
                "error": message,
                "fatal": false,
                "failures": failures,
                "restart_in_ms": i64::try_from(delay.as_millis()).unwrap_or(i64::MAX),
            }),
        )
        .await;
        tokio::select! {
            () = env.clock.sleep(delay) => {}
            () = env.shutdown.wait() => {}
        }
    }
}

/// A fatal failure (or too many panics): the instance stays down and
/// unhealthy.
async fn stay_down(name: &str, stats: &StatsHandle, env: &Env, failures: u32, message: String) {
    tracing::error!(adapter = %name, error = %message, "adapter failed fatally; it stays down");
    stats.run_failed(env.clock.now(), message.clone(), failures, None);
    env.lifecycle(
        Event::ADAPTER_FAILED,
        name,
        json!({ "error": message, "fatal": true, "failures": failures }),
    )
    .await;
}

/// Feed records to the sinks until told to stop, then drain what is queued.
async fn pump(
    mut rx: mpsc::Receiver<Envelope>,
    sinks: Vec<Arc<dyn Sink>>,
    mut stop: oneshot::Receiver<()>,
) {
    let deliver = |env: &Envelope| {
        for s in &sinks {
            s.record(env);
        }
    };
    loop {
        tokio::select! {
            biased;
            msg = rx.recv() => match msg {
                Some(env) => deliver(&env),
                None => return,
            },
            _ = &mut stop => break,
        }
    }
    while let Ok(env) = rx.try_recv() {
        deliver(&env);
    }
}

/// Resolves on SIGINT (Ctrl-C) or, on Unix, SIGTERM.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = term => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_is_capped_and_jittered() {
        let policy = Supervision::default();
        let mut j = Jitter(seed("x"));
        let mut secs = |n| backoff(&policy, n, &mut j);
        for n in 1..=20u32 {
            let d = secs(n);
            let base = SignedDuration::from_secs(1)
                .checked_mul(1 << (n - 1).min(30))
                .unwrap_or(SignedDuration::MAX)
                .min(policy.backoff_max);
            assert!(d >= base / 2 && d <= base, "{n}: {d:#} vs {base:#}");
        }
        let mut a = Jitter(seed("same"));
        let mut b = Jitter(seed("same"));
        assert_eq!(backoff(&policy, 3, &mut a), backoff(&policy, 3, &mut b));
    }
}
