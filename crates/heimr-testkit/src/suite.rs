//! The author-facing entry point: [`Suite`].

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use heimr_core::{Adapter, AdapterFactory, Command, Site, TypedFactory};
use jiff::SignedDuration;
use serde_json::Value;

use crate::driver::Driver;
use crate::inprocess::{ClockMode, DriverOptions, InProcess, UpstreamFactory};
use crate::scenario::{
    Expectations, Expected, InvalidCase, Report, Scenario, ScenarioContext, Verdict, catalogue,
    judge,
};
use crate::upstream::{StaticUpstream, Upstream};
use crate::util::panic_message;

/// Environment variable naming scenario ids (comma-separated) to run;
/// everything else is reported as filtered. For debugging one scenario. An
/// id that is not in the catalogue is an error that fails the run.
pub const ONLY_ENV: &str = "HEIMR_TESTKIT_ONLY";

/// A fixed site for tests: the value the kit runs adapters for unless a
/// test sets another ([`Suite::site`], [`Harness::start_with`]). It is a
/// test fixture, nothing more: a real hub has no default site, and its
/// configuration must set `[site]`.
///
/// [`Harness::start_with`]: crate::Harness::start_with
pub fn test_site() -> Site {
    Site::new("Europe/Berlin", "EUR")
        .expect("the time zone database (tzdb) must contain Europe/Berlin")
}

/// Run `scenarios` against fresh drivers from `make_driver`, one driver per
/// scenario. This is the whole runner: an out-of-process host passes its
/// own driver factory and gets the same report.
///
/// Reads [`ONLY_ENV`]. Unknown ids there or in the declarations, and
/// conflicting declarations, are reported in [`Report::errors`].
pub fn run_scenarios(
    kind: &str,
    scenarios: &[&dyn Scenario],
    expect: &Expectations,
    make_driver: &mut dyn FnMut() -> Result<Box<dyn Driver>, String>,
) -> Report {
    let only: Option<Vec<String>> = std::env::var(ONLY_ENV).ok().map(|v| {
        v.split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    });
    let known = |id: &str| scenarios.iter().any(|s| s.id() == id);
    let mut errors = expect.declaration_errors.clone();
    for id in expect.declarations.keys().filter(|id| !known(id)) {
        errors.push(format!("declaration for unknown scenario id {id:?}"));
    }
    for id in only.iter().flatten().filter(|id| !known(id)) {
        errors.push(format!("{ONLY_ENV} names unknown scenario id {id:?}"));
    }
    let mut results = Vec::new();
    for scenario in scenarios {
        let verdict = if only
            .as_ref()
            .is_some_and(|o| !o.iter().any(|id| id == scenario.id()))
        {
            None
        } else {
            Some(match make_driver() {
                Err(e) => Verdict::Fail(format!("could not create a driver: {e}")),
                Ok(mut driver) => {
                    let run = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        let mut cx = ScenarioContext {
                            driver: driver.as_mut(),
                            expect,
                        };
                        scenario.run(&mut cx)
                    }));
                    run.unwrap_or_else(|p| {
                        Verdict::Fail(format!("the scenario panicked: {}", panic_message(&*p)))
                    })
                }
            })
        };
        results.push(judge(*scenario, verdict, expect));
    }
    Report {
        kind: kind.to_owned(),
        results,
        errors,
    }
}

/// The conformance suite for one adapter kind.
///
/// ```no_run
/// use heimr_testkit::Suite;
/// use serde_json::json;
/// # struct MyAdapter;
/// # impl heimr_core::Adapter for MyAdapter {
/// #     type Config = serde::de::IgnoredAny;
/// #     fn descriptor() -> heimr_core::AdapterDescriptor { unimplemented!() }
/// #     fn new(_: &Self::Config) -> Result<Self, heimr_core::AdapterError> { Ok(Self) }
/// #     async fn run(self, _: heimr_core::AdapterContext<Self::Config>) -> Result<(), heimr_core::AdapterError> { Ok(()) }
/// # }
///
/// #[test]
/// fn conformance() {
///     Suite::new::<MyAdapter>(json!({ "interval": "10s" }))
///         .invalid("interval", json!({ "interval": "0s" }))
///         .expect_skip("fault.rejected_is_fatal", "the device has no credentials")
///         .run()
///         .assert_passed();
/// }
/// ```
///
/// Call it from a plain `#[test]`, not `#[tokio::test]`: the suite runs its
/// own runtime.
pub struct Suite {
    factory: Arc<dyn AdapterFactory>,
    upstreams: UpstreamFactory,
    expect: Expectations,
    clock: ClockMode,
    site: Option<Site>,
}

impl std::fmt::Debug for Suite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Suite")
            .field("factory", &self.factory)
            .field("expect", &self.expect)
            .field("clock", &self.clock)
            .finish_non_exhaustive()
    }
}

impl Suite {
    /// A suite for adapter `A` with fixed `settings` and no upstream to
    /// fault or observe.
    pub fn new<A: Adapter>(settings: Value) -> Self {
        Self::for_factory(TypedFactory::<A>::new(), move || {
            let s = settings.clone();
            async move { StaticUpstream(s) }
        })
    }

    /// A suite for adapter `A` against an upstream made fresh for every run
    /// by `make` (for example a [`ReplayServer`](crate::replay::ReplayServer)
    /// or a fixture file tree).
    pub fn with_upstream<A, U, F, Fut>(make: F) -> Self
    where
        A: Adapter,
        U: Upstream,
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = U> + Send + 'static,
    {
        Self::for_factory(TypedFactory::<A>::new(), make)
    }

    /// A suite for an HTTP-backed adapter `A` against a fresh
    /// [`ReplayServer`](crate::replay::ReplayServer) serving `cassette` for
    /// every run, with `settings` building the instance settings from the
    /// server's base URL. Uses [`ClockMode::manual`], since the adapter does
    /// real I/O.
    #[cfg(feature = "replay")]
    pub fn replay<A: Adapter>(
        cassette: crate::replay::Cassette,
        settings: impl Fn(&str) -> Value + Send + Sync + 'static,
    ) -> Self {
        let settings = Arc::new(settings);
        Self::with_upstream::<A, _, _, _>(move || {
            let cassette = cassette.clone();
            let settings = settings.clone();
            async move {
                crate::replay::ReplayServer::start(cassette)
                    .await
                    .upstream(move |uri| settings(uri))
            }
        })
        .clock(ClockMode::manual())
    }

    /// A suite for any [`AdapterFactory`].
    pub fn for_factory<U, F, Fut>(factory: impl AdapterFactory, make: F) -> Self
    where
        U: Upstream,
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = U> + Send + 'static,
    {
        let make = Arc::new(make);
        let upstreams: UpstreamFactory = Arc::new(move || {
            let fut = make();
            Box::pin(async move { Box::new(fut.await) as Box<dyn Upstream> })
        });
        Self {
            factory: Arc::new(factory),
            upstreams,
            expect: Expectations::default(),
            clock: ClockMode::Paused,
            site: None,
        }
    }

    /// The adapter must refuse the settings with `patch` merged in (`null`
    /// removes a key), with a configuration error naming `key`.
    #[must_use]
    pub fn invalid(mut self, key: &str, patch: Value) -> Self {
        self.expect.invalid.push(InvalidCase {
            patch,
            key: Some(key.to_owned()),
        });
        self
    }

    /// Like [`Suite::invalid`], for an error that need not name a key.
    #[must_use]
    pub fn invalid_any(mut self, patch: Value) -> Self {
        self.expect.invalid.push(InvalidCase { patch, key: None });
        self
    }

    /// Observe healthy runs for this long (fake time; default 2 minutes).
    #[must_use]
    pub fn warmup(mut self, d: SignedDuration) -> Self {
        self.expect.warmup = d;
        self
    }

    /// `run` must return this soon after shutdown (default 1 s).
    #[must_use]
    pub fn shutdown_deadline(mut self, d: SignedDuration) -> Self {
        self.expect.shutdown_deadline = d;
        self
    }

    /// Whether a healthy run emits records (default true).
    #[must_use]
    pub fn emits(mut self, emits: bool) -> Self {
        self.expect.emits = emits;
        self
    }

    /// Allow records stamped up to `d` before the run started (default 31
    /// days).
    #[must_use]
    pub fn max_backfill(mut self, d: SignedDuration) -> Self {
        self.expect.max_backfill = d;
        self
    }

    /// Allow records stamped up to `d` ahead of the clock (default 1 s).
    #[must_use]
    pub fn max_future(mut self, d: SignedDuration) -> Self {
        self.expect.max_future = d;
        self
    }

    /// Submit these commands in the command scenarios instead of generated
    /// ones.
    #[must_use]
    pub fn commands(mut self, commands: Vec<Command>) -> Self {
        self.expect.commands = Some(commands);
        self
    }

    /// Address generated commands to this zone or device (default
    /// `zone-a`).
    #[must_use]
    pub fn command_target(mut self, target: &str) -> Self {
        self.expect.command_target = target.to_owned();
        self
    }

    /// Length of injected outages (default 2 minutes).
    #[must_use]
    pub fn outage(mut self, d: SignedDuration) -> Self {
        self.expect.outage = d;
        self
    }

    /// Time allowed to emit again after an outage (default 5 minutes).
    #[must_use]
    pub fn recovery(mut self, d: SignedDuration) -> Self {
        self.expect.recovery = d;
        self
    }

    /// Upstream requests per minute allowed during an outage (default 60).
    #[must_use]
    pub fn max_attempts_per_minute(mut self, n: u64) -> Self {
        self.expect.max_attempts_per_minute = n;
        self
    }

    /// Declare that scenario `id` currently fails, and why. The report
    /// shows it as an expected failure; if it passes or is skipped instead,
    /// [`Report::assert_passed`] fails until the declaration is removed.
    #[must_use]
    pub fn expect_failure(mut self, id: &str, why: &str) -> Self {
        self.expect.declare(id, Expected::Failure, why);
        self
    }

    /// Declare that scenario `id` is skipped for this adapter, and why
    /// (a read-only adapter skips the command scenarios; a fixture that
    /// cannot simulate faults skips the fault scenarios). Undeclared skips
    /// fail [`Report::assert_passed`], and so does a declared skip that
    /// does not happen.
    #[must_use]
    pub fn expect_skip(mut self, id: &str, why: &str) -> Self {
        self.expect.declare(id, Expected::Skip, why);
        self
    }

    /// Declare that scenario `id` passes with a note (part of it cannot be
    /// observed with this fixture), and why. Undeclared notes fail
    /// [`Report::assert_passed`], and so does a declared note that does
    /// not happen.
    #[must_use]
    pub fn expect_note(mut self, id: &str, why: &str) -> Self {
        self.expect.declare(id, Expected::Note, why);
        self
    }

    /// How the fake clock moves (default [`ClockMode::Paused`]).
    #[must_use]
    pub fn clock(mut self, clock: ClockMode) -> Self {
        self.clock = clock;
        self
    }

    /// The site the app runs for (default [`test_site`]).
    #[must_use]
    pub fn site(mut self, site: Site) -> Self {
        self.site = Some(site);
        self
    }

    /// The declarations so far.
    pub fn expectations(&self) -> &Expectations {
        &self.expect
    }

    /// Run every scenario and return the report (also printed, so
    /// `--nocapture` shows it).
    pub fn run(self) -> Report {
        let site = self.site.clone().unwrap_or_else(test_site);
        let mut options = DriverOptions::new(site);
        options.clock = self.clock;
        options.shutdown_deadline = self.expect.shutdown_deadline;
        let factory = self.factory.clone();
        let upstreams = self.upstreams.clone();
        let mut make = || -> Result<Box<dyn Driver>, String> {
            InProcess::new(factory.clone(), upstreams.clone(), options.clone())
                .map(|d| Box::new(d) as Box<dyn Driver>)
                .map_err(|e| e.to_string())
        };
        let kind = self.factory.descriptor().kind;
        let report = run_scenarios(&kind, &catalogue(), &self.expect, &mut make);
        println!("{report}");
        report
    }
}
