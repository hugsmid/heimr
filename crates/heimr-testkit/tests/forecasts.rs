//! The kit's own checks for forecasts, money and the site currency.
//!
//! In the app, the core refuses bad forecasts and undeclared money before
//! any sink sees them (`tests/violations.rs` shows `records.no_refusals`
//! catching them there). A driver for adapters in another process may not
//! sit behind the core's emitter, so the scenarios check the records
//! themselves too. These tests run them against canned records through a
//! minimal [`Driver`], asserting each reason.

#![allow(clippy::unwrap_used)]

use heimr_core::{
    Adapter, AdapterContext, AdapterDescriptor, AdapterError, AdapterStats, AuditRecord,
    Capabilities, Command, Currency, DeviceId, Envelope, Metric, Mode, Outcome, Rejection,
    RunState, SeriesTemplate, Unit,
};
use heimr_testkit::{
    Driver, Expectations, Failure, Fault, Harness, PrepareFailure, Report, Status, Traffic,
    catalogue, run_scenarios, t0,
};
use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};

/// Replays fixed records; `check_site` answers as `site_check` says.
struct Canned {
    currency: Option<Currency>,
    records: Vec<Envelope>,
    site_check: fn(Option<Currency>, Currency) -> Result<(), String>,
}

impl Driver for Canned {
    /// Forecast templates for `rate` (plain number), `unit_rate` (¤/kWh)
    /// and `temperature` (°C).
    fn descriptor(&self) -> AdapterDescriptor {
        let hour = SignedDuration::from_hours(1);
        let m = |s: &str| Metric::new(s).unwrap();
        let d = AdapterDescriptor::new("canned", "canned records", Capabilities::read_only())
            .with_series(scheduled(m("rate"), Unit::Dimensionless, hour))
            .with_series(scheduled(
                m("unit_rate"),
                Unit::CurrencyPerKilowattHour,
                hour,
            ))
            .with_series(scheduled(Metric::TEMPERATURE, Unit::Celsius, hour));
        match self.currency {
            Some(c) => d.with_source_currency(c),
            None => d,
        }
    }
    fn instance(&self) -> &str {
        "under-test"
    }
    fn settings(&mut self) -> Value {
        json!({})
    }
    fn prepare(&mut self, _: &Value) -> Result<Capabilities, PrepareFailure> {
        Ok(Capabilities::read_only())
    }
    fn check_site(&mut self, currency: Currency) -> Result<(), String> {
        (self.site_check)(self.currency, currency)
    }
    fn start(&mut self, _: Mode) -> Result<(), String> {
        Ok(())
    }
    fn advance(&mut self, _: SignedDuration) {}
    fn settle(&mut self) {}
    fn now(&self) -> Timestamp {
        t0()
    }
    fn submit(&mut self, _: Command) -> Result<Outcome, Rejection> {
        Err(Rejection::UnknownAdapter {
            adapter: "under-test".into(),
        })
    }
    fn records(&self) -> Vec<Envelope> {
        self.records.clone()
    }
    fn failures(&self) -> Vec<Failure> {
        Vec::new()
    }
    fn stats(&self) -> AdapterStats {
        let mut s = AdapterStats::default();
        s.run_state = RunState::Running;
        s
    }
    fn audit(&self) -> Vec<AuditRecord> {
        Vec::new()
    }
    fn inject(&mut self, _: Option<Fault>) -> bool {
        false
    }
    fn traffic(&self) -> Option<Traffic> {
        None
    }
    fn stop(&mut self) -> AdapterStats {
        self.stats()
    }
}

/// The app's rule: a declared currency must match the site's.
fn app_rule(declared: Option<Currency>, site: Currency) -> Result<(), String> {
    match declared {
        Some(d) if d != site => Err(format!("kind quotes {d}, site is {site}")),
        _ => Ok(()),
    }
}

/// A forecast envelope as a host outside the core might deliver it.
fn forecast(from_mins: i64, to_mins: i64, value: Value, unit: Unit, metric: &str) -> Envelope {
    let at = |m: i64| (t0() + SignedDuration::from_mins(m)).to_string();
    serde_json::from_value(json!({
        "source": "under-test",
        "record": { "type": "forecast", "data": {
            "issued_at": at(0),
            "valid_from": at(from_mins),
            "valid_to": at(to_mins),
            "device": "tariff",
            "metric": metric,
            "value": value,
            "unit": unit,
            "quality": "good",
            "basis": "published",
            "changed_at": 1,
        }}
    }))
    .unwrap()
}

/// JSON has no NaN: build a valid envelope, then break it.
fn not_finite() -> Envelope {
    let mut env = forecast(0, 0, json!(1.0), Unit::Dimensionless, "rate");
    if let heimr_core::Record::Forecast(f) = &mut env.record {
        f.value = f64::NAN;
    }
    env
}

fn run(driver: fn() -> Canned, ids: &[&str]) -> Report {
    let scenarios: Vec<_> = catalogue()
        .into_iter()
        .filter(|s| ids.contains(&s.id()))
        .collect();
    let expect = Expectations::default();
    run_scenarios("canned", &scenarios, &expect, &mut || {
        Ok(Box::new(driver()) as Box<dyn Driver>)
    })
}

fn reason(r: &Report, id: &str) -> String {
    let res = r.get(id).unwrap();
    assert_eq!(res.status, Status::Failed, "{r}");
    res.reason.clone().unwrap()
}

#[test]
fn well_formed_checks_forecast_periods_horizon_and_values() {
    let r = run(
        || Canned {
            currency: None,
            records: vec![
                forecast(60, 90, json!(1.0), Unit::Dimensionless, "rate"),
                forecast(90, 60, json!(1.0), Unit::Dimensionless, "rate"),
                forecast(0, 17 * 24 * 60, json!(1.0), Unit::Dimensionless, "rate"),
                not_finite(),
            ],
            site_check: app_rule,
        },
        &["records.well_formed"],
    );
    let why = reason(&r, "records.well_formed");
    assert!(
        why.contains(
            "forecast tariff/rate for 2030-01-01T01:30:00Z..2030-01-01T01:00:00Z issued at 2030-01-01T00:00:00Z: valid_from is after valid_to"
        ),
        "{why}"
    );
    assert!(
        why.contains("ends more than the 384h forecast horizon after it was issued"),
        "{why}"
    );
    assert!(!why.contains("NaN"), "a NaN is not a problem: {why}");
    assert_eq!(why.matches("; ").count(), 1, "exactly two problems: {why}");
}

/// A versioned series needs `changed_at` (forecast series are versioned);
/// one that is not must not carry it.
#[test]
fn well_formed_checks_changed_at_against_versioning() {
    fn without_changed_at() -> Envelope {
        let mut env = forecast(60, 90, json!(1.0), Unit::Dimensionless, "rate");
        if let heimr_core::Record::Forecast(f) = &mut env.record {
            f.changed_at = None;
        }
        env
    }
    let r = run(
        || Canned {
            currency: None,
            records: vec![
                forecast(60, 90, json!(1.0), Unit::Dimensionless, "rate"),
                without_changed_at(),
            ],
            site_check: app_rule,
        },
        &["records.well_formed"],
    );
    assert_eq!(
        reason(&r, "records.well_formed"),
        "forecast tariff/rate for 2030-01-01T01:00:00Z..2030-01-01T01:30:00Z issued at 2030-01-01T00:00:00Z: the series is versioned, but it carries no changed_at"
    );
}

/// NaN and ±∞ are passed through, so `records.well_formed` notes them
/// without failing.
#[test]
fn well_formed_notes_non_finite_values_without_failing() {
    let r = run(
        || Canned {
            currency: None,
            records: vec![
                forecast(60, 90, json!(1.0), Unit::Dimensionless, "rate"),
                not_finite(),
            ],
            site_check: app_rule,
        },
        &["records.well_formed"],
    );
    let res = r.get("records.well_formed").unwrap();
    assert_eq!(res.status, Status::PassedWithNote, "{r}");
    let why = res.reason.clone().unwrap();
    assert!(
        why.starts_with("non-finite values passed through (the store will record them as bad quality): forecast tariff/rate for "),
        "{why}"
    );
    assert!(why.ends_with(": value NaN"), "{why}");
}

#[test]
fn units_accept_money_only_from_a_declared_currency() {
    fn records() -> Vec<Envelope> {
        vec![
            forecast(
                0,
                30,
                json!(0.2),
                Unit::CurrencyPerKilowattHour,
                "unit_rate",
            ),
            forecast(0, 30, json!(21.0), Unit::Percent, "temperature"),
            forecast(0, 30, json!(3.0), Unit::MetrePerSecond, "wind_speed"),
        ]
    }
    let undeclared = run(
        || Canned {
            currency: None,
            records: records(),
            site_check: app_rule,
        },
        &["records.units"],
    );
    let why = reason(&undeclared, "records.units");
    assert_eq!(
        why,
        "template for scheduled unit_rate: money unit ¤/kWh from a kind that declares no source currency; \
         tariff/unit_rate: money unit ¤/kWh from a kind that declares no source currency; \
         forecast tariff/temperature: unit % differs from its template's °C; \
         forecast tariff/wind_speed: no series template for forecasts of wind_speed"
    );
    let declared = run(
        || Canned {
            currency: Some(Currency::of("EUR")),
            records: records(),
            site_check: app_rule,
        },
        &["records.units"],
    );
    let why = reason(&declared, "records.units");
    assert_eq!(
        why,
        "forecast tariff/temperature: unit % differs from its template's °C; \
         forecast tariff/wind_speed: no series template for forecasts of wind_speed",
        "money passes once declared"
    );
}

/// A modelled forecast against a scheduled template is flagged.
#[test]
fn units_check_the_basis_against_the_role() {
    let mut env = forecast(0, 30, json!(1.0), Unit::Dimensionless, "rate");
    if let heimr_core::Record::Forecast(f) = &mut env.record {
        f.basis = heimr_core::Basis::Modelled;
    }
    let series = [scheduled(
        Metric::new("rate").unwrap(),
        Unit::Dimensionless,
        SignedDuration::from_hours(1),
    )];
    assert_eq!(
        heimr_testkit::checks::units(&[env.clone()], &series, None),
        [
            "forecast tariff/rate: a modelled forecast needs a forecast template, not a scheduled one"
        ]
    );
    let series = [SeriesTemplate::forecast(
        Metric::new("rate").unwrap(),
        Unit::Dimensionless,
        SignedDuration::from_hours(1),
    )];
    assert!(heimr_testkit::checks::units(&[env], &series, None).is_empty());
}

#[test]
fn currency_matches_site_checks_both_directions() {
    let ok = |currency| {
        let r = run(
            if currency {
                || Canned {
                    currency: Some(Currency::of("USD")),
                    records: vec![],
                    site_check: app_rule,
                }
            } else {
                || Canned {
                    currency: None,
                    records: vec![],
                    site_check: app_rule,
                }
            },
            &["config.currency_matches_site"],
        );
        r.get("config.currency_matches_site").unwrap().status
    };
    assert_eq!(ok(true), Status::Passed);
    assert_eq!(ok(false), Status::Passed);
    // A host that ignores the declared currency.
    let lax = run(
        || Canned {
            currency: Some(Currency::of("USD")),
            records: vec![],
            site_check: |_, _| Ok(()),
        },
        &["config.currency_matches_site"],
    );
    assert_eq!(
        reason(&lax, "config.currency_matches_site"),
        "accepted a site quoting XTS, though the kind quotes USD"
    );
    // One that refuses without saying why.
    let mute = run(
        || Canned {
            currency: Some(Currency::of("XTS")),
            records: vec![],
            site_check: |d, s| {
                if Some(s) == d {
                    Ok(())
                } else {
                    Err("no".into())
                }
            },
        },
        &["config.currency_matches_site"],
    );
    assert_eq!(
        reason(&mute, "config.currency_matches_site"),
        "refused a site quoting XXX without naming both currencies: no"
    );
    // A kind without a currency that refuses foreign sites.
    let picky = run(
        || Canned {
            currency: None,
            records: vec![],
            site_check: |_, s| Err(format!("only EUR, not {s}")),
        },
        &["config.currency_matches_site"],
    );
    assert!(
        reason(&picky, "config.currency_matches_site")
            .starts_with("refused a site quoting XTS, though the kind declares no source currency"),
        "{picky}"
    );
}

/// No settings.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NoKeys {}

/// Counts its runs in its state directory and emits the count.
struct Counter;

impl Adapter for Counter {
    type Config = NoKeys;
    fn descriptor() -> AdapterDescriptor {
        AdapterDescriptor::new("counter", "counts its runs", Capabilities::read_only()).with_series(
            SeriesTemplate::counter(
                Metric::new("runs").unwrap(),
                Unit::Dimensionless,
                SignedDuration::from_hours(24),
                None,
            ),
        )
    }
    fn new(_: &NoKeys) -> Result<Self, AdapterError> {
        Ok(Self)
    }
    async fn run(self, ctx: AdapterContext<NoKeys>) -> Result<(), AdapterError> {
        let path = ctx.state_dir().join("runs");
        let runs: u32 = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
            + 1;
        std::fs::write(&path, runs.to_string()).map_err(AdapterError::fatal)?;
        let r = ctx.reading(
            DeviceId::new("runs").unwrap(),
            Metric::new("runs").unwrap(),
            f64::from(runs),
            Unit::Dimensionless,
        );
        ctx.emit(r).await?;
        ctx.shutdown().wait().await;
        Ok(())
    }
}

/// A changed_at on a series that is not versioned is flagged.
#[test]
fn a_stray_changed_at_is_flagged() {
    let series = [SeriesTemplate::gauge(
        Metric::new("runs").unwrap(),
        Unit::Dimensionless,
        SignedDuration::from_hours(1),
    )];
    let env: Envelope = serde_json::from_value(json!({
        "source": "under-test",
        "record": { "type": "reading", "data": {
            "time": t0().to_string(), "source": "under-test", "device": "runs", "metric": "runs",
            "value": 1.0, "unit": "1", "quality": "good", "changed_at": 9,
        }}
    }))
    .unwrap();
    assert_eq!(
        heimr_testkit::checks::versioning(&[env], &series),
        [
            "reading runs/runs at 2030-01-01T00:00:00Z: changed_at 9 on a series that is not versioned"
        ]
    );
}

#[tokio::test]
async fn harness_restart_keeps_state_dir_clock_and_records() {
    let mut h = Harness::start::<Counter>(json!({})).await.unwrap();
    let dir = h.state_dir().to_owned();
    h.advance(SignedDuration::from_mins(1)).await;
    h.restart().await.unwrap();
    assert_eq!(h.state_dir(), dir);
    let values: Vec<_> = h.readings().iter().map(|r| (r.value, r.time)).collect();
    assert_eq!(
        values,
        [(1.0, t0()), (2.0, t0() + SignedDuration::from_mins(1))]
    );
    assert!(h.forecasts().is_empty());
    h.stop().await.unwrap();
}

#[test]
fn forecasts_render_with_period_and_basis() {
    let env = forecast(
        60,
        90,
        json!(0.184),
        Unit::CurrencyPerKilowattHour,
        "unit_rate",
    );
    assert_eq!(
        heimr_testkit::render::Render::new()
            .relative_to(t0())
            .render(&[env]),
        "+0s forecast under-test tariff unit_rate 0.184 ¤/kWh for +1h..+1h 30m published\n"
    );
}

/// A versioned scheduled template (published, correctable values).
fn scheduled(metric: Metric, unit: Unit, interval: SignedDuration) -> SeriesTemplate {
    SeriesTemplate::scheduled(metric, unit, interval).versioned()
}

/// The kit's own interval check, for drivers outside the core: a forecast
/// of an interval series must start an interval.
#[test]
fn intervals_are_checked_against_the_template() {
    let mut series = SeriesTemplate::forecast(
        Metric::new("rate").unwrap(),
        Unit::Dimensionless,
        SignedDuration::from_hours(1),
    );
    series.kind = heimr_core::SeriesKind::Interval {
        length: SignedDuration::from_hours(1),
    };
    let ok = forecast(60, 120, json!(1.0), Unit::Dimensionless, "rate");
    let off = forecast(90, 150, json!(1.0), Unit::Dimensionless, "rate");
    assert_eq!(
        heimr_testkit::checks::intervals(&[ok, off], std::slice::from_ref(&series)),
        [
            "forecast tariff/rate for 2030-01-01T01:30:00Z..2030-01-01T02:30:00Z issued at 2030-01-01T00:00:00Z: 2030-01-01T01:30:00Z does not start an interval of 1h"
        ]
    );
}

/// A forecast re-sent with another period (`valid_to`) is not a repeat.
#[test]
fn a_resent_forecast_keeps_its_period() {
    let series = [scheduled(
        Metric::new("rate").unwrap(),
        Unit::Dimensionless,
        SignedDuration::from_hours(1),
    )];
    let first = vec![forecast(60, 90, json!(1.0), Unit::Dimensionless, "rate")];
    let same = vec![forecast(60, 90, json!(1.0), Unit::Dimensionless, "rate")];
    let longer = vec![forecast(60, 120, json!(1.0), Unit::Dimensionless, "rate")];
    assert!(heimr_testkit::checks::monotonic(&[first.clone(), same], &series).is_empty());
    assert_eq!(
        heimr_testkit::checks::monotonic(&[first, longer], &series),
        [
            "forecast tariff/rate at 2030-01-01T01:00:00Z: re-sent with another period (valid to 2030-01-01T02:00:00Z, was 2030-01-01T01:30:00Z)"
        ]
    );
}
