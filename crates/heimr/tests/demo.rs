//! End to end: the app runs the demo adapter on a fake clock; shadow mode
//! blocks command delivery, live mode delivers; the journal holds every
//! decision; holds end overrides; telemetry counts it all.

#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use heimr::core::{
    AdapterStats, AuditRecord, AuditResult, ClimateCommand, ClimateMode, CommandRequest, Event,
    HealthStatus, Hold, Metric, Mode, Outcome, Record, Rejection, RunState, Setpoint, TokioClock,
    Unit, ZoneId,
};
use heimr::{App, Config, MemorySink};
use jiff::{SignedDuration, Timestamp, ToSpan};

fn t0() -> Timestamp {
    "2030-01-01T00:00:00Z".parse().unwrap()
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("heimr-test-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Run {
    sink: MemorySink,
    /// Outcome of the first setpoint.
    outcome: Result<Outcome, Rejection>,
    stats: AdapterStats,
    journal: Vec<AuditRecord>,
}

fn set_target(zone: &str, c: f64) -> ClimateCommand {
    ClimateCommand::set_target(
        ZoneId::new(zone).unwrap(),
        Setpoint::new(c).unwrap(),
        Hold::NextScheduleChange,
    )
}

fn journal(dir: &std::path::Path) -> Vec<AuditRecord> {
    let mut out = vec![];
    let mut files: Vec<_> = std::fs::read_dir(dir.join("audit"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    for f in files {
        for line in std::fs::read_to_string(f).unwrap().lines() {
            out.push(serde_json::from_str(line).unwrap());
        }
    }
    out
}

/// Run the demo for 35½ minutes with commands at 5:30 (zone-a → 21.5 °C),
/// 15:30 (an unknown zone), 25:30 (zone-a again) and 25:40 (too soon:
/// rate limited).
async fn run_demo(mode: Mode) -> Run {
    let dir = TempDir::new(mode.as_str());
    let config = Config::from_toml_str(&format!(
        r#"
        [site]
        time_zone = "Europe/Berlin"
        currency = "EUR"
        [heimr]
        state_dir = "{}"
        [sinks]
        logging = false
        [adapters.demo]
        type = "demo"
        mode = "{}"
        interval = "1m"
        zones = ["zone-a"]
        initial_target = 19.0
        "#,
        dir.0.display(),
        mode.as_str()
    ))
    .unwrap();
    // ANCHOR: app-under-test
    let sink = MemorySink::new(100_000);
    let app = App::builder(config)
        .public_adapters()
        .sink(sink.clone())
        .clock(Arc::new(TokioClock::starting_at(t0())))
        .build()
        .await
        .unwrap();
    assert_eq!(app.adapters(), [("demo".to_owned(), mode)]);
    let planner = app.submitter("planner").unwrap();
    let telemetry = app.telemetry();
    let stop = app.shutdown_trigger();
    let running = tokio::spawn(app.run_until(std::future::pending()));
    // ANCHOR_END: app-under-test

    let submit = |cmd, reason| planner.submit(CommandRequest::new("demo", cmd, reason));
    let mins = |m: u64| tokio::time::sleep(Duration::from_secs(m * 60));

    // Within the start-up hold-off nothing but "resume schedule" passes.
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert!(matches!(
        submit(set_target("zone-a", 21.5), "too early").await,
        Err(Rejection::RateLimited { .. })
    ));
    tokio::time::sleep(Duration::from_secs(4 * 60 + 30)).await;
    let outcome = planner
        .submit(
            CommandRequest::new("demo", set_target("zone-a", 21.5), "warm up zone-a")
                .with_inputs(serde_json::json!({ "measured_c": 17.3 })),
        )
        .await;
    mins(10).await;
    assert!(submit(set_target("zone-z", 20.0), "typo").await.is_ok());
    mins(10).await;
    assert!(submit(set_target("zone-a", 21.5), "again").await.is_ok());
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert!(matches!(
        submit(set_target("zone-a", 21.0), "too soon").await,
        Err(Rejection::RateLimited { .. })
    ));
    tokio::time::sleep(Duration::from_secs(10 * 60 - 10)).await;
    stop.trigger();
    running.await.unwrap().unwrap();
    assert!(
        dir.0.join("adapters/demo").is_dir(),
        "state directory was created"
    );
    let stats = telemetry.get("demo").unwrap();
    assert_eq!(telemetry.snapshot(), std::slice::from_ref(&stats));
    Run {
        sink,
        outcome,
        stats,
        journal: journal(&dir.0),
    }
}

/// Stats both modes share: counts on the fake clock, run times, metrics.
fn assert_common_stats(s: &AdapterStats, mode: Mode) {
    assert_eq!(s.name, "demo");
    assert_eq!(s.kind, "demo");
    assert_eq!(s.mode, mode);
    assert_eq!(s.run_state, RunState::Stopped);
    assert_eq!(s.started_at, Some(t0()));
    assert_eq!(
        s.exited_at,
        Some(t0() + SignedDuration::from_secs(35 * 60 + 30))
    );
    assert_eq!(s.exit_error, None);
    assert_eq!(s.restarts, 0);
    assert_eq!(s.readings, 2 * 36);
    assert_eq!(s.emit_errors, 0);
    assert_eq!(s.provenance_rejections, 0);
    assert_eq!(s.last_emit_at, Some(t0() + 35.minutes()));
    assert_eq!(s.commands_offered, 5);
    assert_eq!(s.commands_rejected_rate_limit, 2);
    assert_eq!(s.commands_rejected_policy, 0);
    assert_eq!(
        (
            s.commands_failed,
            s.commands_dropped,
            s.commands_audit_failed
        ),
        (0, 0, 0)
    );
    assert_eq!(s.counters.get("ticks"), Some(&36));
    // Plain data: round-trips through JSON.
    let json = serde_json::to_value(s).unwrap();
    assert_eq!(serde_json::from_value::<AdapterStats>(json).unwrap(), *s);
}

/// The journal holds every decision, in order, with unique ids and holds.
fn assert_journal(run: &Run, accepted: &AuditResult) {
    let j = &run.journal;
    assert_eq!(
        j.len(),
        5,
        "every command is in the journal, rejected ones too"
    );
    let results: Vec<_> = j.iter().map(|r| &r.result).collect();
    assert!(matches!(results[0], AuditResult::Rejected { .. }));
    assert_eq!(results[1], accepted);
    assert_eq!(results[2], accepted);
    assert_eq!(results[3], accepted);
    assert!(matches!(results[4], AuditResult::Rejected { .. }));
    assert!(j.windows(2).all(|w| w[0].id < w[1].id));
    assert_eq!(j[1].id, run.outcome.as_ref().unwrap().id());
    assert!(j.iter().all(|r| r.hold == Some(Hold::NextScheduleChange)));
    assert_eq!(j[1].reason, "warm up zone-a");
    assert_eq!(j[1].inputs["measured_c"], 17.3);
}

fn health_events(sink: &MemorySink) -> Vec<serde_json::Value> {
    sink.events()
        .into_iter()
        .filter(|e| e.kind == Event::ADAPTER_HEALTH)
        .map(|e| {
            assert_eq!(e.actor, "demo");
            e.payload
        })
        .collect()
}

fn temperatures(sink: &MemorySink) -> Vec<(Timestamp, f64)> {
    sink.readings()
        .into_iter()
        .filter(|r| r.metric == Metric::TEMPERATURE)
        .map(|r| (r.time, r.value))
        .collect()
}

fn assert_readings_plausible(sink: &MemorySink) {
    let readings = sink.readings();
    for r in &readings {
        assert_eq!(r.source, "demo");
        assert_eq!(r.device.as_str(), "zone-a");
        match r.metric.as_str() {
            "temperature" => {
                assert_eq!(r.unit, Unit::Celsius);
                assert!((5.0..=30.0).contains(&r.value), "{r:?}");
            }
            "humidity" => {
                assert_eq!(r.unit, Unit::Percent);
                assert!((20.0..=80.0).contains(&r.value), "{r:?}");
            }
            other => panic!("unexpected metric {other}"),
        }
    }
    // One sample per metric per minute, on the fake clock, from t0.
    let temps = temperatures(sink);
    let times: Vec<_> = temps.iter().map(|(t, _)| *t).collect();
    let expected: Vec<_> = (0..=35).map(|m| t0() + m.minutes()).collect();
    assert_eq!(times, expected);
    let hum = readings
        .iter()
        .filter(|r| r.metric == Metric::HUMIDITY)
        .count();
    assert_eq!(hum, expected.len());
}

fn command_events(sink: &MemorySink) -> Vec<Event> {
    sink.snapshot()
        .into_iter()
        .filter(|e| e.source == heimr::ACTUATOR_SOURCE)
        .filter_map(|e| match e.record {
            Record::Event(ev) if ev.kind == Event::COMMAND => Some(ev),
            _ => None,
        })
        .collect()
}

fn manual_states(sink: &MemorySink) -> usize {
    sink.snapshot()
        .iter()
        .filter(|e| matches!(&e.record, Record::Climate(s) if s.mode == ClimateMode::Manual))
        .count()
}

#[tokio::test(start_paused = true)]
async fn shadow_mode_audits_but_never_delivers() {
    let run = run_demo(Mode::Shadow).await;
    assert!(matches!(run.outcome, Ok(Outcome::Shadowed { .. })));
    assert_readings_plausible(&run.sink);
    assert_journal(&run, &AuditResult::Shadowed);

    // Sinks get a copy of every audit record.
    let events = command_events(&run.sink);
    assert_eq!(events.len(), 5);
    let p = &events[1].payload;
    assert_eq!(events[1].actor, "planner");
    assert_eq!(events[1].time, t0() + SignedDuration::from_secs(330));
    assert_eq!(p["mode"], "shadow");
    assert_eq!(p["reason"], "warm up zone-a");
    assert_eq!(p["hold"], "next_schedule_change");
    assert_eq!(p["result"]["outcome"], "shadowed");

    // The adapter never saw it: the zone stays on its schedule at 19 °C.
    assert_eq!(manual_states(&run.sink), 0);
    let last = temperatures(&run.sink).last().unwrap().1;
    assert!(
        last < 19.0,
        "shadow zone should stay below its 19 °C target, got {last}"
    );

    let s = &run.stats;
    assert_common_stats(s, Mode::Shadow);
    assert_eq!((s.commands_shadowed, s.commands_delivered), (3, 0));
    assert_eq!(
        s.states, 36,
        "no command reached the adapter, so no extra state"
    );
    // Healthy from the start; the unknown-zone command never arrived.
    assert_eq!(s.health, HealthStatus::Ok);
    assert_eq!(s.health_since, Some(t0()));
    assert_eq!(health_events(&run.sink).len(), 1);
    assert_eq!(s.events, 1);

    let lifecycle: Vec<_> = run
        .sink
        .events()
        .into_iter()
        .filter(|e| e.actor == heimr::APP_SOURCE)
        .map(|e| e.kind)
        .collect();
    assert_eq!(lifecycle, [Event::ADAPTER_STARTED, Event::ADAPTER_STOPPED]);
}

#[tokio::test(start_paused = true)]
async fn live_mode_delivers() {
    let shadow = run_demo(Mode::Shadow).await;
    let run = run_demo(Mode::Live).await;
    assert!(matches!(run.outcome, Ok(Outcome::Delivered { .. })));
    assert_readings_plausible(&run.sink);
    assert_journal(&run, &AuditResult::Delivered);
    assert_eq!(
        command_events(&run.sink)[1].payload["result"]["outcome"],
        "delivered"
    );

    // The adapter applied it at once: a manual state targeting 21.5 °C at
    // the submission time, then on every tick.
    let first_manual = run
        .sink
        .snapshot()
        .into_iter()
        .find_map(|e| match e.record {
            Record::Climate(s) if s.mode == ClimateMode::Manual => Some(s),
            _ => None,
        })
        .unwrap();
    assert_eq!(first_manual.time, t0() + SignedDuration::from_secs(330));
    assert_eq!(first_manual.target_c, Some(21.5));
    assert_eq!(manual_states(&run.sink), 2 + 30);

    let s = &run.stats;
    assert_common_stats(s, Mode::Live);
    assert_eq!((s.commands_shadowed, s.commands_delivered), (0, 3));
    assert_eq!(s.states, 36 + 2);
    // ok → degraded (unknown zone) → ok (next good command).
    let health: Vec<_> = health_events(&run.sink)
        .iter()
        .map(|p| p["status"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(health, ["ok", "degraded", "ok"]);
    assert_eq!(s.events, 3);
    assert_eq!(s.health, HealthStatus::Ok);
    assert_eq!(s.health_reason, None);
    assert_eq!(
        s.health_since,
        Some(t0() + SignedDuration::from_secs(25 * 60 + 30))
    );
    let degraded = &health_events(&run.sink)[1];
    let typo_id = run.journal[2].id;
    assert_eq!(
        degraded["reason"],
        format!("command {typo_id} names unknown zone zone-z")
    );
    assert_eq!(degraded["previous"], "ok");

    // Same seed and clock: identical until the command, then warmer.
    let (s, l) = (temperatures(&shadow.sink), temperatures(&run.sink));
    assert_eq!(s[..6], l[..6]);
    let (s_last, l_last) = (s.last().unwrap().1, l.last().unwrap().1);
    assert!(l_last > s_last + 1.0, "live {l_last} vs shadow {s_last}");
}

/// The device's own timer ends a timed hold; heimr does not have to.
#[tokio::test(start_paused = true)]
async fn timed_hold_expires_on_the_device() {
    let dir = TempDir::new("hold");
    let config = Config::from_toml_str(&format!(
        r#"
        [site]
        time_zone = "Europe/Berlin"
        currency = "EUR"
        [heimr]
        state_dir = "{}"
        [sinks]
        logging = false
        [safety]
        min_command_interval = "0s"
        [adapters.demo]
        type = "demo"
        mode = "live"
        zones = ["zone-a"]
        "#,
        dir.0.display(),
    ))
    .unwrap();
    let sink = MemorySink::new(10_000);
    let app = App::builder(config)
        .public_adapters()
        .sink(sink.clone())
        .clock(Arc::new(TokioClock::starting_at(t0())))
        .build()
        .await
        .unwrap();
    let planner = app.submitter("planner").unwrap();
    let stop = app.shutdown_trigger();
    let running = tokio::spawn(app.run_until(std::future::pending()));
    tokio::time::sleep(Duration::from_secs(30)).await;
    // ANCHOR: submit
    let target = planner.policy_for("demo").unwrap().setpoint(21.0).unwrap();
    let hold = Hold::for_duration(SignedDuration::from_mins(10)).unwrap();
    let cmd = ClimateCommand::set_target(ZoneId::new("zone-a").unwrap(), target, hold);
    let request = CommandRequest::new("demo", cmd, "boost before the cheap slot ends");
    let outcome = planner.submit(request).await;
    // Live: Ok(Outcome::Delivered { id }); shadow: Ok(Outcome::Shadowed { id });
    // refused (capability, band, hold, rate limit, …): Err(Rejection::…).
    assert!(matches!(outcome, Ok(Outcome::Delivered { .. })));
    // ANCHOR_END: submit
    // A hold beyond the policy's maximum (2h) is refused.
    let long = Hold::for_duration(SignedDuration::from_hours(3)).unwrap();
    let cmd = ClimateCommand::set_target(
        ZoneId::new("zone-a").unwrap(),
        Setpoint::new(21.0).unwrap(),
        long,
    );
    assert!(matches!(
        planner
            .submit(CommandRequest::new("demo", cmd, "too long"))
            .await,
        Err(Rejection::InvalidHold { .. })
    ));
    tokio::time::sleep(Duration::from_secs(20 * 60)).await;
    stop.trigger();
    running.await.unwrap().unwrap();
    let modes: Vec<(Timestamp, ClimateMode)> = sink
        .snapshot()
        .into_iter()
        .filter_map(|e| match e.record {
            Record::Climate(s) => Some((s.time, s.mode)),
            _ => None,
        })
        .collect();
    let manual: Vec<_> = modes
        .iter()
        .filter(|(_, m)| *m == ClimateMode::Manual)
        .collect();
    assert_eq!(
        manual.first().unwrap().0,
        t0() + SignedDuration::from_secs(30)
    );
    // Back on schedule from the first tick at or after 10:30.
    assert!(
        manual
            .iter()
            .all(|(t, _)| *t < t0() + SignedDuration::from_secs(10 * 60 + 30))
    );
    assert_eq!(modes.last().unwrap().1, ClimateMode::Schedule);
}
