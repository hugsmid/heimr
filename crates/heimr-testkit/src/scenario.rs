//! Scenarios as data: a fixed, named catalogue run against any [`Driver`].
//!
//! A scenario is a trait object with a stable id, a one-line summary and a
//! `run` that only uses the [`Driver`] and the author's [`Expectations`].
//! Nothing here knows how the adapter is hosted, so an out-of-process
//! runner can execute the same [`catalogue`].

use std::collections::BTreeMap;
use std::fmt;

use heimr_core::{
    AdapterDescriptor, AuditResult, Capabilities, ClimateCommand, Command, CommandKind, Currency,
    DeviceId, FanCommand, FanMode, HealthStatus, Hold, Mode, Outcome, Record, RunState, Setpoint,
    SwitchCommand, ZoneId,
};
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::checks;
use crate::driver::{Driver, Fault, PrepareFailure};

/// The result of one scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Verdict {
    /// The adapter met the contract.
    Pass,
    /// Every check that could run passed, but part of the scenario could
    /// not be observed with this fixture; the note says which part.
    PassWithNote(String),
    /// It did not, and why.
    Fail(String),
    /// The scenario does not apply, or the fixture cannot support it.
    Skip(String),
}

/// A per-adapter invalid configuration case: a patch merged into the valid
/// settings (top-level keys; `null` removes a key), and the key the error
/// should name.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct InvalidCase {
    /// Keys to set (`null` removes).
    pub patch: Value,
    /// The key the configuration error should name, if any.
    pub key: Option<String>,
}

impl InvalidCase {
    /// A case: `patch` must be refused, naming `key` if given.
    pub fn new(patch: Value, key: Option<&str>) -> Self {
        Self {
            patch,
            key: key.map(str::to_owned),
        }
    }
}

/// What the author declares about the adapter under test. Everything has a
/// default; most adapters only list invalid settings.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Expectations {
    /// Invalid settings the adapter must refuse.
    pub invalid: Vec<InvalidCase>,
    /// How long a healthy run is observed (fake time). Default 2 minutes;
    /// make it cover a few polling intervals.
    pub warmup: SignedDuration,
    /// How soon after shutdown is signalled `run` must return (default 1 s).
    pub shutdown_deadline: SignedDuration,
    /// Whether a healthy run emits records within the warm-up (default
    /// true).
    pub emits: bool,
    /// How far into the future of the fake clock a record may be stamped
    /// (default [`heimr_core::MAX_FUTURE_SKEW`], 1 s).
    pub max_future: SignedDuration,
    /// How far into the past a record may be stamped, before the run
    /// started (default 31 days; raise it for adapters that back-fill
    /// history).
    pub max_backfill: SignedDuration,
    /// How far past its issue time a forecast may reach (default
    /// [`heimr_core::DEFAULT_FORECAST_HORIZON`]); the in-process driver
    /// runs the app with this `forecast_horizon`.
    pub forecast_horizon: SignedDuration,
    /// Commands to submit in the command scenarios. Default: one per
    /// declared kind, addressed to [`Expectations::command_target`].
    pub commands: Option<Vec<Command>>,
    /// Zone or device addressed by generated commands (default `zone-a`).
    pub command_target: String,
    /// How long an injected outage lasts (default 2 minutes).
    pub outage: SignedDuration,
    /// How long after an outage the adapter has to emit again (default 5
    /// minutes; restarts back off).
    pub recovery: SignedDuration,
    /// Upper bound on upstream requests per minute during an outage
    /// (default 60).
    pub max_attempts_per_minute: u64,
    /// Declared outcomes by scenario id: every skip, note and failure must
    /// be declared for [`Report::passed`].
    pub declarations: BTreeMap<String, Declaration>,
    /// Problems with the declarations themselves (an id declared twice
    /// with different outcomes).
    pub declaration_errors: Vec<String>,
}

/// An outcome the author declares for one scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Expected {
    /// The scenario is skipped (it does not apply, or the fixture cannot
    /// support it).
    Skip,
    /// The scenario passes with a note naming what it could not observe.
    Note,
    /// The scenario fails.
    Failure,
}

impl Expected {
    fn describe(self) -> &'static str {
        match self {
            Self::Skip => "declared skipped",
            Self::Note => "declared to pass with a note",
            Self::Failure => "declared to fail",
        }
    }
}

/// A declared outcome and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Declaration {
    /// The outcome.
    pub outcome: Expected,
    /// Why, in the author's words.
    pub why: String,
}

impl Expectations {
    /// Declare `outcome` for scenario `id`. Declaring an id again with a
    /// different outcome is recorded as an error.
    pub fn declare(&mut self, id: &str, outcome: Expected, why: &str) {
        if let Some(d) = self.declarations.get(id)
            && d.outcome != outcome
        {
            self.declaration_errors.push(format!(
                "{id}: {} and {}",
                d.outcome.describe(),
                outcome.describe()
            ));
        }
        self.declarations.insert(
            id.to_owned(),
            Declaration {
                outcome,
                why: why.to_owned(),
            },
        );
    }
}

impl Default for Expectations {
    fn default() -> Self {
        Self {
            invalid: Vec::new(),
            warmup: SignedDuration::from_mins(2),
            shutdown_deadline: SignedDuration::from_secs(1),
            emits: true,
            max_future: heimr_core::MAX_FUTURE_SKEW,
            max_backfill: SignedDuration::from_hours(31 * 24),
            forecast_horizon: heimr_core::DEFAULT_FORECAST_HORIZON,
            commands: None,
            command_target: "zone-a".to_owned(),
            outage: SignedDuration::from_mins(2),
            recovery: SignedDuration::from_mins(5),
            max_attempts_per_minute: 60,
            declarations: BTreeMap::new(),
            declaration_errors: Vec::new(),
        }
    }
}

/// What a scenario runs with.
pub struct ScenarioContext<'a> {
    /// The adapter under test.
    pub driver: &'a mut dyn Driver,
    /// The author's declarations.
    pub expect: &'a Expectations,
}

impl fmt::Debug for ScenarioContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScenarioContext")
            .field("instance", &self.driver.instance())
            .finish_non_exhaustive()
    }
}

/// One named check of the adapter contract.
pub trait Scenario: Send + Sync {
    /// Stable id, e.g. `shutdown.prompt`.
    fn id(&self) -> &'static str;
    /// One line: what the adapter must do.
    fn summary(&self) -> &'static str;
    /// Run it.
    fn run(&self, cx: &mut ScenarioContext<'_>) -> Verdict;
}

/// A scenario defined by a function.
#[derive(Clone, Copy)]
pub struct FnScenario {
    id: &'static str,
    summary: &'static str,
    run: fn(&mut ScenarioContext<'_>) -> Verdict,
}

impl fmt::Debug for FnScenario {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id)
    }
}

impl Scenario for FnScenario {
    fn id(&self) -> &'static str {
        self.id
    }
    fn summary(&self) -> &'static str {
        self.summary
    }
    fn run(&self, cx: &mut ScenarioContext<'_>) -> Verdict {
        (self.run)(cx)
    }
}

macro_rules! scenarios {
    ($($id:literal => $summary:literal, $f:path;)*) => {
        static CATALOGUE: &[FnScenario] = &[
            $(FnScenario { id: $id, summary: $summary, run: $f },)*
        ];
    };
}

scenarios! {
    "descriptor.valid" => "the descriptor's kind name and capabilities are valid", descriptor_valid;
    "config.accepted" => "the example settings are accepted; instance capabilities are valid and within the descriptor", config_accepted;
    "config.rebuild_stable" => "preparing the same settings again gives the same capabilities (every restart rebuilds)", config_rebuild_stable;
    "config.unknown_key" => "an unknown key is refused as a configuration error naming it", config_unknown_key;
    "config.reserved_keys" => "keys owned by the app (type, enabled, mode, safety) and the site (time zone, currency) are not adapter keys", config_reserved_keys;
    "config.currency_matches_site" => "a kind declaring a source currency runs only for a site in that currency (refused otherwise, naming both); a kind declaring none runs for any site currency", config_currency_matches_site;
    "config.invalid_values" => "every listed invalid setting is refused as a fatal configuration error naming its key", config_invalid_values;
    "config.malformed_no_panic" => "settings of the wrong shape or type are refused without panicking, never as transient errors", config_malformed;
    "config.no_io_in_new" => "building an instance from settings (validation, new) does no upstream I/O", config_no_io_in_new;
    "run.stays_up" => "a healthy run neither returns nor fails before shutdown", run_stays_up;
    "run.emits" => "a healthy run emits records", run_emits;
    "run.reports_health" => "a healthy run reports health, and it is ok", run_reports_health;
    "records.no_refusals" => "the core refuses none of the instance's records", records_no_refusals;
    "records.well_formed" => "records name the instance, use valid identifiers and have sane timestamps; forecasts have ordered periods within the horizon; versioned series carry changed_at, others none; interval series are stamped at interval starts; non-finite values are noted, not failed", records_well_formed;
    "records.units" => "series templates of well-known metrics use the documented unit; every reading and forecast has a template and uses its unit; money units only from a kind declaring a source currency", records_units;
    "records.monotonic" => "within a run, readings of a series that is not versioned strictly increase in time; any repeat (across a restart, or on a versioned series) is identical, and a changed value outranks with a higher changed_at on a versioned series; state never goes back in time", records_monotonic;
    "records.restart" => "after a restart of the app on the same state directory and clock, every repeat is identical and corrections outrank", records_restart;
    "records.keep_alive" => "a record-on-change series is re-sent at least once per expected interval while unchanged", records_keep_alive;
    "telemetry.counted" => "the core's stats count exactly the records the instance emitted", telemetry_counted;
    "shutdown.prompt" => "after a healthy run, shutdown returns Ok within the deadline", shutdown_prompt;
    "shutdown.any_point" => "shutdown at arbitrary points (start-up, mid-interval, on a tick) returns Ok within the deadline", shutdown_any_point;
    "commands.shadow" => "shadow mode: commands are audited, never delivered, and cause no device writes", commands_shadow;
    "commands.no_unsolicited_writes" => "without commands (live if write-capable, else shadow): no device writes", commands_no_unsolicited_writes;
    "commands.live" => "live mode: every command is delivered with a delivered audit record; the instance survives every declared kind and stops promptly with one in flight", commands_live;
    "fault.transient_recovers" => "an outage is not fatal, is reflected in health, and the instance emits again once it ends", fault_transient_recovers;
    "fault.no_hot_loop" => "during an outage the upstream is retried at a bounded rate", fault_no_hot_loop;
    "fault.rejected_is_fatal" => "rejected credentials keep the instance down (fatal) without restarts", fault_rejected_is_fatal;
    "fault.malformed_no_panic" => "garbage from the upstream causes no panic and no invalid records", fault_malformed;
}

/// The conformance catalogue, in run order.
pub fn catalogue() -> Vec<&'static dyn Scenario> {
    CATALOGUE.iter().map(|s| s as &dyn Scenario).collect()
}

// --- helpers ----------------------------------------------------------------

fn fail(problems: Vec<String>) -> Verdict {
    noted(problems, Vec::new())
}

/// Fail on any problem; otherwise pass, with a note listing the parts that
/// could not be observed.
fn noted(problems: Vec<String>, unobserved: Vec<String>) -> Verdict {
    if !problems.is_empty() {
        Verdict::Fail(problems.join("; "))
    } else if !unobserved.is_empty() {
        Verdict::PassWithNote(unobserved.join("; "))
    } else {
        Verdict::Pass
    }
}

/// The note for a fixture whose upstream does not count traffic.
const NO_TRAFFIC: &str = "the upstream does not report traffic";

fn merged(base: &Value, patch: &Value) -> Value {
    let mut out = base.clone();
    if let (Some(o), Some(p)) = (out.as_object_mut(), patch.as_object()) {
        for (k, v) in p {
            if v.is_null() {
                o.remove(k);
            } else {
                o.insert(k.clone(), v.clone());
            }
        }
    }
    out
}

fn describe(f: &PrepareFailure) -> String {
    match f {
        PrepareFailure::Refused {
            config,
            key,
            fatal,
            message,
        } => format!(
            "{} error{}{}: {message}",
            if *config {
                "configuration"
            } else {
                "non-configuration"
            },
            key.as_ref()
                .map(|k| format!(" naming {k:?}"))
                .unwrap_or_default(),
            if *fatal { "" } else { " (transient)" },
        ),
        PrepareFailure::Panicked { message } => format!("panicked: {message}"),
    }
}

/// Start a healthy shadow run and observe it for the warm-up.
fn healthy_run(cx: &mut ScenarioContext<'_>) -> Result<Timestamp, Verdict> {
    let started = cx.driver.now();
    cx.driver
        .start(Mode::Shadow)
        .map_err(|e| Verdict::Fail(format!("could not start: {e}")))?;
    cx.driver.advance(cx.expect.warmup);
    Ok(started)
}

fn run_problems(cx: &ScenarioContext<'_>) -> Vec<String> {
    let s = cx.driver.stats();
    let mut p = Vec::new();
    if s.run_state != RunState::Running {
        p.push(format!("run state is {:?}, not running", s.run_state));
    }
    for f in cx.driver.failures() {
        p.push(format!(
            "failed at {}: {}{}",
            f.time,
            f.error,
            if f.fatal { " (fatal)" } else { "" }
        ));
    }
    p
}

fn finish(cx: &mut ScenarioContext<'_>, v: Verdict) -> Verdict {
    cx.driver.stop();
    v
}

/// Generated sample commands: one per declared kind.
pub fn sample_commands(caps: &Capabilities, target: &str) -> Result<Vec<Command>, String> {
    let zone = ZoneId::new(target).map_err(|e| format!("command target {target:?}: {e}"))?;
    let device = DeviceId::from(zone.clone());
    let hold = Hold::for_duration(SignedDuration::from_mins(30)).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for kind in &caps.write {
        let cmd: Command = match kind {
            CommandKind::ClimateTarget => {
                let sp = Setpoint::new(20.0).map_err(|e| e.to_string())?;
                ClimateCommand::set_target(zone.clone(), sp, Hold::NextScheduleChange).into()
            }
            CommandKind::ClimateSchedule => ClimateCommand::resume_schedule(zone.clone()).into(),
            CommandKind::ClimateOff => ClimateCommand::turn_off(zone.clone(), hold).into(),
            CommandKind::FanMode => {
                let mode = match (caps.fan_levels, caps.fan_presets.first()) {
                    (Some(r), _) => FanMode::Manual(r.max),
                    (None, Some(p)) => FanMode::preset(p.clone()),
                    (None, None) => FanMode::Auto,
                };
                FanCommand::set_mode(device.clone(), mode, hold).into()
            }
            CommandKind::Switch => SwitchCommand::set(device.clone(), true).into(),
            _ => continue,
        };
        out.push(cmd);
    }
    Ok(out)
}

fn commands_for(cx: &mut ScenarioContext<'_>) -> Result<Vec<Command>, Verdict> {
    let commands = match &cx.expect.commands {
        Some(c) => c.clone(),
        None => {
            let settings = cx.driver.settings();
            let caps = cx
                .driver
                .prepare(&settings)
                .map_err(|f| Verdict::Fail(format!("settings refused: {}", describe(&f))))?;
            sample_commands(&caps, &cx.expect.command_target).map_err(Verdict::Fail)?
        }
    };
    if commands.is_empty() {
        return Err(Verdict::Skip(
            "no commands to submit: none generated for the declared kinds (Suite::commands)".into(),
        ));
    }
    Ok(commands)
}

fn write_capable(cx: &mut ScenarioContext<'_>) -> Option<Verdict> {
    if cx.driver.descriptor().capabilities.can_write() {
        None
    } else {
        Some(Verdict::Skip("read-only adapter".into()))
    }
}

/// Submit each command, spaced past the per-target interval.
fn submit_all(
    cx: &mut ScenarioContext<'_>,
    commands: &[Command],
) -> Vec<Result<Outcome, heimr_core::Rejection>> {
    commands
        .iter()
        .map(|c| {
            cx.driver.advance(SignedDuration::from_secs(2));
            cx.driver.submit(c.clone())
        })
        .collect()
}

// --- scenarios --------------------------------------------------------------

fn descriptor_valid(cx: &mut ScenarioContext<'_>) -> Verdict {
    let d: AdapterDescriptor = cx.driver.descriptor();
    match d.validate() {
        Ok(()) if d.summary.trim().is_empty() => Verdict::Fail("summary is empty".into()),
        Ok(()) => Verdict::Pass,
        Err(e) => Verdict::Fail(e.to_string()),
    }
}

fn config_accepted(cx: &mut ScenarioContext<'_>) -> Verdict {
    let settings = cx.driver.settings();
    match cx.driver.prepare(&settings) {
        Ok(caps) => {
            let kind = cx.driver.descriptor().capabilities;
            match caps.validate() {
                Err(e) => Verdict::Fail(format!("instance capabilities: {e}")),
                Ok(()) if !kind.includes(&caps) => {
                    Verdict::Fail("instance capabilities exceed the descriptor's".into())
                }
                Ok(()) => {
                    let series = cx.driver.series();
                    match cx.driver.descriptor().check_instance_series(&series) {
                        Ok(()) => Verdict::Pass,
                        Err(e) => Verdict::Fail(e.to_string()),
                    }
                }
            }
        }
        Err(f) => Verdict::Fail(format!("example settings refused: {}", describe(&f))),
    }
}

fn config_rebuild_stable(cx: &mut ScenarioContext<'_>) -> Verdict {
    let settings = cx.driver.settings();
    let a = cx.driver.prepare(&settings);
    let b = cx.driver.prepare(&settings);
    match (a, b) {
        (Ok(a), Ok(b)) if a == b => Verdict::Pass,
        (Ok(_), Ok(_)) => Verdict::Fail("capabilities differ between two builds".into()),
        (Err(f), _) | (_, Err(f)) => Verdict::Fail(describe(&f)),
    }
}

fn expect_unknown_key(cx: &mut ScenarioContext<'_>, key: &str, value: Value) -> Option<String> {
    let base = cx.driver.settings();
    let settings = merged(&base, &json!({ key: value }));
    match cx.driver.prepare(&settings) {
        Ok(_) => Some(format!("{key:?} was accepted")),
        Err(PrepareFailure::Refused {
            config: true,
            key: Some(k),
            fatal: true,
            ..
        }) if k == key => None,
        Err(f) => Some(format!(
            "{key:?}: expected an unknown-key error, got {}",
            describe(&f)
        )),
    }
}

fn config_unknown_key(cx: &mut ScenarioContext<'_>) -> Verdict {
    if !cx.driver.settings().is_object() {
        return Verdict::Fail("settings must be a JSON object".into());
    }
    fail(
        expect_unknown_key(cx, "heimr_testkit_unknown_key", json!(1))
            .into_iter()
            .collect(),
    )
}

/// Keys an adapter must never declare: the app's own instance keys, and the
/// site's time zone and currency (set once, in `[site]`).
pub const NOT_ADAPTER_KEYS: &[&str] = &[
    "type",
    "enabled",
    "mode",
    "safety",
    "time_zone",
    "timezone",
    "tz",
    "currency",
];

fn config_reserved_keys(cx: &mut ScenarioContext<'_>) -> Verdict {
    let mut p = Vec::new();
    for key in NOT_ADAPTER_KEYS {
        // A string is the most plausible value for each of these.
        if let Some(problem) = expect_unknown_key(cx, key, json!("Europe/Berlin")) {
            p.push(problem);
        }
    }
    fail(p)
}

/// ISO 4217's code for testing: no real site quotes it.
const TEST_CURRENCY: Currency = Currency::of("XTS");
/// ISO 4217's code for "no currency", the fallback when a kind quotes XTS.
const NO_CURRENCY: Currency = Currency::of("XXX");

/// The app does the matching; this checks the adapter does not get in its
/// way (a kind without a source currency must not check the site's
/// currency itself) and that the declared currency is honoured.
fn config_currency_matches_site(cx: &mut ScenarioContext<'_>) -> Verdict {
    let Some(quoted) = cx.driver.descriptor().source_currency else {
        return match cx.driver.check_site(TEST_CURRENCY) {
            Ok(()) => Verdict::Pass,
            Err(e) => Verdict::Fail(format!(
                "refused a site quoting {TEST_CURRENCY}, though the kind declares no source currency (declare source_currency in the descriptor instead): {e}"
            )),
        };
    };
    let mut p = Vec::new();
    if let Err(e) = cx.driver.check_site(quoted) {
        p.push(format!(
            "refused a site quoting its own source currency {quoted}: {e}"
        ));
    }
    let other = if quoted == TEST_CURRENCY {
        NO_CURRENCY
    } else {
        TEST_CURRENCY
    };
    match cx.driver.check_site(other) {
        Ok(()) => p.push(format!(
            "accepted a site quoting {other}, though the kind quotes {quoted}"
        )),
        Err(e) if !(e.contains(quoted.as_str()) && e.contains(other.as_str())) => p.push(format!(
            "refused a site quoting {other} without naming both currencies: {e}"
        )),
        Err(_) => {}
    }
    fail(p)
}

fn config_invalid_values(cx: &mut ScenarioContext<'_>) -> Verdict {
    if cx.expect.invalid.is_empty() {
        return Verdict::Skip("no invalid settings listed (Suite::invalid)".into());
    }
    let base = cx.driver.settings();
    let mut p = Vec::new();
    for case in &cx.expect.invalid {
        let settings = merged(&base, &case.patch);
        match cx.driver.prepare(&settings) {
            Ok(_) => p.push(format!("{} was accepted", case.patch)),
            Err(PrepareFailure::Refused {
                config: true,
                key,
                fatal: true,
                ..
            }) => {
                if let Some(want) = &case.key
                    && key.as_deref() != Some(want.as_str())
                {
                    p.push(format!(
                        "{}: error names {:?}, expected {want:?}",
                        case.patch, key
                    ));
                }
            }
            Err(f) => p.push(format!("{}: {}", case.patch, describe(&f))),
        }
    }
    fail(p)
}

fn config_malformed(cx: &mut ScenarioContext<'_>) -> Verdict {
    let base = cx.driver.settings();
    let mut p = Vec::new();
    let substitutes = [
        Value::Null,
        json!(true),
        json!(-1),
        json!(0),
        json!(u64::MAX),
        json!(1.5e300),
        json!(""),
        json!("not a value \u{0} ☃"),
        json!([]),
        json!([1, "x", null]),
        json!({ "nested": { "deeper": [] } }),
    ];
    let mut cases: Vec<(String, Value, bool)> = vec![
        ("settings = null".into(), Value::Null, true),
        ("settings = []".into(), json!([]), true),
        ("settings = \"x\"".into(), json!("x"), true),
        ("settings = 42".into(), json!(42), true),
        ("settings = {}".into(), json!({}), false),
    ];
    if let Some(obj) = base.as_object() {
        for key in obj.keys() {
            for v in &substitutes {
                let mut s = base.clone();
                if let Some(o) = s.as_object_mut() {
                    o.insert(key.clone(), v.clone());
                }
                cases.push((format!("{key} = {v}"), s, false));
            }
        }
    }
    for (what, settings, must_refuse) in cases {
        match cx.driver.prepare(&settings) {
            Ok(_) if must_refuse => p.push(format!("{what}: accepted")),
            Ok(_) => {}
            Err(f @ PrepareFailure::Panicked { .. }) => p.push(format!("{what}: {}", describe(&f))),
            Err(PrepareFailure::Refused {
                fatal: false,
                message,
                ..
            }) => {
                p.push(format!("{what}: refused as transient: {message}"));
            }
            Err(PrepareFailure::Refused {
                config: false,
                message,
                ..
            }) if must_refuse => {
                p.push(format!("{what}: not a configuration error: {message}"));
            }
            Err(_) => {}
        }
    }
    fail(p)
}

fn config_no_io_in_new(cx: &mut ScenarioContext<'_>) -> Verdict {
    let base = cx.driver.settings();
    let Some(before) = cx.driver.traffic() else {
        return Verdict::Skip(NO_TRAFFIC.into());
    };
    let mut cases = vec![base.clone()];
    cases.extend(cx.expect.invalid.iter().map(|c| merged(&base, &c.patch)));
    for settings in &cases {
        let _ = cx.driver.prepare(settings);
    }
    let after = cx.driver.traffic().unwrap_or(before);
    let requests = after.requests.saturating_sub(before.requests);
    let writes = after.writes.saturating_sub(before.writes);
    if requests == 0 && writes == 0 {
        Verdict::Pass
    } else {
        Verdict::Fail(format!(
            "{requests} requests and {writes} writes while building {} instances (validate only; connect in run)",
            cases.len()
        ))
    }
}

fn run_stays_up(cx: &mut ScenarioContext<'_>) -> Verdict {
    if let Err(v) = healthy_run(cx) {
        return v;
    }
    let v = fail(run_problems(cx));
    finish(cx, v)
}

fn run_emits(cx: &mut ScenarioContext<'_>) -> Verdict {
    if !cx.expect.emits {
        return Verdict::Skip("declared not to emit (Suite::emits(false))".into());
    }
    if let Err(v) = healthy_run(cx) {
        return v;
    }
    let n = cx
        .driver
        .records()
        .iter()
        .filter(|e| !matches!(&e.record, Record::Event(ev) if ev.kind == heimr_core::Event::ADAPTER_HEALTH))
        .count();
    let v = if n == 0 {
        Verdict::Fail(format!(
            "nothing but health reports in {:#}",
            cx.expect.warmup
        ))
    } else {
        Verdict::Pass
    };
    finish(cx, v)
}

fn run_reports_health(cx: &mut ScenarioContext<'_>) -> Verdict {
    if let Err(v) = healthy_run(cx) {
        return v;
    }
    let s = cx.driver.stats();
    let v = match s.health {
        HealthStatus::Ok => Verdict::Pass,
        HealthStatus::Unknown => Verdict::Fail("never reported health (ctx.health().ok())".into()),
        other => Verdict::Fail(format!(
            "health is {} ({})",
            other.as_str(),
            s.health_reason.unwrap_or_default()
        )),
    };
    finish(cx, v)
}

fn records_no_refusals(cx: &mut ScenarioContext<'_>) -> Verdict {
    if let Err(v) = healthy_run(cx) {
        return v;
    }
    let s = cx.driver.stats();
    let v = if s.emit_errors == 0 {
        Verdict::Pass
    } else {
        Verdict::Fail(format!(
            "{} records refused ({} for provenance); last: {}",
            s.emit_errors,
            s.provenance_rejections,
            s.last_emit_error.unwrap_or_default()
        ))
    };
    finish(cx, v)
}

fn records_well_formed(cx: &mut ScenarioContext<'_>) -> Verdict {
    let started = match healthy_run(cx) {
        Ok(t) => t,
        Err(v) => return v,
    };
    let earliest = started
        .checked_sub(cx.expect.max_backfill)
        .unwrap_or(Timestamp::MIN);
    let latest = cx
        .driver
        .now()
        .checked_add(cx.expect.max_future)
        .unwrap_or(Timestamp::MAX);
    let instance = cx.driver.instance().to_owned();
    let records = cx.driver.records();
    let mut problems = checks::well_formed(
        &instance,
        &records,
        earliest,
        latest,
        cx.expect.forecast_horizon,
    );
    let series = cx.driver.descriptor().series;
    problems.extend(checks::versioning(&records, &series));
    problems.extend(checks::intervals(&records, &series));
    // Passed through, not refused: worth a note, never a failure.
    let odd = checks::non_finite(&records);
    let notes = if odd.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "non-finite values passed through (the store will record them as bad quality): {}",
            odd.join("; ")
        )]
    };
    let v = noted(problems, notes);
    finish(cx, v)
}

fn records_units(cx: &mut ScenarioContext<'_>) -> Verdict {
    if let Err(v) = healthy_run(cx) {
        return v;
    }
    let d = cx.driver.descriptor();
    let v = fail(checks::units(
        &cx.driver.records(),
        &d.series,
        d.source_currency,
    ));
    finish(cx, v)
}

/// A healthy run, then (where the upstream can simulate one) an outage and
/// the recovery after it, checked as one sequence of runs: whatever the
/// adapter does to recover (reconnect inside `run`, or return an error and
/// be restarted by the supervisor), repeats must be identical and
/// corrections must outrank ([`checks::monotonic`]). Restarts of the whole
/// app are out of reach here: each scenario starts a fresh app and clock;
/// test them with [`Harness::restart`](crate::Harness::restart).
fn records_monotonic(cx: &mut ScenarioContext<'_>) -> Verdict {
    if let Err(e) = cx.driver.start(Mode::Shadow) {
        return Verdict::Fail(format!("could not start: {e}"));
    }
    let can_fault = cx.driver.inject(Some(Fault::Unavailable));
    cx.driver.inject(None);
    cx.driver.advance(cx.expect.warmup);
    let mut unobserved = Vec::new();
    if can_fault {
        cx.driver.inject(Some(Fault::Unavailable));
        cx.driver.advance(cx.expect.outage);
        cx.driver.inject(None);
        cx.driver.advance(cx.expect.recovery);
    } else {
        unobserved.push(
            "only one uninterrupted run checked, not a restart or reconnect: the upstream cannot simulate an outage"
                .to_owned(),
        );
    }
    let series = cx.driver.descriptor().series;
    let v = noted(checks::monotonic(&cx.driver.runs(), &series), unobserved);
    finish(cx, v)
}

/// A restart of the whole app: a healthy run, then the instance started
/// again on the same state directory, upstream and clock, judged as one
/// sequence of runs ([`checks::monotonic`]). This is where what an adapter
/// keeps in its state directory (revision markers, first sightings) must
/// survive: re-sent records must be bit-identical.
fn records_restart(cx: &mut ScenarioContext<'_>) -> Verdict {
    if let Err(v) = healthy_run(cx) {
        return v;
    }
    let mut runs = cx.driver.runs();
    if let Err(e) = cx.driver.restart(Mode::Shadow) {
        return finish(cx, Verdict::Skip(e));
    }
    cx.driver.advance(cx.expect.warmup);
    runs.extend(cx.driver.runs());
    let series = cx.driver.descriptor().series;
    let v = fail(checks::monotonic(&runs, &series));
    finish(cx, v)
}

/// Record-on-change series must send a keep-alive at least once per
/// expected interval while the value is unchanged. Runs a healthy instance
/// for four of the longest such intervals on the fake clock and measures,
/// per series (device, metric), every gap between reading times, from the
/// start to the first and from the last to the end. Passes with a note
/// when no template is record-on-change (polled series keep themselves
/// alive), and fails a record-on-change metric nothing emitted: the
/// instance's templates name only what it emits ([`Driver::series`]).
fn records_keep_alive(cx: &mut ScenarioContext<'_>) -> Verdict {
    // The instance's templates: its configuration sets the intervals.
    let series: Vec<_> = cx
        .driver
        .series()
        .into_iter()
        .filter(|t| t.record_on_change && !t.for_forecasts())
        .collect();
    let Some(longest) = series.iter().map(|t| t.expected_interval).max() else {
        return Verdict::PassWithNote(
            "no series template is record-on-change: nothing needs a keep-alive".into(),
        );
    };
    let started = cx.driver.now();
    if let Err(e) = cx.driver.start(Mode::Shadow) {
        return Verdict::Fail(format!("could not start: {e}"));
    }
    let span = longest.checked_mul(4).unwrap_or(longest);
    cx.driver.advance(span);
    let end = cx.driver.now();
    let records = cx.driver.records();
    let mut problems = Vec::new();
    for t in &series {
        let mut by_device: BTreeMap<String, Vec<Timestamp>> = BTreeMap::new();
        for env in &records {
            if let Record::Reading(r) = &env.record
                && r.metric == t.metric
            {
                by_device
                    .entry(r.device.to_string())
                    .or_default()
                    .push(r.time);
            }
        }
        // The instance declares the template, so it emits the metric: a
        // record-on-change source sends its state at start, then keeps it
        // alive.
        if by_device.is_empty() {
            problems.push(format!(
                "{}: record-on-change, but nothing emitted in {span:#}",
                t.metric
            ));
        }
        for (device, mut times) in by_device {
            times.sort();
            let mut prev = started;
            for at in times.into_iter().chain([end]) {
                let gap = at.duration_since(prev);
                if gap > t.expected_interval {
                    problems.push(format!(
                        "{device}/{}: silent for {gap:#} from {prev} to {at}, longer than its keep-alive interval {:#}",
                        t.metric, t.expected_interval
                    ));
                    break;
                }
                prev = prev.max(at);
            }
        }
    }
    let v = fail(problems);
    finish(cx, v)
}

/// This checks the core rather than the adapter: the core counts records
/// at the seam it owns, so no adapter behaviour can make this fail without
/// a core bug. It stays in the catalogue for drivers whose host does its
/// own counting (adapters in other processes).
fn telemetry_counted(cx: &mut ScenarioContext<'_>) -> Verdict {
    if let Err(v) = healthy_run(cx) {
        return v;
    }
    let s = cx.driver.stop();
    let records = cx.driver.records();
    let (mut readings, mut forecasts, mut states, mut events) = (0u64, 0u64, 0u64, 0u64);
    for env in &records {
        match env.record {
            Record::Reading(_) => readings += 1,
            Record::Forecast(_) => forecasts += 1,
            Record::Event(_) => events += 1,
            _ => states += 1,
        }
    }
    let mut p = Vec::new();
    let counted = (s.readings, s.forecasts, s.states, s.events);
    if counted != (readings, forecasts, states, events) {
        p.push(format!(
            "stats count {}/{}/{}/{} readings/forecasts/states/events, sink saw {readings}/{forecasts}/{states}/{events}",
            s.readings, s.forecasts, s.states, s.events
        ));
    }
    let newest = records.iter().map(|e| e.record.time()).max();
    if s.last_emit_at != newest {
        p.push(format!(
            "last_emit_at {:?}, newest record {:?}",
            s.last_emit_at, newest
        ));
    }
    fail(p)
}

fn stopped_cleanly(s: &heimr_core::AdapterStats) -> Option<String> {
    match (&s.run_state, &s.exit_error) {
        (RunState::Stopped, None) => None,
        (RunState::Stopped, Some(e)) => Some(e.clone()),
        (state, e) => Some(format!(
            "run state {state:?}{}",
            e.as_ref().map(|e| format!(": {e}")).unwrap_or_default()
        )),
    }
}

fn shutdown_prompt(cx: &mut ScenarioContext<'_>) -> Verdict {
    if let Err(v) = healthy_run(cx) {
        return v;
    }
    let s = cx.driver.stop();
    match stopped_cleanly(&s) {
        None => Verdict::Pass,
        Some(e) => Verdict::Fail(e),
    }
}

/// Offsets into a run at which shutdown is signalled.
fn shutdown_offsets(warmup: SignedDuration) -> Vec<SignedDuration> {
    let ms = SignedDuration::from_millis;
    let mut out = vec![
        SignedDuration::ZERO,
        ms(1),
        ms(999),
        ms(1000),
        ms(2500),
        ms(10_000),
        ms(30_001),
        ms(59_999),
        ms(60_000),
    ];
    // Deterministic pseudo-random points within the warm-up.
    let span = warmup.as_millis().max(1) as u64;
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..6 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push(ms((x % span) as i64));
    }
    out
}

fn shutdown_any_point(cx: &mut ScenarioContext<'_>) -> Verdict {
    let mut p = Vec::new();
    for at in shutdown_offsets(cx.expect.warmup) {
        if let Err(e) = cx.driver.start(Mode::Shadow) {
            return Verdict::Fail(format!("could not start: {e}"));
        }
        cx.driver.settle();
        if at > SignedDuration::ZERO {
            cx.driver.advance(at);
        }
        let s = cx.driver.stop();
        if let Some(e) = stopped_cleanly(&s) {
            p.push(format!("at +{at:#}: {e}"));
        }
    }
    fail(p)
}

fn commands_shadow(cx: &mut ScenarioContext<'_>) -> Verdict {
    if let Some(v) = write_capable(cx) {
        return v;
    }
    let commands = match commands_for(cx) {
        Ok(c) => c,
        Err(v) => return v,
    };
    if let Err(e) = cx.driver.start(Mode::Shadow) {
        return Verdict::Fail(format!("could not start: {e}"));
    }
    cx.driver.advance(SignedDuration::from_secs(5));
    let outcomes = submit_all(cx, &commands);
    cx.driver.advance(SignedDuration::from_secs(30));
    let mut p = run_problems(cx);
    for (c, o) in commands.iter().zip(&outcomes) {
        if !matches!(o, Ok(Outcome::Shadowed { .. })) {
            p.push(format!(
                "{} {}: {o:?}, expected shadowed",
                c.kind(),
                c.target()
            ));
        }
    }
    let audited = cx
        .driver
        .audit()
        .iter()
        .filter(|r| r.result == AuditResult::Shadowed)
        .count();
    if audited != commands.len() {
        p.push(format!(
            "{audited} shadowed audit records for {} commands",
            commands.len()
        ));
    }
    let mut unobserved = Vec::new();
    match cx.driver.traffic() {
        Some(t) if t.writes > 0 => p.push(format!("{} device writes in shadow mode", t.writes)),
        Some(_) => {}
        None => unobserved.push(format!("device writes not observed: {NO_TRAFFIC}")),
    }
    let s = cx.driver.stats();
    if s.commands_delivered > 0 {
        p.push(format!("{} commands delivered", s.commands_delivered));
    }
    finish(cx, noted(p, unobserved))
}

fn commands_no_unsolicited_writes(cx: &mut ScenarioContext<'_>) -> Verdict {
    // A read-only adapter cannot be live; it must not write in shadow mode
    // either.
    let mode = if cx.driver.descriptor().capabilities.can_write() {
        Mode::Live
    } else {
        Mode::Shadow
    };
    if let Err(e) = cx.driver.start(mode) {
        return Verdict::Fail(format!("could not start: {e}"));
    }
    cx.driver.advance(cx.expect.warmup);
    let v = match cx.driver.traffic() {
        None => Verdict::Skip(NO_TRAFFIC.into()),
        Some(t) if t.writes == 0 => Verdict::Pass,
        Some(t) => Verdict::Fail(format!(
            "{} device writes without any command ({} mode)",
            t.writes,
            mode.as_str()
        )),
    };
    finish(cx, v)
}

/// Sees the outcomes and the audit trail, not the order of events inside
/// the core: that the audit record is durable before the adapter receives
/// a command is the core's guarantee, tested in the core.
fn commands_live(cx: &mut ScenarioContext<'_>) -> Verdict {
    if let Some(v) = write_capable(cx) {
        return v;
    }
    let commands = match commands_for(cx) {
        Ok(c) => c,
        Err(v) => return v,
    };
    if let Err(e) = cx.driver.start(Mode::Live) {
        return Verdict::Fail(format!("could not start: {e}"));
    }
    cx.driver.advance(SignedDuration::from_secs(5));
    let outcomes = submit_all(cx, &commands);
    cx.driver.advance(SignedDuration::from_secs(30));
    let mut p = run_problems(cx);
    let audit = cx.driver.audit();
    for (c, o) in commands.iter().zip(&outcomes) {
        match o {
            Ok(Outcome::Delivered { id }) => {
                if !audit
                    .iter()
                    .any(|r| r.id == *id && r.result == AuditResult::Delivered)
                {
                    p.push(format!("command {id} delivered without an audit record"));
                }
            }
            other => p.push(format!(
                "{} {}: {other:?}, expected delivered",
                c.kind(),
                c.target()
            )),
        }
    }
    if let Some(dropped) = audit
        .iter()
        .find(|r| matches!(r.result, AuditResult::Dropped { .. }))
    {
        p.push(format!(
            "command {} was dropped: the instance stopped",
            dropped.id
        ));
    }
    // A command in flight at shutdown must not delay it.
    if let Some(c) = commands.first() {
        cx.driver.advance(SignedDuration::from_secs(2));
        let _ = cx.driver.submit(c.clone());
    }
    let s = cx.driver.stop();
    if let Some(e) = stopped_cleanly(&s) {
        p.push(format!("shutdown with a command in flight: {e}"));
    }
    fail(p)
}

/// How many records other than events the instance has emitted so far.
/// Counted by arrival, not by timestamp: a re-sent record keeps its time.
fn emitted(cx: &ScenarioContext<'_>) -> usize {
    cx.driver
        .records()
        .iter()
        .filter(|e| !matches!(e.record, Record::Event(_)))
        .count()
}

fn fault_transient_recovers(cx: &mut ScenarioContext<'_>) -> Verdict {
    if let Err(e) = cx.driver.start(Mode::Shadow) {
        return Verdict::Fail(format!("could not start: {e}"));
    }
    if !cx.driver.inject(Some(Fault::Unavailable)) {
        return finish(
            cx,
            Verdict::Skip("the upstream cannot simulate an outage".into()),
        );
    }
    cx.driver.inject(None);
    cx.driver.advance(cx.expect.warmup / 2);
    cx.driver.inject(Some(Fault::Unavailable));
    cx.driver.advance(cx.expect.outage);
    let mut p = Vec::new();
    let s = cx.driver.stats();
    if s.health == HealthStatus::Ok {
        p.push("health stayed ok throughout the outage".to_owned());
    }
    cx.driver.inject(None);
    let cleared = emitted(cx);
    cx.driver.advance(cx.expect.recovery);
    let s = cx.driver.stats();
    if s.run_state == RunState::Down {
        p.push(format!(
            "kept down after an outage: {}",
            s.exit_error.unwrap_or_default()
        ));
    } else if cx.expect.emits && emitted(cx) == cleared {
        p.push(format!(
            "emitted nothing within {:#} of the outage ending",
            cx.expect.recovery
        ));
    }
    if cx.expect.emits && s.health != HealthStatus::Ok && s.run_state == RunState::Running {
        p.push(format!("health is {} after recovery", s.health.as_str()));
    }
    let unobserved = if cx.expect.emits {
        Vec::new()
    } else {
        vec!["emitting and health after the outage not checked: declared not to emit".to_owned()]
    };
    finish(cx, noted(p, unobserved))
}

fn fault_no_hot_loop(cx: &mut ScenarioContext<'_>) -> Verdict {
    if let Err(e) = cx.driver.start(Mode::Shadow) {
        return Verdict::Fail(format!("could not start: {e}"));
    }
    let supported = cx.driver.inject(Some(Fault::Unavailable));
    cx.driver.inject(None);
    if !supported || cx.driver.traffic().is_none() {
        return finish(
            cx,
            Verdict::Skip("the upstream cannot simulate an outage or count requests".into()),
        );
    }
    cx.driver.advance(cx.expect.warmup / 2);
    let before = cx.driver.traffic().unwrap_or_default();
    cx.driver.inject(Some(Fault::Unavailable));
    cx.driver.advance(cx.expect.outage);
    let after = cx.driver.traffic().unwrap_or_default();
    let Some(requests) = after.requests.checked_sub(before.requests) else {
        return finish(
            cx,
            Verdict::Fail(format!(
                "the upstream's request count went backwards ({} to {}) during the outage",
                before.requests, after.requests
            )),
        );
    };
    let minutes = (cx.expect.outage.as_secs_f64() / 60.0).max(1.0 / 60.0);
    let rate = requests as f64 / minutes;
    let v = if rate <= cx.expect.max_attempts_per_minute as f64 {
        Verdict::Pass
    } else {
        Verdict::Fail(format!(
            "{rate:.0} requests per minute during the outage (limit {})",
            cx.expect.max_attempts_per_minute
        ))
    };
    finish(cx, v)
}

fn fault_rejected_is_fatal(cx: &mut ScenarioContext<'_>) -> Verdict {
    if let Err(e) = cx.driver.start(Mode::Shadow) {
        return Verdict::Fail(format!("could not start: {e}"));
    }
    if !cx.driver.inject(Some(Fault::Rejected)) {
        return finish(
            cx,
            Verdict::Skip("the upstream cannot reject credentials".into()),
        );
    }
    cx.driver.inject(None);
    cx.driver.advance(cx.expect.warmup / 2);
    cx.driver.inject(Some(Fault::Rejected));
    cx.driver.advance(cx.expect.recovery);
    let s = cx.driver.stats();
    let mut p = Vec::new();
    if s.run_state != RunState::Down {
        p.push(format!("run state {:?}, expected down", s.run_state));
    }
    if s.restarts > 0 {
        p.push(format!("{} restarts before giving up", s.restarts));
    }
    finish(cx, fail(p))
}

fn fault_malformed(cx: &mut ScenarioContext<'_>) -> Verdict {
    if let Err(e) = cx.driver.start(Mode::Shadow) {
        return Verdict::Fail(format!("could not start: {e}"));
    }
    if !cx.driver.inject(Some(Fault::Malformed)) {
        return finish(
            cx,
            Verdict::Skip("the upstream cannot serve garbage".into()),
        );
    }
    cx.driver.inject(None);
    cx.driver.advance(cx.expect.warmup / 2);
    cx.driver.inject(Some(Fault::Malformed));
    cx.driver.advance(cx.expect.outage);
    let mut p: Vec<String> = cx
        .driver
        .failures()
        .into_iter()
        .filter(|f| f.error.contains("panicked"))
        .map(|f| format!("panicked at {}", f.time))
        .collect();
    let s = cx.driver.stats();
    if s.emit_errors > 0 {
        p.push(format!("{} records refused", s.emit_errors));
    }
    let instance = cx.driver.instance().to_owned();
    p.extend(checks::well_formed(
        &instance,
        &cx.driver.records(),
        Timestamp::MIN,
        Timestamp::MAX,
        cx.expect.forecast_horizon,
    ));
    finish(cx, fail(p))
}

// --- report -----------------------------------------------------------------

/// How a scenario ended, after expected failures are taken into account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Status {
    /// Passed.
    Passed,
    /// Passed what could be checked; the reason says what could not be
    /// observed with this fixture.
    PassedWithNote,
    /// Failed.
    Failed,
    /// Did not apply.
    Skipped,
    /// Failed, as declared.
    ExpectedFailure,
    /// Declared to fail, but passed: remove the declaration.
    UnexpectedPass,
    /// Not run: excluded by [`ONLY_ENV`](crate::ONLY_ENV).
    Filtered,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Passed => "pass",
            Self::PassedWithNote => "note",
            Self::Failed => "FAIL",
            Self::Skipped => "skip",
            Self::ExpectedFailure => "xfail",
            Self::UnexpectedPass => "XPASS",
            Self::Filtered => "-",
        }
    }
}

/// One scenario's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ScenarioResult {
    /// Scenario id.
    pub id: String,
    /// What it checks.
    pub summary: String,
    /// Outcome.
    pub status: Status,
    /// Why it failed or was skipped, or what was not observed.
    pub reason: Option<String>,
    /// The author's declaration for this scenario, if any.
    pub declared: Option<Declaration>,
    /// Why this result fails the suite: an undeclared skip, note or
    /// failure, or a declaration that did not happen.
    pub problem: Option<String>,
}

/// Every scenario's result for one adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Report {
    /// Adapter kind.
    pub kind: String,
    /// Results in run order.
    pub results: Vec<ScenarioResult>,
    /// Problems with the run itself: an unknown id in
    /// [`ONLY_ENV`](crate::ONLY_ENV) or in a declaration, or conflicting
    /// declarations.
    pub errors: Vec<String>,
}

impl Report {
    /// Whether the run is clean: no run errors, and every scenario either
    /// passed, was filtered out, or ended exactly as declared (a declared
    /// skip skipped, a declared note noted, a declared failure failed).
    /// Undeclared skips, notes and failures, and declarations that did not
    /// happen, all count against it.
    pub fn passed(&self) -> bool {
        self.problems().is_empty()
    }

    /// Everything that makes [`Report::passed`] false, one line each,
    /// naming the scenario id and the reason.
    pub fn problems(&self) -> Vec<String> {
        let mut out = self.errors.clone();
        out.extend(
            self.results
                .iter()
                .filter_map(|r| r.problem.as_ref().map(|p| format!("{}: {p}", r.id))),
        );
        out
    }

    /// How many scenarios ended with `status` (declared or not).
    pub fn count(&self, status: Status) -> usize {
        self.results.iter().filter(|r| r.status == status).count()
    }

    /// How many scenarios ended as declared.
    pub fn declared(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.declared.is_some() && r.problem.is_none() && r.status != Status::Filtered)
            .count()
    }

    /// The result for one scenario id.
    pub fn get(&self, id: &str) -> Option<&ScenarioResult> {
        self.results.iter().find(|r| r.id == id)
    }

    /// Panic unless [`Report::passed`], listing each problem, then the
    /// whole report.
    #[track_caller]
    pub fn assert_passed(&self) {
        let problems = self.problems();
        assert!(
            problems.is_empty(),
            "conformance of {} failed:\n  - {}\n\n{self}",
            self.kind,
            problems.join("\n  - ")
        );
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "conformance: {}", self.kind)?;
        for e in &self.errors {
            writeln!(f, "  ERROR {e}")?;
        }
        for r in &self.results {
            write!(f, "  {:<5} {:<30} {}", r.status.label(), r.id, r.summary)?;
            if let Some(reason) = &r.reason {
                write!(f, "\n        → {reason}")?;
            }
            if let Some(d) = &r.declared {
                write!(f, "\n        ({}: {})", d.outcome.describe(), d.why)?;
            }
            if let Some(p) = &r.problem {
                write!(f, "\n        ✗ {p}")?;
            }
            writeln!(f)?;
        }
        let count = |s| self.count(s);
        write!(
            f,
            "  {} passed, {} passed with notes, {} failed, {} skipped, {} expected failures, {} unexpected passes, {} filtered; {} as declared, {} problems",
            count(Status::Passed),
            count(Status::PassedWithNote),
            count(Status::Failed),
            count(Status::Skipped),
            count(Status::ExpectedFailure),
            count(Status::UnexpectedPass),
            count(Status::Filtered),
            self.declared(),
            self.problems().len()
        )
    }
}

/// Combine a verdict with the author's declaration. `filtered` scenarios
/// were not run; their declarations are neither enforced nor stale.
pub(crate) fn judge(
    scenario: &dyn Scenario,
    verdict: Option<Verdict>,
    expect: &Expectations,
) -> ScenarioResult {
    let declared = expect.declarations.get(scenario.id()).cloned();
    let (status, reason, problem) = match verdict {
        None => (Status::Filtered, None, None),
        Some(v) => classify(v, declared.as_ref()),
    };
    ScenarioResult {
        id: scenario.id().to_owned(),
        summary: scenario.summary().to_owned(),
        status,
        reason,
        declared,
        problem,
    }
}

fn classify(v: Verdict, d: Option<&Declaration>) -> (Status, Option<String>, Option<String>) {
    let outcome = d.map(|d| d.outcome);
    let mismatch =
        |what: &str| d.map(|d| format!("{} ({}) but {what}", d.outcome.describe(), d.why));
    match v {
        Verdict::Pass => match outcome {
            None => (Status::Passed, None, None),
            Some(Expected::Failure) => (
                Status::UnexpectedPass,
                None,
                mismatch("passed: remove the declaration"),
            ),
            Some(_) => (
                Status::Passed,
                None,
                mismatch("passed: remove the declaration"),
            ),
        },
        Verdict::PassWithNote(why) => match outcome {
            Some(Expected::Note) => (Status::PassedWithNote, Some(why), None),
            None => (
                Status::PassedWithNote,
                Some(why.clone()),
                Some(format!(
                    "undeclared note: {why} (declare it with Suite::expect_note, or extend the fixture)"
                )),
            ),
            Some(_) => {
                let p = mismatch(&format!("passed with a note: {why}"));
                (Status::PassedWithNote, Some(why), p)
            }
        },
        Verdict::Fail(why) => match outcome {
            Some(Expected::Failure) => (Status::ExpectedFailure, Some(why), None),
            None => (Status::Failed, Some(why.clone()), Some(why)),
            Some(_) => {
                let p = mismatch(&format!("failed: {why}"));
                (Status::Failed, Some(why), p)
            }
        },
        Verdict::Skip(why) => match outcome {
            Some(Expected::Skip) => (Status::Skipped, Some(why), None),
            None => (
                Status::Skipped,
                Some(why.clone()),
                Some(format!(
                    "undeclared skip: {why} (declare it with Suite::expect_skip, or extend the fixture)"
                )),
            ),
            Some(_) => {
                let p = mismatch(&format!("was skipped: {why}"));
                (Status::Skipped, Some(why), p)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_well_formed() {
        let mut ids: Vec<_> = catalogue().iter().map(|s| s.id()).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n);
        for id in ids {
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '.' || c == '_'),
                "{id}"
            );
        }
    }

    #[test]
    fn merge_sets_and_removes() {
        let base = json!({"a": 1, "b": 2});
        assert_eq!(
            merged(&base, &json!({"a": 3, "b": null, "c": 4})),
            json!({"a": 3, "c": 4})
        );
    }

    #[test]
    fn sample_commands_follow_capabilities() {
        let caps = Capabilities::read_write([CommandKind::FanMode, CommandKind::Switch])
            .with_fan_levels(1, 4);
        let cmds = sample_commands(&caps, "dev-1").unwrap();
        assert_eq!(cmds.len(), 2);
        assert!(sample_commands(&caps, "bad id").is_err());
    }
}
