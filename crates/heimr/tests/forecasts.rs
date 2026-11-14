//! The app wires each instance's emitter with its kind's source currency
//! and the configured forecast horizon.

#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use heimr::core::{
    Adapter, AdapterContext, AdapterDescriptor, AdapterError, Basis, Capabilities, Currency,
    DeviceId, Metric, RunState, SeriesTemplate, TokioClock, Unit,
};
use heimr::{App, Config, MemorySink};
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;

fn t0() -> Timestamp {
    "2030-01-01T00:00:00Z".parse().unwrap()
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("heimr-fc-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PricesConfig {
    /// How far ahead of now the one forecast ends.
    ahead: SignedDuration,
    unit: Unit,
    /// How far after now the forecast claims to be issued.
    #[serde(default)]
    issued_ahead: SignedDuration,
}

/// Emits one forecast, then waits for shutdown. Generic over whether the
/// kind declares a currency.
struct Prices<const DECLARED: bool>;

impl<const DECLARED: bool> Adapter for Prices<DECLARED> {
    type Config = PricesConfig;

    fn descriptor() -> AdapterDescriptor {
        const USD: Currency = Currency::of("USD");
        // The undeclared kind's template is in °C: its money is refused
        // for the missing currency before the template is consulted.
        let unit = if DECLARED {
            Unit::CurrencyPerKilowattHour
        } else {
            Unit::Celsius
        };
        let d = AdapterDescriptor::new(
            if DECLARED { "prices" } else { "undeclared" },
            "one forecast",
            Capabilities::read_only(),
        )
        .with_series(
            SeriesTemplate::scheduled(
                Metric::new("unit_rate").unwrap(),
                unit,
                SignedDuration::from_hours(1),
            )
            .versioned(),
        );
        if DECLARED {
            d.with_source_currency(USD)
        } else {
            d
        }
    }

    fn new(_: &PricesConfig) -> Result<Self, AdapterError> {
        Ok(Self)
    }

    async fn run(self, ctx: AdapterContext<PricesConfig>) -> Result<(), AdapterError> {
        let now = ctx.now();
        let f = ctx
            .forecast(
                DeviceId::new("tariff").unwrap(),
                Metric::new("unit_rate").unwrap(),
                0.25,
                ctx.config().unit,
                Basis::Published,
            )
            .with_issued_at(now + ctx.config().issued_ahead)
            .valid_between(now, now + ctx.config().ahead)
            .with_changed_at(1);
        ctx.emit(f).await?;
        ctx.health().ok().await?;
        ctx.shutdown().wait().await;
        Ok(())
    }
}

/// Run one instance for a minute of paused time; return what reached the
/// sink and the instance's final stats.
async fn run<A: Adapter>(
    kind: &str,
    horizon: &str,
    ahead: &str,
    unit: &str,
    extra: &str,
) -> (MemorySink, heimr::core::AdapterStats) {
    let dir = TempDir::new();
    let toml = format!(
        "[site]\ntime_zone = \"America/New_York\"\ncurrency = \"USD\"\n\
         [heimr]\nstate_dir = \"{}\"\nforecast_horizon = \"{horizon}\"\n\
         [sinks]\nlogging = false\n\
         [adapters.p]\ntype = \"{kind}\"\nahead = \"{ahead}\"\nunit = \"{unit}\"\n{extra}",
        dir.0.display()
    );
    let sink = MemorySink::new(100);
    let app = App::builder(Config::from_toml_str(&toml).unwrap())
        .adapter::<A>()
        .sink(sink.clone())
        .clock(Arc::new(TokioClock::starting_at(t0())))
        .build()
        .await
        .unwrap();
    let telemetry = app.telemetry();
    let stop = app.shutdown_trigger();
    let running = tokio::spawn(app.run_until(std::future::pending()));
    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    let stats = telemetry.get("p").unwrap();
    stop.trigger();
    running.await.unwrap().unwrap();
    (sink, stats)
}

#[tokio::test(start_paused = true)]
async fn declared_money_within_the_horizon_reaches_the_sinks() {
    let (sink, stats) = run::<Prices<true>>("prices", "384h", "30m", "¤/kWh", "").await;
    let f = sink.forecasts();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].unit, Unit::CurrencyPerKilowattHour);
    assert_eq!(f[0].issued_at, t0());
    assert_eq!(stats.forecasts, 1);
    assert_eq!(stats.emit_errors, 0);
}

#[tokio::test(start_paused = true)]
async fn the_configured_horizon_is_enforced() {
    let (sink, stats) = run::<Prices<true>>("prices", "1h", "2h", "¤/kWh", "").await;
    assert!(sink.forecasts().is_empty());
    assert_eq!(stats.run_state, RunState::Down, "a refused record is fatal");
    let why = stats.last_emit_error.unwrap();
    assert!(why.contains("more than the 1h horizon"), "{why}");
}

#[tokio::test(start_paused = true)]
async fn money_from_an_undeclared_kind_is_refused() {
    let (sink, stats) = run::<Prices<false>>("undeclared", "384h", "30m", "¤/kWh", "").await;
    assert!(sink.forecasts().is_empty());
    assert_eq!(stats.run_state, RunState::Down);
    assert_eq!(
        stats.last_emit_error.as_deref(),
        Some("¤/kWh is a money unit, but the adapter kind declares no source currency")
    );
    // The same kind may emit other units.
    let (sink, _) = run::<Prices<false>>("undeclared", "384h", "30m", "°C", "").await;
    assert_eq!(sink.forecasts().len(), 1);
}

/// The app hands the kind's series templates to the instance's emitter:
/// a unit other than the template's is refused, and the refusal is fatal.
#[tokio::test(start_paused = true)]
async fn the_kind_series_templates_reach_the_emitter() {
    let (sink, stats) = run::<Prices<false>>("undeclared", "384h", "30m", "%", "").await;
    assert!(sink.forecasts().is_empty());
    assert_eq!(stats.run_state, RunState::Down);
    assert_eq!(
        stats.last_emit_error.as_deref(),
        Some("unit_rate on tariff: unit % differs from its series template's °C")
    );
}

#[tokio::test(start_paused = true)]
async fn a_forecast_issued_in_the_future_is_refused() {
    let (sink, stats) =
        run::<Prices<true>>("prices", "384h", "30m", "¤/kWh", "issued_ahead = \"2s\"\n").await;
    assert!(sink.forecasts().is_empty());
    assert_eq!(stats.run_state, RunState::Down);
    let why = stats.last_emit_error.unwrap();
    assert!(why.contains("is after the clock's"), "{why}");
    // Within the allowed skew it passes.
    let (sink, _) =
        run::<Prices<true>>("prices", "384h", "30m", "¤/kWh", "issued_ahead = \"1s\"\n").await;
    assert_eq!(sink.forecasts().len(), 1);
}
