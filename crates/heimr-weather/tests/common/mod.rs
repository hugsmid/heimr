//! Replay cassettes built from the `weather` client's hand-built fixtures
//! (see `crates/weather/tests/fixtures/README.md`), with their time axes
//! moved to the test kit's clock. No test makes a live request.

#![allow(dead_code, clippy::unwrap_used)]

use heimr_testkit::replay::{Cassette, Interaction};
use heimr_testkit::t0;
use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};

pub const FORECAST: &str = include_str!("../../../weather/tests/fixtures/forecast.json");
pub const HISTORICAL: &str =
    include_str!("../../../weather/tests/fixtures/historical_forecast.json");
pub const RATE_LIMITED: &str =
    include_str!("../../../weather/tests/fixtures/error_rate_limited.json");
pub const INVALID_VARIABLE: &str =
    include_str!("../../../weather/tests/fixtures/error_invalid_variable.json");

/// The forecast fixture holds two days.
pub const FORECAST_DAYS: u8 = 2;
pub const HOUR: SignedDuration = SignedDuration::from_hours(1);

/// `fixture` with its `hourly.time` axis moved so the first row is at
/// `first`.
pub fn shifted(fixture: &str, first: Timestamp) -> Value {
    let mut v: Value = serde_json::from_str(fixture).unwrap();
    let times = v["hourly"]["time"].as_array_mut().unwrap();
    let start = times[0].as_i64().unwrap();
    for t in times.iter_mut() {
        *t = json!(t.as_i64().unwrap() - start + first.as_second());
    }
    v
}

/// Forecast rows from 00:00 UTC on the clock's first day.
pub fn forecast() -> Value {
    shifted(FORECAST, t0())
}

/// The 24 hours before the clock starts.
pub fn historical() -> Value {
    shifted(HISTORICAL, t0() - 24 * HOUR)
}

/// The forecast interaction (the only request with `forecast_days`), then
/// the historical forecast (same path) answered with `historical` in
/// order, the last repeating.
pub fn cassette_with(historical: Vec<Value>) -> Cassette {
    let mut c = Cassette::default();
    c.push(
        Interaction::get("/v1/forecast")
            .query("forecast_days", &FORECAST_DAYS.to_string())
            .json(200, forecast()),
    );
    let mut h = Interaction::get("/v1/forecast");
    for body in historical {
        h = h.json(200, body);
    }
    c.push(h);
    c
}

pub fn cassette() -> Cassette {
    cassette_with(vec![historical()])
}

/// Settings pointing both hosts at `uri`, with generic coordinates.
pub fn settings(uri: &str, forecast_interval: &str, observed_interval: &str) -> Value {
    json!({
        "latitude": 0.0,
        "longitude": 0.0,
        "forecast_days": FORECAST_DAYS,
        "forecast_interval": forecast_interval,
        "observed_interval": observed_interval,
        "forecast_url": uri,
        "historical_url": uri,
    })
}

/// Whether two readings are the same fact, bit for bit.
pub fn same_reading(a: &heimr_core::Reading, b: &heimr_core::Reading) -> bool {
    let (mut x, mut y) = (a.clone(), b.clone());
    x.value = 0.0;
    y.value = 0.0;
    a.value.to_bits() == b.value.to_bits() && x == y
}

/// Whether two forecasts are the same fact, bit for bit (issue time and
/// `changed_at` included).
pub fn same_forecast(a: &heimr_core::Forecast, b: &heimr_core::Forecast) -> bool {
    let (mut x, mut y) = (a.clone(), b.clone());
    x.value = 0.0;
    y.value = 0.0;
    a.value.to_bits() == b.value.to_bits() && x == y
}

/// A hand-built fixture's value at `row` of `column`.
pub fn value(fixture: &Value, column: &str, row: usize) -> f64 {
    fixture["hourly"][column][row].as_f64().unwrap()
}

/// Run the adapter directly with a context built here, so a test owns the
/// record channel (its capacity, and when it closes).
pub fn run_direct(
    settings: Value,
    dir: &std::path::Path,
    clock: heimr_core::ManualClock,
    capacity: usize,
) -> (
    tokio::sync::mpsc::Receiver<heimr_core::Envelope>,
    heimr_core::ShutdownTrigger,
    tokio::task::JoinHandle<Result<(), heimr_core::AdapterError>>,
) {
    use heimr_core::{Adapter, AdapterContext, ContextParts, Emitter, Mode, StatsHandle};
    use heimr_weather::OpenMeteo;
    let config: heimr_weather::Config = serde_json::from_value(settings).unwrap();
    let site = heimr_testkit::test_site();
    let adapter = OpenMeteo::build(&config, &site).unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(capacity);
    let emitter =
        Emitter::for_adapter("under-test", tx).with_series(OpenMeteo::descriptor().series);
    let (trigger, shutdown) = heimr_core::shutdown_channel();
    let stats = StatsHandle::new("under-test", "open-meteo", Mode::Shadow);
    let parts = ContextParts::new(
        emitter,
        std::sync::Arc::new(clock),
        shutdown,
        dir.to_owned(),
        stats.for_instance(),
        site,
    );
    let ctx = AdapterContext::new(parts, std::sync::Arc::new(config));
    (rx, trigger, tokio::spawn(adapter.run(ctx)))
}

/// A fresh, empty state directory, removed on drop.
pub struct StateDir(pub std::path::PathBuf);

impl StateDir {
    pub fn new(tag: &str) -> Self {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("heimr-weather-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for StateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
