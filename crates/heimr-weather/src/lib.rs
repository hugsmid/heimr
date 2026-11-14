//! `open-meteo`: a read-only [heimr](https://github.com/hugsmid/heimr)
//! adapter for the [Open-Meteo](https://open-meteo.com) weather APIs,
//! built on the [`weather`] client.
//!
//! Every `forecast_interval` it fetches the hourly forecast and emits each
//! hour as a [`Forecast`](heimr_core::Forecast) with
//! [`Basis::Modelled`]; every `observed_interval` it fetches the recent
//! hours from the historical-forecast API (archived short-range forecasts,
//! which cover the days the reanalysis archive has not reached yet) and
//! emits each hour as a [`Reading`](heimr_core::Reading) with
//! [`Quality::Estimated`]. All records are for one device, `outdoor`:
//!
//! | Metric | Unit | Period |
//! |---|---|---|
//! | `temperature` | °C | the hour's instant |
//! | `humidity` (relative) | % | the hour's instant |
//! | `wind_speed` (10 m) | m/s | the hour's instant |
//! | `wind_direction` (10 m, where it blows from) | ° | the hour's instant |
//! | `cloud_cover` | % | the hour's instant |
//! | `pressure` (surface) | hPa | the hour's instant |
//! | `precipitation` | mm | sum over the hour *before* the row's time |
//! | `shortwave_radiation` | W/m² | mean over the hour *before* the row's time |
//!
//! Each metric has two series templates, both versioned: forecasts (keyed
//! by valid time, re-issued every `forecast_interval`) and recent-hour
//! readings (stamped by the source, fetched every `observed_interval`;
//! versioned because reanalysis later replaces the stitched forecast for
//! an hour). The templates state the default intervals. Precipitation is
//! an interval series of one hour (an amount, stamped at the hour's
//! start); radiation, a mean over the hour, and the instants are gauges.
//!
//! A forecast of an instant has `valid_from == valid_to`; precipitation and
//! radiation cover an interval, so their forecasts are valid for that hour
//! and their readings are stamped at the start of the hour, as heimr
//! stamps every interval reading. A row whose interval cannot be computed
//! (a time at the edge of the representable range) is skipped with a
//! warning. A value missing from a row is not sent; it is sent when a
//! later fetch has it.
//!
//! Forecast hours already past at the fetch, and hours beyond the app's
//! forecast horizon, are dropped. Recent hours are sent once they have
//! passed, back to [`LOOKBACK`] (48 hours) before the fetch.
//!
//! # Re-sending and revisions
//! Open-Meteo reports no model run time or revision, so the adapter keeps
//! one per series and hour ([`Revisions`]): the
//! last value seen and its `changed_at`, the time that value was first
//! seen (in nanoseconds, strictly increasing per hour). A forecast's
//! `issued_at` is that time too, so an unchanged forecast hour is not
//! re-issued by every fetch.
//!
//! - **On every start** the first good fetch of each endpoint re-sends its
//!   whole window: the forecast horizon, and the lookback. Repeats are
//!   identical (same time, value bits, quality and `changed_at`, and for a
//!   forecast the same `issued_at`), so the store will drop them.
//! - **After a failed fetch** the next good fetch of each endpoint re-sends
//!   its window too.
//! - **Later fetches** send only what is new or changed. A changed hour
//!   (a new model run, or reanalysis replacing a recent hour) is sent
//!   again with a higher `changed_at`.
//! - **After the clock steps back**, a forecast hour first seen at a time
//!   now in the future is held back until the clock passes it; a value that
//!   changes meanwhile still gets a higher `changed_at`. The request for
//!   recent hours never starts after now.
//!
//! The map is bounded to the two windows and kept in the instance's state
//! directory (`revisions.json`), replaced durably (written, synced,
//! renamed) after each fetch's values are seen and before they are sent.
//! If it is empty or unreadable, the adapter logs an error naming it,
//! reports health `degraded` until the next good round, and starts a new
//! one: values seen again then get new `changed_at` values that look newer
//! than they are (the same values, re-issued; nothing is reverted).
//!
//! <!-- ANCHOR: toml -->
//! ```toml
//! [adapters.weather]
//! type = "open-meteo"
//! # Generic placeholder coordinates, not a real site: set the site's own,
//! # in its private configuration only.
//! latitude = 0.0                # -90 ..= 90
//! longitude = 0.0               # -180 ..= 180
//! # forecast_interval = "1h"    # 5m ..= 24h
//! # observed_interval = "3h"    # 15m ..= 24h
//! # forecast_days = 7           # 1 ..= 16
//! # models = "best_match"       # Open-Meteo model name(s); default: its choice
//! # forecast_url = "https://api.open-meteo.com"
//! # historical_url = "https://historical-forecast-api.open-meteo.com"
//! ```
//! <!-- ANCHOR_END: toml -->
//!
//! # Failures
//! A failed request is retried after 30 s, doubling up to the forecast
//! interval, with health `degraded` meanwhile; a rate limit (HTTP 429) is
//! treated the same way, waiting at least as long as the service asks.
//! Malformed responses are failures like any other. HTTP 401 or 403, or a
//! request Open-Meteo rejects as invalid (HTTP 400, e.g. an unknown model),
//! is fatal: the instance stays down.
//!
//! # Terms
//! Open-Meteo data is licensed under CC BY 4.0: credit "Weather data by
//! Open-Meteo.com" when you show or share it. The free API is for
//! non-commercial use. Not affiliated with Open-Meteo.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::Path;

use heimr_core::{
    Adapter, AdapterContext, AdapterDescriptor, AdapterError, Basis, Capabilities, DeviceId,
    MAX_FUTURE_SKEW, Metric, Quality, Revisions, SeriesKind, SeriesTemplate, TimeSource, Unit,
};
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;
use weather::client::MAX_FORECAST_DAYS;
use weather::{Client, ForecastOptions, ForecastPoint, IssuedAt, Location, Observation};

/// Instance settings; see the [crate docs](crate) for an example.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Config {
    /// Latitude of the site, degrees north (required).
    #[serde(default)]
    pub latitude: Option<f64>,
    /// Longitude of the site, degrees east (required).
    #[serde(default)]
    pub longitude: Option<f64>,
    /// Time between forecast fetches (5 min ..= 24 h; default 1 h).
    #[serde(default = "default_forecast_interval")]
    pub forecast_interval: SignedDuration,
    /// Time between fetches of the recent hours (15 min ..= 24 h; default
    /// 3 h).
    #[serde(default = "default_observed_interval")]
    pub observed_interval: SignedDuration,
    /// Days of forecast to fetch, from 00:00 UTC today (1 ..= 16; default
    /// 7).
    #[serde(default = "default_forecast_days")]
    pub forecast_days: u8,
    /// Open-Meteo weather model(s), comma-separated; default: Open-Meteo's
    /// choice.
    #[serde(default)]
    pub models: Option<String>,
    /// Forecast API host (default `https://api.open-meteo.com`).
    #[serde(default)]
    pub forecast_url: Option<String>,
    /// Historical-forecast API host (default
    /// `https://historical-forecast-api.open-meteo.com`).
    #[serde(default)]
    pub historical_url: Option<String>,
}

fn default_forecast_interval() -> SignedDuration {
    SignedDuration::from_hours(1)
}
fn default_observed_interval() -> SignedDuration {
    SignedDuration::from_hours(3)
}
fn default_forecast_days() -> u8 {
    7
}

/// Shortest forecast interval.
pub const MIN_FORECAST_INTERVAL: SignedDuration = SignedDuration::from_mins(5);
/// Shortest interval between fetches of the recent hours.
pub const MIN_OBSERVED_INTERVAL: SignedDuration = SignedDuration::from_mins(15);
/// Longest interval for either.
pub const MAX_INTERVAL: SignedDuration = SignedDuration::from_hours(24);
/// The first retry after a failure; doubles up to the forecast interval.
const RETRY_INITIAL: SignedDuration = SignedDuration::from_secs(30);
/// The longest wait a rate limit may impose.
const RETRY_AFTER_MAX: SignedDuration = SignedDuration::from_hours(1);
/// How far back every fetch of recent hours reaches: the window re-sent on
/// start-up and watched for revisions.
pub const LOOKBACK: SignedDuration = SignedDuration::from_hours(48);
/// The file in the state directory holding the revisions map.
const STATE_FILE: &str = "revisions.json";
/// The cursor file of earlier versions, removed once the map is saved.
const LEGACY_CURSOR: &str = "cursor.json";

/// The `open-meteo` adapter. See the [crate docs](crate).
#[derive(Debug)]
pub struct OpenMeteo {
    location: Location,
    forecast_interval: SignedDuration,
    observed_interval: SignedDuration,
}

fn check_interval(
    key: &str,
    v: SignedDuration,
    min: SignedDuration,
    min_text: &str,
) -> Result<(), AdapterError> {
    if v < min || v > MAX_INTERVAL {
        Err(AdapterError::config_key(
            key,
            format!("must be between {min_text} and 24h"),
        ))
    } else {
        Ok(())
    }
}

fn check_url(key: &str, url: Option<&str>) -> Result<(), AdapterError> {
    let Some(u) = url else { return Ok(()) };
    match url::Url::parse(u) {
        Ok(p) if matches!(p.scheme(), "http" | "https") && p.has_host() => Ok(()),
        _ => Err(AdapterError::config_key(key, "must be an http(s) URL")),
    }
}

impl Adapter for OpenMeteo {
    type Config = Config;

    fn descriptor() -> AdapterDescriptor {
        let d = AdapterDescriptor::new(
            "open-meteo",
            "hourly weather forecasts and recent hours from Open-Meteo",
            Capabilities::read_only(),
        );
        SPEC.iter().fold(d, |d, &(name, unit, _)| {
            let metric = Metric::new(name).unwrap_or(Metric::TEMPERATURE);
            // Precipitation is an amount per hour: an interval series.
            // Radiation over the hour is a mean, not an amount: a gauge.
            let kind = if name == "precipitation" {
                SeriesKind::Interval { length: HOUR }
            } else {
                SeriesKind::Gauge
            };
            let reading =
                SeriesTemplate::new(metric.clone(), kind, unit, default_observed_interval())
                    .with_t_source(TimeSource::Device)
                    // Reanalysis replaces the stitched forecast for an hour.
                    .versioned();
            let mut forecast = SeriesTemplate::forecast(metric, unit, default_forecast_interval());
            forecast.kind = kind;
            d.with_series(reading).with_series(forecast)
        })
    }

    fn new(c: &Config) -> Result<Self, AdapterError> {
        let required = |key: &str| {
            AdapterError::config_key(key, "required: the site's coordinates, in degrees")
        };
        let latitude = c.latitude.ok_or_else(|| required("latitude"))?;
        let longitude = c.longitude.ok_or_else(|| required("longitude"))?;
        if !(-90.0..=90.0).contains(&latitude) {
            return Err(AdapterError::config_key(
                "latitude",
                "must be between -90 and 90",
            ));
        }
        if !(-180.0..=180.0).contains(&longitude) {
            return Err(AdapterError::config_key(
                "longitude",
                "must be between -180 and 180",
            ));
        }
        let location = Location::new(latitude, longitude)
            .map_err(|e| AdapterError::config_key("latitude", e.to_string()))?;
        check_interval(
            "forecast_interval",
            c.forecast_interval,
            MIN_FORECAST_INTERVAL,
            "5m",
        )?;
        check_interval(
            "observed_interval",
            c.observed_interval,
            MIN_OBSERVED_INTERVAL,
            "15m",
        )?;
        if !(1..=MAX_FORECAST_DAYS).contains(&c.forecast_days) {
            return Err(AdapterError::config_key(
                "forecast_days",
                format!("must be between 1 and {MAX_FORECAST_DAYS}"),
            ));
        }
        if let Some(m) = &c.models
            && (m.trim().is_empty() || m.len() > 200 || m.chars().any(char::is_control))
        {
            return Err(AdapterError::config_key("models", "invalid model name"));
        }
        check_url("forecast_url", c.forecast_url.as_deref())?;
        check_url("historical_url", c.historical_url.as_deref())?;
        Ok(Self {
            location,
            forecast_interval: c.forecast_interval,
            observed_interval: c.observed_interval,
        })
    }

    /// The kind's templates, expected every configured interval:
    /// forecasts every `forecast_interval`, recent hours every
    /// `observed_interval` but never more often than hourly. The newest
    /// recent hour is up to about two hours old when fetched, so a shorter
    /// poll must not make the readings look stale between hours (the
    /// staleness limit is three expected intervals).
    fn series(&self) -> Vec<SeriesTemplate> {
        Self::descriptor()
            .series
            .into_iter()
            .map(|t| {
                let every = if t.for_forecasts() {
                    self.forecast_interval
                } else {
                    self.observed_interval.max(HOUR)
                };
                t.with_expected_interval(every)
            })
            .collect()
    }

    async fn run(self, ctx: AdapterContext<Config>) -> Result<(), AdapterError> {
        let c = ctx.config().clone();
        let mut builder = Client::builder();
        if let Some(u) = &c.forecast_url {
            builder = builder.forecast_base_url(u);
        }
        if let Some(u) = &c.historical_url {
            builder = builder.historical_forecast_base_url(u);
        }
        let client = builder.build().map_err(AdapterError::fatal)?;
        let mut options = ForecastOptions::days(c.forecast_days);
        options.models = c.models.clone();
        let device = DeviceId::new("outdoor").map_err(AdapterError::fatal)?;
        // An unreadable map is reported and rebuilt: values seen again get
        // new, higher revision markers.
        let (revisions, mut problem) = match Revisions::load(ctx.state_dir(), STATE_FILE) {
            Ok(r) => (r, None),
            Err(why) => {
                tracing::error!(error = %why, "starting a new revisions map: re-sent values will look newer");
                (Revisions::new(), Some(format!("{why}; rebuilt")))
            }
        };
        if let Some(why) = &problem {
            ctx.health().degraded(why.clone()).await?;
        }
        let mut poller = Poller {
            client,
            location: self.location,
            options,
            device,
            saved: revisions.clone(),
            revisions,
            resend_forecast: true,
            resend_observed: true,
        };
        let mut next_forecast = ctx.now();
        let mut next_observed = ctx.now();
        let mut backoff = RETRY_INITIAL;
        loop {
            let next = next_forecast.min(next_observed);
            tokio::select! {
                () = ctx.shutdown().wait() => return Ok(()),
                () = ctx.sleep_until(next) => {}
            }
            let now = ctx.now();
            let mut failure = None;
            if now >= next_forecast {
                let Some(fetched) = poller.until_shutdown(&ctx, Endpoint::Forecast, now).await
                else {
                    return Ok(());
                };
                match fetched {
                    Ok(()) => next_forecast = after(now, c.forecast_interval),
                    Err(e) => failure = Some(("forecast", e)),
                }
            }
            // After a failed forecast, the other endpoint waits too.
            if failure.is_none() && now >= next_observed {
                let Some(fetched) = poller.until_shutdown(&ctx, Endpoint::Observed, now).await
                else {
                    return Ok(());
                };
                match fetched {
                    Ok(()) => next_observed = after(now, c.observed_interval),
                    Err(e) => failure = Some(("recent hours", e)),
                }
            }
            match failure {
                None => {
                    backoff = RETRY_INITIAL;
                    // The first good round after an unreadable map still
                    // says so; the next one reports ok.
                    match problem.take() {
                        Some(why) => ctx.health().degraded(why).await?,
                        None => ctx.health().ok().await?,
                    }
                }
                Some((_, Failure::Emit(e))) => return Err(AdapterError::Emit(e)),
                Some((what, Failure::Fatal(e))) => {
                    ctx.health()
                        .failed(format!("{what} request refused: {e}"))
                        .await?;
                    return Err(AdapterError::fatal(format!("{what} request refused: {e}")));
                }
                Some((what, Failure::Transient { error, wait })) => {
                    let delay = backoff.max(wait.min(RETRY_AFTER_MAX));
                    tracing::warn!(error = %error, retry_in = %format!("{delay:#}"), "{what} request failed");
                    ctx.health()
                        .degraded(format!("{what} request failed: {error}"))
                        .await?;
                    // After an error, re-send both windows: what was sent
                    // before it may not have arrived.
                    poller.resend_forecast = true;
                    poller.resend_observed = true;
                    let retry = after(now, delay);
                    if now >= next_forecast {
                        next_forecast = retry;
                    }
                    if now >= next_observed {
                        next_observed = retry;
                    }
                    backoff = (backoff * 2).min(c.forecast_interval);
                }
            }
        }
    }
}

fn after(t: Timestamp, d: SignedDuration) -> Timestamp {
    t.saturating_add(d).unwrap_or(Timestamp::MAX)
}

/// Why a fetch did not complete.
enum Failure {
    /// Retry later (transport, 5xx, malformed responses, rate limits).
    Transient {
        error: String,
        /// At least this long, if the service said so.
        wait: SignedDuration,
    },
    /// A restart cannot help (refused credentials, an invalid request).
    Fatal(String),
    /// The core refused a record, or the app is closing.
    Emit(heimr_core::EmitError),
}

impl From<heimr_core::EmitError> for Failure {
    fn from(e: heimr_core::EmitError) -> Self {
        Self::Emit(e)
    }
}

fn classify(error: weather::Error) -> Failure {
    use weather::Error as E;
    match &error {
        E::RateLimited { retry_after, .. } => {
            let wait = retry_after.unwrap_or(SignedDuration::ZERO);
            Failure::Transient {
                error: error.to_string(),
                wait,
            }
        }
        E::Status {
            status: 401 | 403, ..
        }
        | E::Url(_) => Failure::Fatal(error.to_string()),
        E::Api { status, .. } if (400..500).contains(status) && *status != 429 => {
            Failure::Fatal(error.to_string())
        }
        // Coordinates and lengths are checked in `new`; what is left is a
        // date range, which a clock step can upset: retry.
        _ => Failure::Transient {
            error: error.to_string(),
            wait: SignedDuration::ZERO,
        },
    }
}

/// One metric of a row, with its period.
struct Value {
    metric: &'static str,
    unit: Unit,
    value: Option<f64>,
    /// A sum or mean over the hour before the row's time (else an
    /// instant).
    interval: bool,
}

/// The metrics of one row, from the client's columns in this order:
/// temperature, humidity, wind speed, wind direction, cloud cover,
/// pressure, precipitation, shortwave radiation.
/// Each metric: name, unit, and whether it covers the hour before the
/// row's time (else an instant).
const SPEC: [(&str, Unit, bool); METRICS] = [
    ("temperature", Unit::Celsius, false),
    ("humidity", Unit::Percent, false),
    ("wind_speed", Unit::MetrePerSecond, false),
    ("wind_direction", Unit::Degree, false),
    ("cloud_cover", Unit::Percent, false),
    ("pressure", Unit::Hectopascal, false),
    ("precipitation", Unit::Millimetre, true),
    ("shortwave_radiation", Unit::WattPerSquareMetre, true),
];

fn values(v: [Option<f64>; METRICS]) -> [Value; METRICS] {
    let mut i = 0;
    v.map(|value| {
        let (metric, unit, interval) = SPEC[i];
        i += 1;
        Value {
            metric,
            unit,
            // Passed through as they come: the store will record a
            // non-finite value as bad quality.
            value,
            interval,
        }
    })
}

fn forecast_values(p: &ForecastPoint) -> [Value; METRICS] {
    values([
        p.temperature_c,
        p.humidity_pct,
        p.wind_speed_ms,
        p.wind_dir_deg,
        p.cloud_cover_pct,
        p.pressure_hpa,
        p.precip_mm,
        p.shortwave_wm2,
    ])
}

fn observed_values(o: &Observation) -> [Value; METRICS] {
    values([
        o.temperature_c,
        o.humidity_pct,
        o.wind_speed_ms,
        o.wind_dir_deg,
        o.cloud_cover_pct,
        o.pressure_hpa,
        o.precip_mm,
        o.shortwave_wm2,
    ])
}

const HOUR: SignedDuration = SignedDuration::from_hours(1);
/// How many metrics a row has.
const METRICS: usize = 8;

/// Which API a fetch goes to.
#[derive(Debug, Clone, Copy)]
enum Endpoint {
    Forecast,
    Observed,
}

/// Fetches and emits; keeps the revisions map.
struct Poller {
    client: Client,
    location: Location,
    options: ForecastOptions,
    device: DeviceId,
    /// Per series and hour, the last value seen and its revision.
    revisions: Revisions,
    /// As last written to the state directory.
    saved: Revisions,
    /// Whether the next forecast fetch re-sends its whole window.
    resend_forecast: bool,
    /// Whether the next fetch of recent hours re-sends its whole window.
    resend_observed: bool,
}

/// The map key of a series: forecasts and readings of a metric apart.
fn key(endpoint: Endpoint, metric: &str) -> String {
    match endpoint {
        Endpoint::Forecast => format!("forecast/{metric}"),
        Endpoint::Observed => format!("reading/{metric}"),
    }
}

impl Poller {
    /// One fetch, unless shutdown comes first (`None`).
    async fn until_shutdown(
        &mut self,
        ctx: &AdapterContext<Config>,
        endpoint: Endpoint,
        now: Timestamp,
    ) -> Option<Result<(), Failure>> {
        let fetch = async {
            match endpoint {
                Endpoint::Forecast => self.forecast(ctx, now).await,
                Endpoint::Observed => self.observed(ctx, now).await,
            }
        };
        tokio::pin!(fetch);
        tokio::select! {
            () = ctx.shutdown().wait() => None,
            r = &mut fetch => Some(r),
        }
    }

    /// Write the map to the state directory if it changed. A failed write
    /// fails the fetch: nothing it minted may be sent before it is saved,
    /// or a restart on the old map could reuse a marker the store already
    /// ranks below what was sent. The next fetch saves and re-sends.
    fn persist(&mut self, dir: &Path) -> Result<(), Failure> {
        if self.revisions == self.saved {
            remove_legacy_cursor(dir);
            return Ok(());
        }
        match self.revisions.save(dir, STATE_FILE) {
            Ok(()) => {
                self.saved = self.revisions.clone();
                remove_legacy_cursor(dir);
                Ok(())
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot save the revisions map; nothing sent");
                Err(Failure::Transient {
                    error: format!("cannot save the revisions map: {e}"),
                    wait: SignedDuration::ZERO,
                })
            }
        }
    }

    async fn forecast(
        &mut self,
        ctx: &AdapterContext<Config>,
        now: Timestamp,
    ) -> Result<(), Failure> {
        let points = self
            .client
            .forecast(self.location, &self.options, IssuedAt::At(now))
            .await
            .map_err(classify)?;
        let horizon = ctx.forecast_horizon();
        let mut due = Vec::new();
        let mut held = false;
        for p in &points {
            for v in forecast_values(p) {
                let Some(value) = v.value else {
                    continue;
                };
                let (from, to) = if v.interval {
                    match p.valid_at.checked_sub(HOUR) {
                        Ok(from) => (from, p.valid_at),
                        Err(_) => {
                            tracing::warn!(valid_at = %p.valid_at, metric = v.metric, "skipping a forecast hour out of range");
                            continue;
                        }
                    }
                } else {
                    (p.valid_at, p.valid_at)
                };
                // Past hours, and hours beyond the horizon, are not sent.
                if to < now || to.duration_since(now) > horizon {
                    continue;
                }
                let rev =
                    self.revisions
                        .observe(&key(Endpoint::Forecast, v.metric), from, value, now);
                if !(self.resend_forecast || rev.minted) {
                    continue;
                }
                // After the clock steps back, wait until it passes the
                // first sighting again (the core refuses a future issue).
                if rev.first_seen.duration_since(now) > MAX_FUTURE_SKEW {
                    held = true;
                    continue;
                }
                if to.duration_since(rev.first_seen) > horizon {
                    continue;
                }
                due.push(
                    ctx.forecast(
                        self.device.clone(),
                        metric(v.metric)?,
                        value,
                        v.unit,
                        Basis::Modelled,
                    )
                    .with_issued_at(rev.first_seen)
                    .valid_between(from, to)
                    .with_changed_at(rev.changed_at),
                );
            }
        }
        // Keep the forecast hours still ahead, and the readings' window.
        let earliest = now.saturating_sub(HOUR).unwrap_or(now);
        self.prune(Endpoint::Forecast, earliest);
        self.persist(ctx.state_dir())?;
        for f in due {
            ctx.emit(f).await?;
        }
        self.resend_forecast = held;
        Ok(())
    }

    async fn observed(
        &mut self,
        ctx: &AdapterContext<Config>,
        now: Timestamp,
    ) -> Result<(), Failure> {
        let earliest = now.saturating_sub(LOOKBACK).unwrap_or(now);
        let utc = jiff::tz::TimeZone::UTC;
        let start = earliest.to_zoned(utc.clone()).date();
        let end = now.to_zoned(utc).date();
        let rows = self
            .client
            .historical_forecast(self.location, start, end)
            .await
            .map_err(classify)?;
        let mut due = Vec::new();
        for o in &rows {
            // Only hours that have passed, within the lookback.
            if o.time > now || o.time < earliest {
                continue;
            }
            for v in observed_values(o) {
                let Some(value) = v.value else {
                    continue;
                };
                // An interval reading is stamped at the interval's start.
                let time = if v.interval {
                    match o.time.checked_sub(HOUR) {
                        Ok(t) => t,
                        Err(_) => {
                            tracing::warn!(time = %o.time, metric = v.metric, "skipping a recent hour out of range");
                            continue;
                        }
                    }
                } else {
                    o.time
                };
                let rev =
                    self.revisions
                        .observe(&key(Endpoint::Observed, v.metric), time, value, now);
                if !(self.resend_observed || rev.minted) {
                    continue;
                }
                due.push(
                    ctx.reading(self.device.clone(), metric(v.metric)?, value, v.unit)
                        .with_time(time)
                        .with_quality(Quality::Estimated)
                        .with_changed_at(rev.changed_at),
                );
            }
        }
        // Interval readings are stamped an hour before their row.
        let keep_from = earliest.saturating_sub(HOUR).unwrap_or(earliest);
        self.prune(Endpoint::Observed, keep_from);
        self.persist(ctx.state_dir())?;
        for r in due {
            ctx.emit(r).await?;
        }
        self.resend_observed = false;
        Ok(())
    }

    /// Forget one endpoint's entries before `earliest`, keeping the
    /// other's.
    fn prune(&mut self, endpoint: Endpoint, earliest: Timestamp) {
        let prefix = key(endpoint, "");
        self.revisions
            .retain(|k, t| !k.starts_with(&prefix) || t >= earliest);
    }
}

fn metric(name: &str) -> Result<Metric, Failure> {
    Metric::new(name).map_err(|e| Failure::Fatal(e.to_string()))
}

/// Remove the cursor file of earlier versions, if it is still there: the
/// revisions map, saved, replaces it.
fn remove_legacy_cursor(dir: &Path) {
    match std::fs::remove_file(dir.join(LEGACY_CURSOR)) {
        Ok(()) => tracing::info!("removed the old cursor file"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(error = %e, "cannot remove the old cursor file"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(v: serde_json::Value) -> Result<Config, serde_json::Error> {
        serde_json::from_value(v)
    }

    #[test]
    fn defaults_and_limits() {
        let c = config(serde_json::json!({ "latitude": 0.0, "longitude": 0.0 })).unwrap();
        assert_eq!(c.forecast_interval, SignedDuration::from_hours(1));
        assert_eq!(c.observed_interval, SignedDuration::from_hours(3));
        assert_eq!(c.forecast_days, 7);
        assert!(OpenMeteo::new(&c).is_ok());
        let bad = |patch: serde_json::Value| {
            let mut v = serde_json::json!({ "latitude": 0.0, "longitude": 0.0 });
            for (k, x) in patch.as_object().unwrap() {
                v[k] = x.clone();
            }
            match OpenMeteo::new(&config(v).unwrap()) {
                Err(AdapterError::Config { key, .. }) => key,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(
            bad(serde_json::json!({ "latitude": 90.5 })).as_deref(),
            Some("latitude")
        );
        assert_eq!(
            bad(serde_json::json!({ "longitude": -181 })).as_deref(),
            Some("longitude")
        );
        assert_eq!(
            bad(serde_json::json!({ "forecast_interval": "4m" })).as_deref(),
            Some("forecast_interval")
        );
        assert_eq!(
            bad(serde_json::json!({ "observed_interval": "25h" })).as_deref(),
            Some("observed_interval")
        );
        assert_eq!(
            bad(serde_json::json!({ "forecast_days": 0 })).as_deref(),
            Some("forecast_days")
        );
        assert_eq!(
            bad(serde_json::json!({ "forecast_url": "ftp://x" })).as_deref(),
            Some("forecast_url")
        );
        assert_eq!(
            bad(serde_json::json!({ "models": " " })).as_deref(),
            Some("models")
        );
    }

    #[test]
    fn instance_series_follow_the_configured_intervals() {
        let c = config(serde_json::json!({
            "latitude": 0.0, "longitude": 0.0,
            "forecast_interval": "30m", "observed_interval": "24h",
        }))
        .unwrap();
        let s = OpenMeteo::new(&c).unwrap().series();
        for t in &s {
            let want = if t.for_forecasts() {
                SignedDuration::from_mins(30)
            } else {
                SignedDuration::from_hours(24)
            };
            assert_eq!(t.expected_interval, want, "{}", t.label());
        }
        OpenMeteo::descriptor().check_instance_series(&s).unwrap();
    }

    /// At a 15-minute poll the readings are still expected hourly, so
    /// the newest hour (up to about two hours old) is never stale.
    #[test]
    fn readings_are_expected_at_most_hourly() {
        let c = config(serde_json::json!({
            "latitude": 0.0, "longitude": 0.0, "observed_interval": "15m",
        }))
        .unwrap();
        for t in OpenMeteo::new(&c)
            .unwrap()
            .series()
            .iter()
            .filter(|t| !t.for_forecasts())
        {
            assert_eq!(t.expected_interval, HOUR, "{}", t.label());
            assert!(t.staleness_limit() >= 2 * HOUR, "{}", t.label());
        }
    }

    #[test]
    fn precipitation_is_an_hourly_interval_series() {
        let d = OpenMeteo::descriptor();
        d.validate().unwrap();
        let rain = Metric::new("precipitation").unwrap();
        for forecast in [false, true] {
            let t = d.series_for(&rain, forecast).unwrap();
            assert_eq!(t.interval_length(), Some(HOUR), "{}", t.label());
        }
        // Everything else is a gauge, radiation (a mean) included.
        let others = d.series.iter().filter(|t| t.metric != rain);
        assert!(
            others
                .into_iter()
                .all(|t| t.kind == heimr_core::SeriesKind::Gauge)
        );
    }

    #[test]
    fn errors_are_classified() {
        let status = |s| weather::Error::Status {
            status: s,
            body: String::new(),
        };
        assert!(matches!(classify(status(503)), Failure::Transient { .. }));
        assert!(matches!(classify(status(401)), Failure::Fatal(_)));
        assert!(matches!(
            classify(weather::Error::Api {
                status: 400,
                reason: "bad model".into()
            }),
            Failure::Fatal(_)
        ));
        assert!(matches!(
            classify(weather::Error::Decode("x".into())),
            Failure::Transient { .. }
        ));
        // A date range a clock step upset is retried, never fatal.
        assert!(matches!(
            classify(weather::Error::InvalidRequest(
                "end date before start".into()
            )),
            Failure::Transient { .. }
        ));
        match classify(weather::Error::RateLimited {
            retry_after: Some(SignedDuration::from_secs(90)),
            reason: None,
        }) {
            Failure::Transient { wait, .. } => assert_eq!(wait, SignedDuration::from_secs(90)),
            _ => panic!("rate limits are transient"),
        }
    }
}
