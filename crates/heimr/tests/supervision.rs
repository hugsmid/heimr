//! Per-instance supervision on a fake clock: transient failures (errors,
//! panics, returning early) restart with backoff and jitter; fatal errors
//! and repeated panics keep the instance down while the app runs on;
//! commands queued for a dead instance are dropped and audited, never
//! replayed; provenance holds across several instances of one kind.

#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use heimr::core::{
    Adapter, AdapterContext, AdapterDescriptor, AdapterError, AdapterStats, AuditKind,
    Capabilities, ClimateCommand, ClimateMode, ClimateState, CommandKind, CommandRequest, DeviceId,
    EmitError, Event, HealthStatus, MemoryAuditLog, Metric, Record, Rejection, RunState,
    TokioClock, Unit, ZoneId,
};
use heimr::{App, Config, MemorySink};
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;

fn t0() -> Timestamp {
    "2030-01-01T00:00:00Z".parse().unwrap()
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("heimr-sup-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum How {
    Error,
    Panic,
    Return,
    Fatal,
    Spoof,
}

/// A test adapter that misbehaves on its first `failures` runs (counted in
/// its state directory, so the count survives rebuilds), after `run_for`.
/// Healthy runs report ok, emit one state per zone, log every command they
/// receive as an event, and wait for shutdown. With `hoard`, failing runs
/// never read their commands.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FlakyConfig {
    how: How,
    failures: u32,
    #[serde(default)]
    run_for: SignedDuration,
    #[serde(default)]
    hoard: bool,
}

struct Flaky;

impl Adapter for Flaky {
    type Config = FlakyConfig;

    fn descriptor() -> AdapterDescriptor {
        AdapterDescriptor::new(
            "flaky",
            "fails on purpose",
            Capabilities::read_write([CommandKind::ClimateTarget, CommandKind::ClimateSchedule]),
        )
    }

    fn new(_: &FlakyConfig) -> Result<Self, AdapterError> {
        Ok(Self)
    }

    async fn run(self, mut ctx: AdapterContext<FlakyConfig>) -> Result<(), AdapterError> {
        let counter = ctx.state_dir().join("runs");
        let run = std::fs::read_to_string(&counter)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0)
            + 1;
        std::fs::write(&counter, run.to_string()).map_err(AdapterError::transient)?;
        let (how, failures, run_for, hoard) = {
            let c = ctx.config();
            (c.how, c.failures, c.run_for, c.hoard)
        };
        let failing = run <= failures;
        if failing && how == How::Spoof {
            // Every attempt to speak for someone else is refused…
            let foreign = heimr::core::Reading::new(
                ctx.now(),
                "other",
                DeviceId::new("d").unwrap(),
                Metric::TEMPERATURE,
                1.0,
                Unit::Celsius,
            );
            assert!(matches!(
                ctx.emit(foreign).await,
                Err(EmitError::ForeignSource { .. })
            ));
            let impostor = Event::new(ctx.now(), "door_opened", "planner", serde_json::Value::Null);
            assert!(matches!(
                ctx.emit(impostor).await,
                Err(EmitError::ForeignActor { .. })
            ));
            // …and propagating the refusal is a fatal contract violation.
            ctx.emit(ctx.event(Event::COMMAND, serde_json::json!({"forged": true})))
                .await?;
            unreachable!("a reserved kind was accepted");
        }
        let mut commands = if failing && hoard {
            heimr::core::Commands::disconnected()
        } else {
            ctx.take_commands()
        };
        if failing {
            ctx.sleep(run_for).await;
            match how {
                How::Error => return Err(AdapterError::transient("device went away")),
                How::Panic => panic!("adapter bug"),
                How::Return => return Ok(()),
                How::Fatal => return Err(AdapterError::fatal("credentials rejected")),
                How::Spoof => unreachable!(),
            }
        }
        ctx.health().ok().await?;
        let zone = ZoneId::new("zone-a").unwrap();
        ctx.emit(ClimateState::new(zone, ctx.now(), ClimateMode::Schedule))
            .await?;
        loop {
            tokio::select! {
                () = ctx.shutdown().wait() => return Ok(()),
                Some(cmd) = commands.recv() => {
                    let ev = ctx.event("received", serde_json::json!({ "id": cmd.id, "run": run }));
                    ctx.emit(ev).await?;
                }
            }
        }
    }
}

struct Harness {
    dir: TempDir,
    sink: MemorySink,
    audit: MemoryAuditLog,
}

async fn run_for(
    toml: &str,
    minutes: u64,
    mid: impl AsyncFnOnce(&heimr::core::CommandSubmitter),
) -> (Harness, heimr::Telemetry) {
    let dir = TempDir::new("run");
    let config = Config::from_toml_str(&format!(
        "[heimr]\nstate_dir = \"{}\"\n[sinks]\nlogging = false\n[safety]\nmin_command_interval = \"0s\"\nmax_commands_per_hour = 100\n{toml}",
        dir.0.display()
    ))
    .unwrap();
    let sink = MemorySink::new(100_000);
    let audit = MemoryAuditLog::new();
    let app = App::builder(config)
        .public_adapters()
        .adapter::<Flaky>()
        .sink(sink.clone())
        .audit_log(Arc::new(audit.clone()))
        .clock(Arc::new(TokioClock::starting_at(t0())))
        .build()
        .await
        .unwrap();
    let telemetry = app.telemetry();
    let submitter = app.submitter("test").unwrap();
    let stop = app.shutdown_trigger();
    let running = tokio::spawn(app.run_until(std::future::pending()));
    let started = tokio::time::Instant::now();
    mid(&submitter).await;
    tokio::time::sleep_until(started + Duration::from_secs(minutes * 60)).await;
    stop.trigger();
    // Adapter failures never fail the app.
    running.await.unwrap().unwrap();
    (Harness { dir, sink, audit }, telemetry)
}

fn lifecycle(sink: &MemorySink, instance: &str) -> Vec<(String, Timestamp, serde_json::Value)> {
    sink.events()
        .into_iter()
        .filter(|e| e.actor == heimr::APP_SOURCE && e.payload["adapter"] == instance)
        .map(|e| (e.kind, e.time, e.payload["detail"].clone()))
        .collect()
}

fn starts(sink: &MemorySink, instance: &str) -> Vec<Timestamp> {
    lifecycle(sink, instance)
        .into_iter()
        .filter(|(k, ..)| k == Event::ADAPTER_STARTED)
        .map(|(_, t, _)| t)
        .collect()
}

fn assert_recovered(s: &AdapterStats, failures: u32) {
    assert_eq!(s.restarts, failures);
    assert_eq!(s.consecutive_failures, failures);
    assert_eq!(
        s.health,
        HealthStatus::Ok,
        "healthy again after the last restart"
    );
    assert_eq!(s.run_state, RunState::Stopped);
}

/// Restart delays with `backoff_initial = 1s`: the n-th lies in
/// [2^(n-1)/2, 2^(n-1)] seconds.
fn assert_backoff(starts: &[Timestamp], run_for: SignedDuration) {
    for (n, w) in starts.windows(2).enumerate() {
        let delay = w[1].duration_since(w[0]) - run_for;
        let base = SignedDuration::from_secs(1 << n);
        assert!(
            delay >= base / 2 && delay <= base,
            "restart {}: {delay:#} not in [{:#}, {base:#}]",
            n + 1,
            base / 2
        );
    }
}

#[tokio::test(start_paused = true)]
async fn transient_errors_restart_with_backoff() {
    let (h, t) = run_for(
        "[adapters.f]\ntype = \"flaky\"\nhow = \"error\"\nfailures = 3\nrun_for = \"2s\"",
        5,
        async |_| {},
    )
    .await;
    let s = t.get("f").unwrap();
    assert_recovered(&s, 3);
    let st = starts(&h.sink, "f");
    assert_eq!(st.len(), 4);
    assert_eq!(st[0], t0());
    assert_backoff(&st, SignedDuration::from_secs(2));
    let failed: Vec<_> = lifecycle(&h.sink, "f")
        .into_iter()
        .filter(|(k, ..)| k == Event::ADAPTER_FAILED)
        .map(|(_, _, d)| d)
        .collect();
    assert_eq!(failed.len(), 3);
    assert!(
        failed
            .iter()
            .all(|d| d["fatal"] == false && d["error"] == "device went away")
    );
    assert_eq!(failed[2]["failures"], 3);
}

#[tokio::test(start_paused = true)]
async fn panics_and_early_returns_also_restart() {
    for how in ["panic", "return"] {
        let (h, t) = run_for(
            &format!("[adapters.f]\ntype = \"flaky\"\nhow = \"{how}\"\nfailures = 2"),
            1,
            async |_| {},
        )
        .await;
        assert_recovered(&t.get("f").unwrap(), 2);
        assert_backoff(&starts(&h.sink, "f"), SignedDuration::ZERO);
    }
}

#[tokio::test(start_paused = true)]
async fn supervision_is_deterministic_under_the_fake_clock() {
    let toml = "[adapters.f]\ntype = \"flaky\"\nhow = \"error\"\nfailures = 6\n[adapters.g]\ntype = \"flaky\"\nhow = \"panic\"\nfailures = 4";
    let (a, _) = run_for(toml, 3, async |_| {}).await;
    let (b, _) = run_for(toml, 3, async |_| {}).await;
    for inst in ["f", "g"] {
        assert_eq!(starts(&a.sink, inst), starts(&b.sink, inst));
    }
    // Each instance has its own jitter sequence.
    assert_ne!(starts(&a.sink, "f")[1], starts(&a.sink, "g")[1]);
}

#[tokio::test(start_paused = true)]
async fn backoff_is_capped_and_resets_after_a_healthy_period() {
    let (h, t) = run_for(
        "[supervision]\nbackoff_max = \"8s\"\n[adapters.f]\ntype = \"flaky\"\nhow = \"error\"\nfailures = 8",
        5,
        async |_| {},
    )
    .await;
    let st = starts(&h.sink, "f");
    assert_eq!(st.len(), 9);
    for w in st.windows(2) {
        assert!(w[1].duration_since(w[0]) <= SignedDuration::from_secs(8));
    }
    assert_recovered(&t.get("f").unwrap(), 8);

    // Runs longer than reset_after count as healthy: every delay restarts
    // from backoff_initial.
    let (h, t) = run_for(
        "[supervision]\nreset_after = \"30s\"\n[adapters.f]\ntype = \"flaky\"\nhow = \"error\"\nfailures = 4\nrun_for = \"40s\"",
        5,
        async |_| {},
    )
    .await;
    let st = starts(&h.sink, "f");
    for w in st.windows(2) {
        let delay = w[1].duration_since(w[0]) - SignedDuration::from_secs(40);
        assert!(delay <= SignedDuration::from_secs(1), "{delay:#}");
    }
    let s = t.get("f").unwrap();
    assert_eq!((s.restarts, s.consecutive_failures), (4, 1));
}

#[tokio::test(start_paused = true)]
async fn fatal_errors_keep_the_instance_down_and_the_app_running() {
    let (h, t) = run_for(
        "[adapters.f]\ntype = \"flaky\"\nhow = \"fatal\"\nfailures = 99\n[adapters.demo]\ntype = \"demo\"\ninterval = \"3m\"",
        10,
        async |_| {},
    )
    .await;
    let s = t.get("f").unwrap();
    assert_eq!(s.run_state, RunState::Down);
    assert_eq!(s.restarts, 0);
    assert_eq!(s.health, HealthStatus::Failed);
    assert_eq!(s.exit_error.as_deref(), Some("credentials rejected"));
    assert_eq!(starts(&h.sink, "f").len(), 1);
    // The other instance ran throughout.
    let demo = t.get("demo").unwrap();
    assert_eq!(demo.run_state, RunState::Stopped);
    assert_eq!(demo.readings, 2 * 4); // ticks at 0, 3, 6 and 9 minutes
}

#[tokio::test(start_paused = true)]
async fn repeated_panics_keep_the_instance_down_and_the_app_running() {
    // Default limit: five panics in a row.
    let (h, t) = run_for(
        "[adapters.f]\ntype = \"flaky\"\nhow = \"panic\"\nfailures = 99\n[adapters.demo]\ntype = \"demo\"\ninterval = \"3m\"",
        10,
        async |_| {},
    )
    .await;
    let s = t.get("f").unwrap();
    assert_eq!(s.run_state, RunState::Down);
    assert_eq!(s.health, HealthStatus::Failed);
    assert_eq!(s.restarts, 4);
    assert_eq!(s.consecutive_failures, 5);
    assert_eq!(s.next_restart_at, None);
    assert_eq!(
        s.exit_error.as_deref(),
        Some("panicked 5 times without a healthy run; kept down")
    );
    assert_eq!(starts(&h.sink, "f").len(), 5);
    let failed: Vec<_> = lifecycle(&h.sink, "f")
        .into_iter()
        .filter(|(k, ..)| k == Event::ADAPTER_FAILED)
        .map(|(_, _, d)| d)
        .collect();
    assert_eq!(failed.len(), 5);
    assert!(failed[..4].iter().all(|d| d["fatal"] == false));
    assert_eq!(failed[4]["fatal"], true);
    assert_eq!(failed[4]["failures"], 5);
    // The other instance ran throughout.
    let demo = t.get("demo").unwrap();
    assert_eq!(demo.run_state, RunState::Stopped);
    assert_eq!(demo.readings, 2 * 4);

    // The limit is configurable.
    let (h, t) = run_for(
        "[supervision]\nmax_consecutive_panics = 2\n[adapters.f]\ntype = \"flaky\"\nhow = \"panic\"\nfailures = 99",
        5,
        async |_| {},
    )
    .await;
    assert_eq!(t.get("f").unwrap().run_state, RunState::Down);
    assert_eq!(starts(&h.sink, "f").len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_healthy_period_resets_the_panic_count() {
    // Each run lasts longer than reset_after before panicking, so no two
    // panics are consecutive: seven panics, limit three, still restarted.
    let (h, t) = run_for(
        "[supervision]\nreset_after = \"30s\"\nmax_consecutive_panics = 3\n[adapters.f]\ntype = \"flaky\"\nhow = \"panic\"\nfailures = 7\nrun_for = \"40s\"",
        10,
        async |_| {},
    )
    .await;
    let s = t.get("f").unwrap();
    assert_eq!(starts(&h.sink, "f").len(), 8);
    assert_eq!(s.restarts, 7);
    assert_eq!(s.health, HealthStatus::Ok);
    assert_eq!(s.run_state, RunState::Stopped);
}

#[tokio::test(start_paused = true)]
async fn commands_for_a_dead_instance_are_dropped_not_replayed() {
    let (h, t) = run_for(
        "[adapters.f]\ntype = \"flaky\"\nmode = \"live\"\nhow = \"error\"\nfailures = 1\nrun_for = \"10s\"\nhoard = true",
        1,
        async |planner| {
            let cmd = |z: &str| {
                CommandRequest::new(
                    "f",
                    ClimateCommand::resume_schedule(ZoneId::new(z).unwrap()),
                    "r",
                )
            };
            tokio::time::sleep(Duration::from_secs(5)).await;
            // Accepted and queued, but the first run never reads them.
            assert!(planner.submit(cmd("a")).await.is_ok());
            assert!(planner.submit(cmd("b")).await.is_ok());
            // During backoff the instance is not running: refused.
            tokio::time::sleep(Duration::from_millis(5_100)).await;
            assert!(matches!(planner.submit(cmd("c")).await, Err(Rejection::AdapterGone { .. })));
            // After the restart, new commands arrive.
            tokio::time::sleep(Duration::from_secs(5)).await;
            assert!(planner.submit(cmd("d")).await.is_ok());
        },
    )
    .await;
    let records = h.audit.records();
    let dropped: Vec<_> = records
        .iter()
        .filter(|r| r.kind == AuditKind::Dropped)
        .collect();
    assert_eq!(dropped.len(), 2);
    assert_eq!(
        dropped
            .iter()
            .map(|r| r.target.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    // The second run received only `d`.
    let received: Vec<_> = h
        .sink
        .events()
        .into_iter()
        .filter(|e| e.kind == "received")
        .map(|e| e.payload)
        .collect();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0]["run"], 2);
    let d = records.iter().find(|r| r.target == "d").unwrap();
    assert_eq!(received[0]["id"], d.id.to_string());
    let s = t.get("f").unwrap();
    assert_eq!(
        (s.commands_delivered, s.commands_dropped, s.commands_failed),
        (3, 2, 1)
    );
    let _ = h.dir;
}

#[tokio::test(start_paused = true)]
async fn provenance_holds_across_instances_of_one_kind() {
    let (h, t) = run_for(
        "[adapters.liar]\ntype = \"flaky\"\nhow = \"spoof\"\nfailures = 99\n[adapters.f1]\ntype = \"flaky\"\nhow = \"error\"\nfailures = 0\n[adapters.f2]\ntype = \"flaky\"\nhow = \"error\"\nfailures = 0",
        1,
        async |_| {},
    )
    .await;
    let liar = t.get("liar").unwrap();
    assert_eq!(liar.provenance_rejections, 3);
    assert_eq!(
        liar.run_state,
        RunState::Down,
        "a contract violation is fatal"
    );
    // Nothing forged reached a sink.
    for env in h.sink.snapshot() {
        match &env.record {
            Record::Event(e) if e.kind == Event::COMMAND => panic!("forged command event: {env:?}"),
            Record::Event(e)
                if env.source != heimr::APP_SOURCE && env.source != heimr::ACTUATOR_SOURCE =>
            {
                assert_eq!(e.actor, env.source);
            }
            Record::Reading(r) => assert_eq!(r.source, env.source),
            _ => {}
        }
    }
    // Two instances of one kind report the same zone id; their states are
    // kept apart by source.
    let states = h.sink.latest_states();
    let sources: Vec<_> = states
        .keys()
        .map(|k| (k.source.as_str(), k.id.as_str()))
        .collect();
    assert_eq!(sources, [("f1", "zone-a"), ("f2", "zone-a")]);
}
