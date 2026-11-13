//! The suite catches contract violations: a configurable adapter breaks one
//! rule at a time, and the matching scenario must fail while a
//! well-behaved configuration passes everything.

#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use heimr_core::{
    Adapter, AdapterContext, AdapterDescriptor, AdapterError, Capabilities, ClimateCommand,
    ClimateMode, ClimateState, Command, CommandKind, DeviceId, Hold, Metric, Setpoint, Unit,
    ZoneId,
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

struct Rogue {
    flags: Vec<String>,
    /// Which build of its device this instance is (1 for the first).
    build: u64,
}

impl Rogue {
    fn has(&self, f: &str) -> bool {
        self.flags.iter().any(|x| x == f)
    }
}

impl Adapter for Rogue {
    type Config = Config;

    fn descriptor() -> AdapterDescriptor {
        AdapterDescriptor::new(
            "rogue",
            "breaks rules on request",
            Capabilities::read_write([CommandKind::ClimateTarget, CommandKind::ClimateSchedule]),
        )
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
            let r = ctx
                .reading(sensor.clone(), Metric::TEMPERATURE, 20.0, unit)
                .with_time(time);
            ctx.emit(r).await?;
            *dev.last_reading.lock().unwrap() = Some(time);
            let mut s = ClimateState::new(zone.clone(), time, ClimateMode::Schedule);
            if self.has("nan_state") {
                s = s.with_measured_c(f64::NAN);
            }
            ctx.emit(s).await?;
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
    "run.stays_up",
    "run.emits",
    "run.reports_health",
    "records.no_refusals",
    "records.well_formed",
    "records.units",
    "records.monotonic",
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
            &[(
                "fault.transient_recovers",
                "emitted nothing within 5m of the outage ending",
            )],
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
            &[("records.monotonic", "duplicate timestamp")],
            // Every reading is stamped at the start, so none is newer than
            // the outage.
            &[(
                "fault.transient_recovers",
                "emitted nothing within 5m of the outage ending",
            )],
        ),
        case(
            &["restart_on_outage", "reemit_last"],
            &[("records.monotonic", "duplicate timestamp")],
            &[],
        ),
        case(
            &["wrong_unit"],
            &[(
                "records.units",
                "unit % but temperature is documented in °C",
            )],
            &[],
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

/// The parts of the restart violation are harmless alone: repeating the
/// last reading only matters once there is a restart.
#[test]
fn restarting_without_repeating_passes() {
    for flags in [&["restart_on_outage"][..], &["reemit_last"]] {
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

#[test]
fn expected_failures_are_reported_and_must_keep_failing() {
    let r = Suite::with_upstream::<Rogue, _, _, _>(|| async { FakeUpstream::new(&["wrong_unit"]) })
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
