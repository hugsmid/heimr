//! The suite catches contract violations: a configurable adapter breaks one
//! rule at a time, and the matching scenario must fail while a
//! well-behaved configuration passes everything.

#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use heimr_core::{
    Adapter, AdapterContext, AdapterDescriptor, AdapterError, Basis, Capabilities, ClimateCommand,
    ClimateMode, ClimateState, Command, CommandKind, Currency, DeviceId, Hold, Metric,
    SeriesTemplate, Setpoint, Site, Unit, ZoneId,
};
use heimr_testkit::{Fault, Status, Suite, Traffic, Upstream};
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;
use serde_json::json;

/// A fake device shared between the upstream and the adapter by id. It
/// outlives instance restarts, so an adapter can (wrongly) carry state
/// across them.
#[derive(Debug, Default)]
struct Device {
    fault: Mutex<Option<Fault>>,
    requests: AtomicU64,
    writes: AtomicU64,
    last_reading: Mutex<Option<Timestamp>>,
    /// Instances built for this device so far.
    builds: AtomicU64,
    /// Whether the door reading has been sent once.
    door_sent: std::sync::atomic::AtomicBool,
}

fn devices() -> &'static Mutex<HashMap<u64, Arc<Device>>> {
    static D: OnceLock<Mutex<HashMap<u64, Arc<Device>>>> = OnceLock::new();
    D.get_or_init(Default::default)
}

fn device(id: u64) -> Arc<Device> {
    devices().lock().unwrap().entry(id).or_default().clone()
}

struct FakeUpstream {
    id: u64,
    flags: Vec<&'static str>,
}

impl FakeUpstream {
    fn new(flags: &[&'static str]) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        Self {
            id: N.fetch_add(1, Ordering::Relaxed),
            flags: flags.to_vec(),
        }
    }
}

impl Upstream for FakeUpstream {
    fn settings(&self) -> serde_json::Value {
        json!({ "device": self.id, "flags": self.flags, "interval": "10s" })
    }
    fn inject(&self, fault: Option<Fault>) -> bool {
        *device(self.id).fault.lock().unwrap() = fault;
        true
    }
    fn traffic(&self) -> Option<Traffic> {
        let d = device(self.id);
        Some(Traffic::new(
            d.requests.load(Ordering::Relaxed),
            d.writes.load(Ordering::Relaxed),
        ))
    }
}

/// Unknown keys are collected rather than refused by serde, so that the
/// `accept_unknown` flag can let them through.
#[derive(Debug, Deserialize)]
struct Config {
    device: u64,
    interval: SignedDuration,
    #[serde(default)]
    flags: Vec<String>,
    #[serde(flatten)]
    unknown: BTreeMap<String, serde_json::Value>,
}

/// Breaks rules on request. `PRICED` kinds declare the test site's
/// currency as their source currency; `MISLABELLED` kinds declare series
/// templates against the documented units (temperature in %, `unit_rate`
/// as a plain number).
struct RogueOf<const PRICED: bool, const MISLABELLED: bool = false> {
    flags: Vec<String>,
    /// Which build of its device this instance is (1 for the first).
    build: u64,
}

type Rogue = RogueOf<false>;
type Priced = RogueOf<true>;
type Mislabelled = RogueOf<false, true>;

impl<const PRICED: bool, const MISLABELLED: bool> RogueOf<PRICED, MISLABELLED> {
    fn has(&self, f: &str) -> bool {
        self.flags.iter().any(|x| x == f)
    }
}

impl<const PRICED: bool, const MISLABELLED: bool> Adapter for RogueOf<PRICED, MISLABELLED> {
    type Config = Config;

    fn descriptor() -> AdapterDescriptor {
        const EUR: Currency = Currency::of("EUR");
        let tick = SignedDuration::from_secs(10);
        let m = |s: &str| Metric::new(s).unwrap();
        let temperature = if MISLABELLED {
            Unit::Percent
        } else {
            Unit::Celsius
        };
        let mut d = AdapterDescriptor::new(
            match (PRICED, MISLABELLED) {
                (true, _) => "priced-rogue",
                (false, true) => "mislabelled-rogue",
                (false, false) => "rogue",
            },
            "breaks rules on request",
            Capabilities::read_write([CommandKind::ClimateTarget, CommandKind::ClimateSchedule]),
        )
        .with_series(SeriesTemplate::gauge(
            Metric::TEMPERATURE,
            temperature,
            tick,
        ))
        .with_series(SeriesTemplate::gauge(Metric::HUMIDITY, Unit::Percent, tick))
        // Rain per tick: an amount over each 10 s interval.
        .with_series(
            SeriesTemplate::interval(m("rain"), Unit::Millimetre, tick, tick)
                .with_t_source(heimr_core::TimeSource::Device),
        )
        // A contact that reports on change, kept alive every 30 s.
        .with_series(
            SeriesTemplate::gauge(m("door"), Unit::Dimensionless, tick * 3)
                .with_t_source(heimr_core::TimeSource::Device)
                .record_on_change(),
        )
        .with_series(scheduled(m("rate"), Unit::Dimensionless, tick));
        if PRICED {
            d = d.with_source_currency(EUR).with_series(scheduled(
                m("price"),
                Unit::CurrencyPerKilowattHour,
                tick,
            ));
        }
        if MISLABELLED {
            d = d.with_series(scheduled(m("unit_rate"), Unit::Dimensionless, tick));
        }
        d
    }

    /// `refuse_foreign_currency` checks the site's currency itself instead
    /// of declaring a source currency.
    fn build(config: &Config, site: &Site) -> Result<Self, AdapterError> {
        let rogue = Self::new(config)?;
        if rogue.has("refuse_foreign_currency") && site.currency().as_str() != "EUR" {
            return Err(AdapterError::config("this source quotes EUR"));
        }
        Ok(rogue)
    }

    fn new(config: &Config) -> Result<Self, AdapterError> {
        let rogue = Self {
            flags: config.flags.clone(),
            build: device(config.device).builds.fetch_add(1, Ordering::Relaxed) + 1,
        };
        if config.interval <= SignedDuration::ZERO && !rogue.has("accept_negative_interval") {
            return Err(AdapterError::config_key("interval", "must be positive"));
        }
        if let Some(key) = config.unknown.keys().next()
            && !rogue.has("accept_unknown")
        {
            return Err(AdapterError::config_key(key, "unknown key"));
        }
        if rogue.has("panic_in_new") {
            panic!("bad config handling");
        }
        if rogue.has("panic_on_max_device") && config.device == u64::MAX {
            panic!("device id overflow");
        }
        if rogue.has("io_in_new") {
            device(config.device)
                .requests
                .fetch_add(1, Ordering::Relaxed);
        }
        Ok(rogue)
    }

    /// Climate targets only; `unstable_caps` adds schedules on the second
    /// build of a device, so two builds from the same settings differ
    /// while every build stays write-capable.
    fn capabilities(&self) -> Capabilities {
        if self.has("unstable_caps") && self.build == 2 {
            Capabilities::read_write([CommandKind::ClimateTarget, CommandKind::ClimateSchedule])
        } else {
            Capabilities::read_write([CommandKind::ClimateTarget])
        }
    }

    async fn run(self, mut ctx: AdapterContext<Config>) -> Result<(), AdapterError> {
        let dev = device(ctx.config().device);
        let sensor = DeviceId::new("sensor-1").unwrap();
        let zone = ZoneId::new("zone-a").unwrap();
        let start = ctx.now();
        if self.has("return_early") {
            return Ok(());
        }
        if self.has("write_on_start") {
            dev.writes.fetch_add(1, Ordering::Relaxed);
        }
        if !self.has("no_health") {
            ctx.health().ok().await?;
        }
        if self.has("reemit_last") {
            let last = *dev.last_reading.lock().unwrap();
            if let Some(t) = last {
                let r = ctx
                    .reading(sensor.clone(), Metric::TEMPERATURE, 20.0, Unit::Celsius)
                    .with_time(t);
                ctx.emit(r).await?;
            }
        }
        let mut commands = ctx.take_commands();
        let mut next = ctx.now();
        let mut seen_outage = false;
        loop {
            if self.has("ignore_shutdown") {
                ctx.sleep_until(next).await;
            } else {
                tokio::select! {
                    () = ctx.shutdown().wait() => return Ok(()),
                    Some(_) = commands.recv() => {
                        if self.has("crash_on_command") {
                            return Err(AdapterError::transient("unexpected command"));
                        }
                        if self.has("hang_on_command") {
                            // Blocks, deaf to shutdown.
                            ctx.sleep(SignedDuration::from_hours(1)).await;
                        }
                        if self.has("drop_queued") {
                            // Stops reading commands for a while, then
                            // fails: the queued ones are dropped.
                            tokio::select! {
                                () = ctx.shutdown().wait() => return Ok(()),
                                () = ctx.sleep(SignedDuration::from_secs(10)) => {}
                            }
                            return Err(AdapterError::transient("lost the device"));
                        }
                        dev.writes.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    () = ctx.sleep_until(next) => {}
                }
            }
            next += ctx.config().interval;
            let fault = *dev.fault.lock().unwrap();
            dev.requests.fetch_add(1, Ordering::Relaxed);
            match fault {
                Some(Fault::Unavailable) => {
                    seen_outage = true;
                    if self.has("counter_reset") {
                        dev.requests.store(0, Ordering::Relaxed);
                    }
                    if self.has("restart_on_outage") {
                        return Err(AdapterError::transient("device offline"));
                    }
                    if self.has("hot_retry") {
                        for _ in 0..1000 {
                            dev.requests.fetch_add(1, Ordering::Relaxed);
                            ctx.sleep(SignedDuration::from_millis(10)).await;
                        }
                    }
                    if self.has("fatal_on_outage") {
                        return Err(AdapterError::fatal("gave up"));
                    }
                    if !self.has("ignore_outage") {
                        ctx.health().degraded("device offline").await?;
                    }
                    continue;
                }
                Some(Fault::Rejected) => {
                    if self.has("retry_rejection") {
                        return Err(AdapterError::transient("rejected"));
                    }
                    return Err(AdapterError::fatal("credentials rejected"));
                }
                Some(Fault::Malformed) => {
                    if self.has("panic_on_garbage") {
                        panic!("unexpected payload");
                    }
                    if self.has("garbage_records") {
                        let s = ClimateState::new(zone.clone(), ctx.now(), ClimateMode::Schedule)
                            .with_measured_c(f64::NAN);
                        ctx.emit(s).await?;
                    }
                    ctx.health().degraded("garbage").await?;
                    continue;
                }
                _ => {}
            }
            if !self.has("no_health") {
                ctx.health().ok().await?;
            }
            if self.has("refused_record") {
                // A core-reserved kind: the core refuses and counts it.
                let _ = ctx.emit(ctx.event("adapter_rogue", json!({}))).await;
            }
            if self.has("silent") || (self.has("silent_after_outage") && seen_outage) {
                continue;
            }
            let time = if self.has("duplicate_time") {
                start
            } else {
                ctx.now()
            };
            let unit = if self.has("wrong_unit") {
                Unit::Percent
            } else {
                Unit::Celsius
            };
            // Stamped at the start every tick, with a new value each time.
            let value = if self.has("duplicate_time") {
                ctx.now().duration_since(start).as_secs_f64()
            } else {
                20.0
            };
            let r = ctx
                .reading(sensor.clone(), Metric::TEMPERATURE, value, unit)
                .with_time(time);
            if self.has("wrong_unit") && !MISLABELLED {
                // Refused by the core: ignored here, counted there.
                let _ = ctx.emit(r).await;
            } else {
                ctx.emit(r).await?;
            }
            // Versioning broken both ways: refused by the core.
            if self.has("stray_changed_at") {
                let r = ctx
                    .reading(sensor.clone(), Metric::HUMIDITY, 50.0, Unit::Percent)
                    .with_changed_at(1);
                let _ = ctx.emit(r).await;
            }
            // The door has not moved: re-sent as a keep-alive each tick,
            // or (wrongly) only once.
            let first_tick = !dev.door_sent.swap(true, Ordering::Relaxed);
            if first_tick || !self.has("no_keepalive") {
                let r = ctx.reading(
                    sensor.clone(),
                    Metric::new("door").unwrap(),
                    0.0,
                    Unit::Dimensionless,
                );
                ctx.emit(r).await?;
            }
            // Ticks fall on 10 s boundaries; the misaligned one is 1 ms off.
            if self.has("interval") || self.has("misaligned_interval") {
                let off = if self.has("misaligned_interval") {
                    SignedDuration::from_millis(1)
                } else {
                    SignedDuration::ZERO
                };
                let r = ctx
                    .reading(
                        sensor.clone(),
                        Metric::new("rain").unwrap(),
                        0.2,
                        Unit::Millimetre,
                    )
                    .with_time(ctx.now() - tick_len() + off);
                // A misaligned one is refused by the core: counted there.
                let _ = ctx.emit(r).await;
            }
            if self.has("untemplated") {
                let r = ctx.reading(
                    sensor.clone(),
                    Metric::new("pressure").unwrap(),
                    1013.0,
                    Unit::Hectopascal,
                );
                let _ = ctx.emit(r).await;
            }
            // Re-send the previous tick's reading: identically (allowed,
            // as after an error) or with another quality (not).
            let previous = *dev.last_reading.lock().unwrap();
            if let Some(prev) = previous
                && (self.has("resend_last") || self.has("requality_last"))
            {
                let q = if self.has("requality_last") {
                    heimr_core::Quality::Suspect
                } else {
                    heimr_core::Quality::Good
                };
                let r = ctx
                    .reading(sensor.clone(), Metric::TEMPERATURE, 20.0, Unit::Celsius)
                    .with_time(prev)
                    .with_quality(q);
                ctx.emit(r).await?;
            }
            *dev.last_reading.lock().unwrap() = Some(time);
            let mut s = ClimateState::new(zone.clone(), time, ClimateMode::Schedule);
            if self.has("nan_state") {
                s = s.with_measured_c(f64::NAN);
            }
            ctx.emit(s).await?;
            let forecast = |from: SignedDuration, to: SignedDuration, value: f64, unit: Unit| {
                let metric = if unit.is_money() { "price" } else { "rate" };
                ctx.forecast(
                    sensor.clone(),
                    Metric::new(metric).unwrap(),
                    value,
                    unit,
                    Basis::Published,
                )
                .valid_between(ctx.now() + from, ctx.now() + to)
                // A new valid time each tick: first seen now.
                .with_changed_at(heimr_core::revision_time(ctx.now()))
            };
            let hour = SignedDuration::from_hours(1);
            if self.has("with_forecasts") {
                ctx.emit(forecast(hour, hour * 2, 1.0, Unit::Dimensionless))
                    .await?;
            }
            if self.has("no_changed_at") {
                let mut f = forecast(hour, hour * 2, 1.0, Unit::Dimensionless);
                f.changed_at = None;
                let _ = ctx.emit(f).await;
            }
            if MISLABELLED {
                let f = ctx
                    .forecast(
                        sensor.clone(),
                        Metric::new("unit_rate").unwrap(),
                        0.2,
                        Unit::Dimensionless,
                        Basis::Published,
                    )
                    .valid_between(ctx.now() + hour, ctx.now() + hour * 2)
                    .with_changed_at(heimr_core::revision_time(ctx.now()));
                ctx.emit(f).await?;
            }
            if self.has("money") {
                ctx.emit(forecast(hour, hour * 2, 0.2, Unit::CurrencyPerKilowattHour))
                    .await?;
            }
            // Refused by the core: ignored here, counted there.
            if self.has("forecast_inverted") {
                let _ = ctx
                    .emit(forecast(hour * 2, hour, 1.0, Unit::Dimensionless))
                    .await;
            }
            if self.has("forecast_beyond_horizon") {
                let _ = ctx
                    .emit(forecast(hour, hour * 24 * 17, 1.0, Unit::Dimensionless))
                    .await;
            }
            // Passed through, not refused.
            if self.has("non_finite") {
                ctx.emit(forecast(hour, hour, f64::NAN, Unit::Dimensionless))
                    .await?;
                let r = ctx
                    .reading(
                        sensor.clone(),
                        Metric::TEMPERATURE,
                        f64::INFINITY,
                        Unit::Celsius,
                    )
                    .with_time(time + SignedDuration::from_millis(1));
                ctx.emit(r).await?;
            }
            if self.has("money_undeclared") {
                let _ = ctx
                    .emit(forecast(hour, hour * 2, 0.2, Unit::CurrencyPerKilowattHour))
                    .await;
            }
            if self.has("future") {
                let r = ctx
                    .reading(sensor.clone(), Metric::HUMIDITY, 50.0, Unit::Percent)
                    .with_time(ctx.now() + SignedDuration::from_hours(1));
                ctx.emit(r).await?;
            }
        }
    }
}

fn suite(flags: &[&'static str]) -> heimr_testkit::Report {
    suite_with(flags, |s| s)
}

fn suite_with(flags: &[&'static str], more: impl FnOnce(Suite) -> Suite) -> heimr_testkit::Report {
    let flags = flags.to_vec();
    let s = Suite::with_upstream::<Rogue, _, _, _>(move || {
        let flags = flags.clone();
        async move { FakeUpstream::new(&flags) }
    })
    .invalid("interval", json!({ "interval": "-1s" }));
    more(s).run()
}

/// Every failed scenario of `r` with its reason.
fn failures(r: &heimr_testkit::Report) -> BTreeMap<String, String> {
    r.results
        .iter()
        .filter(|x| x.status == Status::Failed)
        .map(|x| (x.id.clone(), x.reason.clone().unwrap_or_default()))
        .collect()
}

/// Checks that exactly `expected` scenarios failed, each with a reason
/// containing its substring.
fn exactly(r: &heimr_testkit::Report, expected: &[(&str, &str)]) -> Vec<String> {
    let got = failures(r);
    let mut problems = Vec::new();
    for (id, why) in expected {
        match got.get(*id) {
            None => problems.push(format!("{id} did not fail")),
            Some(reason) if !reason.contains(why) => {
                problems.push(format!("{id}: reason lacks {why:?}: {reason}"));
            }
            Some(_) => {}
        }
    }
    for (id, reason) in &got {
        if !expected.iter().any(|(e, _)| e == id) {
            problems.push(format!("{id} also failed: {reason}"));
        }
    }
    problems
}

fn status(report: &heimr_testkit::Report, id: &str) -> Status {
    report
        .get(id)
        .unwrap_or_else(|| panic!("no scenario {id}"))
        .status
}

#[test]
fn a_well_behaved_adapter_passes_every_scenario() {
    let r = suite(&[]);
    r.assert_passed();
    assert!(
        r.results.iter().all(|x| x.status == Status::Passed),
        "nothing skipped with a full fake upstream\n{r}"
    );
}

/// One violation: the flags that break a rule, the scenarios that must
/// catch it (with a reason substring), and the scenarios that also fail as
/// a direct consequence (with theirs).
struct Case {
    flags: &'static [&'static str],
    caught_by: Vec<(&'static str, &'static str)>,
    collateral: Vec<(&'static str, &'static str)>,
}

fn case(
    flags: &'static [&'static str],
    caught_by: &[(&'static str, &'static str)],
    collateral: &[(&'static str, &'static str)],
) -> Case {
    Case {
        flags,
        caught_by: caught_by.to_vec(),
        collateral: collateral.to_vec(),
    }
}

/// Every scenario that builds or starts the adapter with the example
/// settings: a panic there fails them all.
const BUILDS_EXAMPLE: &[&str] = &[
    "config.rebuild_stable",
    "config.currency_matches_site",
    "run.stays_up",
    "run.emits",
    "run.reports_health",
    "records.no_refusals",
    "records.well_formed",
    "records.units",
    "records.monotonic",
    "records.restart",
    "records.keep_alive",
    "telemetry.counted",
    "shutdown.prompt",
    "shutdown.any_point",
    "commands.shadow",
    "commands.no_unsolicited_writes",
    "commands.live",
    "fault.transient_recovers",
    "fault.no_hot_loop",
    "fault.rejected_is_fatal",
    "fault.malformed_no_panic",
];

fn violations() -> Vec<Case> {
    let mut panic_in_new = case(
        &["panic_in_new"],
        &[(
            "config.accepted",
            "example settings refused: panicked: bad config handling",
        )],
        // Its cases with valid values build the adapter too.
        &[(
            "config.malformed_no_panic",
            "device = 0: panicked: bad config handling",
        )],
    );
    panic_in_new.collateral.extend(
        BUILDS_EXAMPLE
            .iter()
            .map(|id| (*id, "panicked: bad config handling")),
    );
    vec![
        case(
            &["accept_unknown"],
            &[(
                "config.unknown_key",
                "\"heimr_testkit_unknown_key\" was accepted",
            )],
            // Reserved keys are unknown keys to the adapter.
            &[("config.reserved_keys", "\"type\" was accepted")],
        ),
        case(
            &["unstable_caps"],
            &[(
                "config.rebuild_stable",
                "capabilities differ between two builds",
            )],
            &[],
        ),
        case(
            &["accept_negative_interval"],
            &[(
                "config.invalid_values",
                "{\"interval\":\"-1s\"} was accepted",
            )],
            &[],
        ),
        panic_in_new,
        case(
            &["panic_on_max_device"],
            &[(
                "config.malformed_no_panic",
                "device = 18446744073709551615: panicked: device id overflow",
            )],
            &[],
        ),
        case(
            &["io_in_new"],
            &[(
                "config.no_io_in_new",
                "1 requests and 0 writes while building 2 instances",
            )],
            &[],
        ),
        case(
            &["ignore_shutdown"],
            &[
                ("shutdown.prompt", "aborted after the shutdown grace period"),
                (
                    "shutdown.any_point",
                    "at +0s: aborted after the shutdown grace period",
                ),
            ],
            &[(
                "commands.live",
                "shutdown with a command in flight: aborted after the shutdown grace period",
            )],
        ),
        case(
            &["return_early"],
            &[(
                "run.stays_up",
                "failed at 2030-01-01T00:00:00Z: returned while the app was running",
            )],
            // An instance that keeps exiting fails everything that needs
            // it running.
            &[
                ("run.emits", "nothing but health reports"),
                (
                    "records.keep_alive",
                    "door: record-on-change, but nothing emitted in 2m",
                ),
                (
                    "run.reports_health",
                    "health is failed (returned while the app was running)",
                ),
                ("commands.shadow", "returned while the app was running"),
                ("commands.live", "returned while the app was running"),
                (
                    "fault.transient_recovers",
                    "emitted nothing within 5m of the outage ending",
                ),
                ("fault.rejected_is_fatal", "expected down"),
            ],
        ),
        case(
            &["silent"],
            &[("run.emits", "nothing but health reports in 2m")],
            &[
                (
                    "records.keep_alive",
                    "door: record-on-change, but nothing emitted in 2m",
                ),
                (
                    "fault.transient_recovers",
                    "emitted nothing within 5m of the outage ending",
                ),
            ],
        ),
        case(
            &["no_health"],
            &[(
                "run.reports_health",
                "never reported health (ctx.health().ok())",
            )],
            // The outage degrades health, and it is never reported ok
            // again.
            &[(
                "fault.transient_recovers",
                "health is degraded after recovery",
            )],
        ),
        case(
            &["refused_record"],
            &[("records.no_refusals", "records refused (12 for provenance)")],
            // That scenario counts refusals over its whole run.
            &[("fault.malformed_no_panic", "records refused")],
        ),
        case(
            &["duplicate_time"],
            &[(
                "records.monotonic",
                "reading sensor-1/temperature at 2030-01-01T00:00:00Z: the value changed on re-send, on a series that is not versioned",
            )],
            // That scenario judges its first run with the same rules.
            &[(
                "records.restart",
                "the value changed on re-send, on a series that is not versioned",
            )],
        ),
        case(
            &["requality_last"],
            &[(
                "records.monotonic",
                "reading sensor-1/temperature at 2030-01-01T00:00:00Z: quality changed on re-send (Suspect, was Good)",
            )],
            &[(
                "records.restart",
                "quality changed on re-send (Suspect, was Good)",
            )],
        ),
        // A unit other than the template's, and a metric without one, are
        // refused by the core; the kit's own record check is tested
        // against canned records in tests/forecasts.rs.
        case(
            &["wrong_unit"],
            &[(
                "records.no_refusals",
                "last: temperature on sensor-1: unit % differs from its series template's °C",
            )],
            &[("fault.malformed_no_panic", "records refused")],
        ),
        case(
            &["no_changed_at"],
            &[(
                "records.no_refusals",
                "last: rate on sensor-1: the series is versioned, but the record carries no changed_at",
            )],
            &[("fault.malformed_no_panic", "records refused")],
        ),
        case(
            &["stray_changed_at"],
            &[(
                "records.no_refusals",
                "last: humidity on sensor-1: changed_at on a series that is not versioned",
            )],
            &[("fault.malformed_no_panic", "records refused")],
        ),
        case(
            &["no_keepalive"],
            &[(
                "records.keep_alive",
                "sensor-1/door: silent for 2m from 2030-01-01T00:00:00Z to 2030-01-01T00:02:00Z, longer than its keep-alive interval 30s",
            )],
            &[],
        ),
        case(
            &["misaligned_interval"],
            &[("records.no_refusals", "does not start an interval of 10s")],
            &[("fault.malformed_no_panic", "records refused")],
        ),
        case(
            &["untemplated"],
            &[(
                "records.no_refusals",
                "last: reading pressure has no series template: declare one on the adapter kind's descriptor",
            )],
            &[("fault.malformed_no_panic", "records refused")],
        ),
        case(
            &["nan_state"],
            &[("records.well_formed", "measured_c is not finite")],
            // That scenario checks every record of its run.
            &[("fault.malformed_no_panic", "measured_c is not finite")],
        ),
        case(
            &["future"],
            &[("records.well_formed", "timestamp outside")],
            &[],
        ),
        // The core refuses bad forecasts and undeclared money before any
        // sink sees them, so records.no_refusals catches them in the app,
        // with the core's reason; the kit's own record checks are tested
        // against canned records in tests/forecasts.rs.
        case(
            &["forecast_inverted"],
            &[(
                "records.no_refusals",
                "records refused (0 for provenance); last: forecast rate on sensor-1: valid_from",
            )],
            &[("fault.malformed_no_panic", "records refused")],
        ),
        case(
            &["forecast_beyond_horizon"],
            &[(
                "records.no_refusals",
                "more than the 384h horizon after issued_at",
            )],
            &[("fault.malformed_no_panic", "records refused")],
        ),
        case(
            &["money_undeclared"],
            &[(
                "records.no_refusals",
                "last: ¤/kWh is a money unit, but the adapter kind declares no source currency",
            )],
            &[("fault.malformed_no_panic", "records refused")],
        ),
        case(
            &["refuse_foreign_currency"],
            &[(
                "config.currency_matches_site",
                "refused a site quoting XTS, though the kind declares no source currency (declare source_currency in the descriptor instead): adapters.under-test: invalid configuration: this source quotes EUR",
            )],
            &[],
        ),
        case(
            &["write_on_start"],
            &[
                ("commands.shadow", "1 device writes in shadow mode"),
                (
                    "commands.no_unsolicited_writes",
                    "1 device writes without any command (live mode)",
                ),
            ],
            &[],
        ),
        case(
            &["crash_on_command"],
            &[("commands.live", ": unexpected command")],
            &[],
        ),
        case(
            &["hot_retry"],
            &[(
                "fault.no_hot_loop",
                "requests per minute during the outage (limit 60)",
            )],
            &[],
        ),
        case(
            &["fatal_on_outage"],
            &[(
                "fault.transient_recovers",
                "kept down after an outage: gave up",
            )],
            &[],
        ),
        case(
            &["ignore_outage"],
            &[(
                "fault.transient_recovers",
                "health stayed ok throughout the outage",
            )],
            &[],
        ),
        case(
            &["silent_after_outage"],
            &[(
                "fault.transient_recovers",
                "emitted nothing within 5m of the outage ending",
            )],
            &[],
        ),
        case(
            &["retry_rejection"],
            &[("fault.rejected_is_fatal", "expected down")],
            &[],
        ),
        case(
            &["panic_on_garbage"],
            &[("fault.malformed_no_panic", "panicked at")],
            &[],
        ),
        case(
            &["garbage_records"],
            &[("fault.malformed_no_panic", "measured_c is not finite")],
            &[],
        ),
    ]
}

/// Valid forecasts, money from a kind that declares its currency, and
/// interval readings at interval starts pass every scenario.
#[test]
fn valid_forecasts_and_declared_money_pass_every_scenario() {
    let r = suite(&["with_forecasts", "interval"]);
    r.assert_passed();
    assert!(r.results.iter().all(|x| x.status == Status::Passed), "{r}");
    let r = Suite::with_upstream::<Priced, _, _, _>(|| async {
        FakeUpstream::new(&["with_forecasts", "money"])
    })
    .invalid("interval", json!({ "interval": "-1s" }))
    .run();
    r.assert_passed();
    assert!(r.results.iter().all(|x| x.status == Status::Passed), "{r}");
}

/// NaN and ±∞ are passed through: nothing refuses or fails them, and
/// `records.well_formed` notes them, so the author must declare the note.
#[test]
fn non_finite_values_are_noted_not_refused() {
    let r = suite(&["non_finite"]);
    assert!(failures(&r).is_empty(), "{r}");
    assert_eq!(status(&r, "records.no_refusals"), Status::Passed, "{r}");
    let res = r.get("records.well_formed").unwrap();
    assert_eq!(res.status, Status::PassedWithNote, "{r}");
    let why = res.reason.clone().unwrap();
    assert!(
        why.starts_with(
            "non-finite values passed through (the store will record them as bad quality): "
        ),
        "{why}"
    );
    assert!(
        why.contains("value NaN") && why.contains("value inf"),
        "{why}"
    );
    assert!(
        r.problems()
            .iter()
            .any(|p| p.starts_with("records.well_formed: ")),
        "an undeclared note is a problem: {r}"
    );
    let declared = suite_with(&["non_finite"], |s| {
        s.expect_note(
            "records.well_formed",
            "the sensor reports NaN when unplugged",
        )
    });
    declared.assert_passed();
}

/// A kind declaring a currency cannot run for a site quoting another: the
/// app refuses every start, naming both currencies.
#[test]
fn a_priced_kind_under_a_foreign_site_cannot_start() {
    let r = Suite::with_upstream::<Priced, _, _, _>(|| async { FakeUpstream::new(&[]) })
        .invalid("interval", json!({ "interval": "-1s" }))
        .site(Site::new("America/New_York", "USD").unwrap())
        .run();
    let reason = failures(&r)
        .remove("run.stays_up")
        .unwrap_or_else(|| panic!("run.stays_up passed\n{r}"));
    assert!(
        reason.contains("could not start")
            && reason.contains("quotes prices in EUR, but [site].currency is USD"),
        "{reason}"
    );
    // Building alone (no app) is the adapter's business and succeeds.
    assert_eq!(status(&r, "config.accepted"), Status::Passed, "{r}");
}

#[test]
fn each_violation_fails_its_scenarios_and_no_others() {
    let mut missed = Vec::new();
    for c in violations() {
        let r = suite(c.flags);
        let expected: Vec<_> = c.caught_by.iter().chain(&c.collateral).copied().collect();
        for p in exactly(&r, &expected) {
            missed.push(format!("{:?}: {p}", c.flags));
        }
    }
    assert!(missed.is_empty(), "{}", missed.join("\n"));
}

/// A command in flight at shutdown that the adapter never lets go of.
#[test]
fn a_command_blocking_shutdown_fails_live() {
    let r = suite(&["hang_on_command"]);
    let p = exactly(
        &r,
        &[(
            "commands.live",
            "shutdown with a command in flight: aborted after the shutdown grace period",
        )],
    );
    assert!(p.is_empty(), "{}\n{r}", p.join("\n"));
    let reason = failures(&r)["commands.live"].clone();
    assert!(!reason.contains(';'), "only the in-flight check: {reason}");
}

/// Commands queued while the instance fails are dropped. A queue is only
/// dropped when its run ends, and a run ending early is itself reported,
/// so the reason also names that failure.
#[test]
fn dropped_commands_fail_live() {
    let zone = ZoneId::new("zone-a").unwrap();
    let commands: Vec<Command> = [20.0, 21.0, 22.0]
        .into_iter()
        .map(|t| {
            ClimateCommand::set_target(
                zone.clone(),
                Setpoint::new(t).unwrap(),
                Hold::NextScheduleChange,
            )
            .into()
        })
        .collect();
    let r = suite_with(&["drop_queued"], move |s| s.commands(commands));
    let p = exactly(
        &r,
        &[("commands.live", "was dropped: the instance stopped")],
    );
    assert!(p.is_empty(), "{}\n{r}", p.join("\n"));
    let reason = failures(&r)["commands.live"].clone();
    assert!(reason.contains(": lost the device"), "{reason}");
}

/// Re-sending the last reading is normal: an identical repeat across a
/// restart, or within a run (as after an error), passes.
#[test]
fn an_identical_repeat_across_a_restart_passes() {
    for flags in [
        &["restart_on_outage", "reemit_last"][..],
        &["restart_on_outage"],
        &["reemit_last"],
        &["resend_last"],
    ] {
        let r = suite(flags);
        assert_eq!(status(&r, "records.monotonic"), Status::Passed, "{r}");
    }
}

#[test]
fn a_request_count_going_backwards_fails_with_a_reason() {
    let r = suite(&["counter_reset"]);
    let res = r.get("fault.no_hot_loop").unwrap();
    assert_eq!(res.status, Status::Failed, "{r}");
    let reason = res.reason.clone().unwrap();
    assert!(reason.contains("went backwards"), "{reason}");
}

/// Without a traffic count, nothing claims device writes were checked.
#[test]
fn unobserved_writes_are_noted_not_passed() {
    static N: AtomicU64 = AtomicU64::new(1 << 32);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let mut suite = Suite::new::<Rogue>(json!({ "device": id, "interval": "10s" }))
        .invalid("interval", json!({ "interval": "-1s" }))
        .expect_note("commands.shadow", "fixed settings: no traffic count")
        .expect_note("records.monotonic", "fixed settings: no outage");
    for id in [
        "commands.no_unsolicited_writes",
        "config.no_io_in_new",
        "fault.transient_recovers",
        "fault.no_hot_loop",
        "fault.rejected_is_fatal",
        "fault.malformed_no_panic",
    ] {
        suite = suite.expect_skip(id, "fixed settings: no traffic count or faults");
    }
    let r = suite.run();
    r.assert_passed();
    let shadow = r.get("commands.shadow").unwrap();
    assert_eq!(shadow.status, Status::PassedWithNote, "{r}");
    assert!(
        shadow
            .reason
            .as_deref()
            .unwrap()
            .contains("device writes not observed"),
        "{r}"
    );
    for id in [
        "commands.no_unsolicited_writes",
        "config.no_io_in_new",
        "fault.no_hot_loop",
    ] {
        assert_eq!(status(&r, id), Status::Skipped, "{id}\n{r}");
    }
    assert_eq!(
        status(&r, "records.monotonic"),
        Status::PassedWithNote,
        "{r}"
    );
    let text = r.to_string();
    let summary = text.lines().last().unwrap();
    let passed = r.count(Status::Passed);
    assert!(
        summary.starts_with(&format!(
            "  {passed} passed, {} passed with notes, 0 failed, {} skipped",
            r.count(Status::PassedWithNote),
            r.count(Status::Skipped)
        )),
        "{summary}"
    );
    assert_eq!(
        passed + r.count(Status::PassedWithNote) + r.count(Status::Skipped),
        r.results.len()
    );
}

/// An adapter whose descriptor has no summary.
struct Nameless;

impl Adapter for Nameless {
    type Config = serde::de::IgnoredAny;
    fn descriptor() -> AdapterDescriptor {
        AdapterDescriptor::new("nameless", " ", Capabilities::read_only())
    }
    fn new(_: &serde::de::IgnoredAny) -> Result<Self, AdapterError> {
        Ok(Self)
    }
    async fn run(self, ctx: AdapterContext<serde::de::IgnoredAny>) -> Result<(), AdapterError> {
        ctx.shutdown().wait().await;
        Ok(())
    }
}

#[test]
fn an_empty_summary_fails_the_descriptor() {
    let r = Suite::new::<Nameless>(json!({})).emits(false).run();
    assert_eq!(status(&r, "descriptor.valid"), Status::Failed, "{r}");
    assert_eq!(
        r.get("descriptor.valid").unwrap().reason.as_deref(),
        Some("summary is empty"),
        "{r}"
    );
}

/// Templates must follow the documented units of well-known metrics:
/// `records.units` names each template that does not, and nothing else
/// fails (the records match their templates).
#[test]
fn templates_against_documented_units_fail_records_units() {
    let r = Suite::with_upstream::<Mislabelled, _, _, _>(|| async {
        FakeUpstream::new(&["wrong_unit"])
    })
    .invalid("interval", json!({ "interval": "-1s" }))
    .run();
    let p = exactly(
        &r,
        &[(
            "records.units",
            "template for reading temperature: unit % but temperature is documented in °C; template for scheduled unit_rate: unit 1 but unit_rate is documented in ¤/kWh",
        )],
    );
    assert!(p.is_empty(), "{}\n{r}", p.join("\n"));
}

/// An adapter declaring two templates for one metric.
struct Twice;

impl Adapter for Twice {
    type Config = serde::de::IgnoredAny;
    fn descriptor() -> AdapterDescriptor {
        let t = SeriesTemplate::gauge(
            Metric::TEMPERATURE,
            Unit::Celsius,
            SignedDuration::from_secs(10),
        );
        AdapterDescriptor::new("twice", "two templates", Capabilities::read_only())
            .with_series(t.clone())
            .with_series(t)
    }
    fn new(_: &serde::de::IgnoredAny) -> Result<Self, AdapterError> {
        Ok(Self)
    }
    async fn run(self, ctx: AdapterContext<serde::de::IgnoredAny>) -> Result<(), AdapterError> {
        ctx.shutdown().wait().await;
        Ok(())
    }
}

#[test]
fn a_duplicate_series_template_fails_the_descriptor() {
    let r = Suite::new::<Twice>(json!({})).emits(false).run();
    assert_eq!(
        r.get("descriptor.valid").unwrap().reason.as_deref(),
        Some("reading temperature has two series templates"),
        "{r}"
    );
}

#[test]
fn expected_failures_are_reported_and_must_keep_failing() {
    let r = Suite::with_upstream::<Mislabelled, _, _, _>(|| async {
        FakeUpstream::new(&["wrong_unit"])
    })
    .invalid("interval", json!({ "interval": "-1s" }))
    .expect_failure("records.units", "documented in the adapter's README")
    .expect_failure("run.emits", "wrongly declared")
    .run();
    assert_eq!(status(&r, "records.units"), Status::ExpectedFailure);
    assert_eq!(status(&r, "run.emits"), Status::UnexpectedPass);
    assert_eq!(
        r.problems(),
        ["run.emits: declared to fail (wrongly declared) but passed: remove the declaration"],
        "{r}"
    );
}

/// An adapter that declares the site's keys itself.
#[derive(Debug, Deserialize)]
struct LocaleConfig {
    #[serde(default)]
    #[allow(dead_code)]
    time_zone: Option<String>,
}

struct Localised;

impl Adapter for Localised {
    type Config = LocaleConfig;
    fn descriptor() -> AdapterDescriptor {
        AdapterDescriptor::new(
            "localised",
            "declares its own zone",
            Capabilities::read_only(),
        )
    }
    fn new(_: &LocaleConfig) -> Result<Self, AdapterError> {
        Ok(Self)
    }
    async fn run(self, ctx: AdapterContext<LocaleConfig>) -> Result<(), AdapterError> {
        ctx.shutdown().wait().await;
        Ok(())
    }
}

#[test]
fn an_adapter_key_for_the_site_zone_is_flagged() {
    let r = Suite::new::<Localised>(json!({})).emits(false).run();
    assert_eq!(status(&r, "config.reserved_keys"), Status::Failed, "{r}");
    let reason = r
        .get("config.reserved_keys")
        .unwrap()
        .reason
        .clone()
        .unwrap();
    assert!(reason.contains("time_zone"), "{reason}");
}

/// A versioned scheduled template (published, correctable values).
fn scheduled(metric: Metric, unit: Unit, interval: SignedDuration) -> SeriesTemplate {
    SeriesTemplate::scheduled(metric, unit, interval).versioned()
}

/// `records.keep_alive` on a manual clock (stepped, with real time for
/// I/O) across four keep-alive intervals: a door re-sent every tick
/// passes, one sent once fails, naming the silence.
#[test]
fn keep_alive_on_a_manual_clock() {
    use heimr_core::TypedFactory;
    use heimr_testkit::inprocess::UpstreamFactory;
    use heimr_testkit::{
        ClockMode, DriverOptions, Expectations, InProcess, catalogue, run_scenarios,
    };
    let run = |flags: &'static [&'static str]| {
        let scenarios: Vec<_> = catalogue()
            .into_iter()
            .filter(|s| s.id() == "records.keep_alive")
            .collect();
        let upstreams: UpstreamFactory = Arc::new(move || {
            Box::pin(async move { Box::new(FakeUpstream::new(flags)) as Box<dyn Upstream> })
        });
        let mut options = DriverOptions::new(heimr_testkit::test_site());
        options.clock = ClockMode::manual();
        run_scenarios("rogue", &scenarios, &Expectations::default(), &mut || {
            InProcess::new(
                Arc::new(TypedFactory::<Rogue>::new()),
                upstreams.clone(),
                options.clone(),
            )
            .map(|d| Box::new(d) as Box<dyn heimr_testkit::Driver>)
            .map_err(|e| e.to_string())
        })
    };
    let ok = run(&[]);
    assert_eq!(status(&ok, "records.keep_alive"), Status::Passed, "{ok}");
    let silent = run(&["no_keepalive"]);
    let res = silent.get("records.keep_alive").unwrap();
    assert_eq!(res.status, Status::Failed, "{silent}");
    let why = res.reason.as_deref().unwrap();
    assert!(
        why.starts_with("sensor-1/door: silent for 1m 5")
            && why.ends_with("to 2030-01-01T00:02:00Z, longer than its keep-alive interval 30s"),
        "{silent}"
    );
}

/// With no record-on-change template, the scenario passes with a note.
#[test]
fn keep_alive_is_noted_without_record_on_change_series() {
    let r = Suite::new::<Twice>(json!({})).emits(false).run();
    let res = r.get("records.keep_alive").unwrap();
    assert_eq!(res.status, Status::PassedWithNote, "{r}");
    assert_eq!(
        res.reason.as_deref(),
        Some("no series template is record-on-change: nothing needs a keep-alive")
    );
}

/// The rogue's tick: one interval of its rain series.
fn tick_len() -> SignedDuration {
    SignedDuration::from_secs(10)
}
