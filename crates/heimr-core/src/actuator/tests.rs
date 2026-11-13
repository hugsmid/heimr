use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use jiff::{SignedDuration, Timestamp};
use proptest::prelude::*;
use tokio::sync::mpsc;

use super::*;
use crate::audit::{AuditTail, MemoryAuditLog, UnreadableEntry};
use crate::clock::{BoxFuture, Clock, ManualClock};
use crate::context::Commands;
use crate::domain::{FanCommand, FanMode, HoldDuration, SwitchCommand};
use crate::id::ZoneId;
use crate::record::{Envelope, Event, Record};

fn t0() -> Timestamp {
    "2030-01-01T00:00:00Z".parse().unwrap()
}

fn all_caps() -> Capabilities {
    Capabilities::read_write(CommandKind::ALL.iter().copied())
        .with_fan_levels(1, 5)
        .with_fan_presets(["night"])
}

fn zone(z: &str) -> ZoneId {
    ZoneId::new(z).unwrap()
}

fn dev(d: &str) -> DeviceId {
    DeviceId::new(d).unwrap()
}

fn set_target(z: &str, c: f64) -> Command {
    ClimateCommand::set_target(zone(z), Setpoint::new(c).unwrap(), Hold::NextScheduleChange).into()
}

fn resume(z: &str) -> Command {
    ClimateCommand::resume_schedule(zone(z)).into()
}

fn req(adapter: &str, cmd: Command) -> CommandRequest {
    CommandRequest::new(adapter, cmd, "because")
}

struct Rig {
    act: Actuator,
    clock: ManualClock,
    log: MemoryAuditLog,
    sink: mpsc::Receiver<Envelope>,
}

async fn rig_with(
    policy: SafetyPolicy,
    log: Arc<dyn AuditLog>,
    clock: ManualClock,
    holdoff: bool,
) -> (Actuator, mpsc::Receiver<Envelope>) {
    let (tx, rx) = mpsc::channel(10_000);
    let mut b = Actuator::builder(policy, Arc::new(clock), log).events(Emitter::core(ACTUATOR, tx));
    if !holdoff {
        b = b.without_boot_holdoff();
    }
    let act = b.build().await.unwrap();
    (act, rx)
}

async fn rig(policy: SafetyPolicy) -> Rig {
    let log = MemoryAuditLog::new();
    let clock = ManualClock::new(t0());
    let (act, sink) = rig_with(policy, Arc::new(log.clone()), clock.clone(), false).await;
    Rig {
        act,
        clock,
        log,
        sink,
    }
}

fn register(act: &Actuator, name: &str, mode: Mode) {
    act.register(InstanceSpec::new(name, "test", all_caps(), mode))
        .unwrap();
}

async fn live(act: &Actuator, name: &str) -> Commands {
    register(act, name, Mode::Live);
    act.attach(name).await.unwrap()
}

async fn nothing_arrives(cmds: &mut Commands) -> bool {
    tokio::time::timeout(std::time::Duration::from_millis(20), cmds.recv())
        .await
        .is_err()
}

fn sink_events(rx: &mut mpsc::Receiver<Envelope>) -> Vec<Event> {
    let mut out = vec![];
    while let Ok(env) = rx.try_recv() {
        assert_eq!(env.source, ACTUATOR);
        let Record::Event(e) = env.record else {
            panic!("not an event")
        };
        out.push(e);
    }
    out
}

#[tokio::test]
async fn shadow_audits_but_never_delivers() {
    let mut r = rig(SafetyPolicy::default()).await;
    register(&r.act, "heat", Mode::Shadow);
    let mut cmds = r.act.attach("heat").await.unwrap();
    assert!(!cmds.is_live());
    let out = r
        .act
        .submit_as(
            "test",
            req("heat", set_target("zone-a", 20.0)).with_inputs(serde_json::json!({"t": 18.2})),
        )
        .await
        .unwrap();
    assert!(matches!(out, Outcome::Shadowed { .. }));
    assert!(
        nothing_arrives(&mut cmds).await,
        "a shadow adapter received a command"
    );
    let log = r.log.records();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].id, out.id());
    assert_eq!(log[0].result, AuditResult::Shadowed);
    assert_eq!(log[0].hold, Some(Hold::NextScheduleChange));
    let events = sink_events(&mut r.sink);
    assert_eq!(events.len(), 1);
    let p = &events[0].payload;
    assert_eq!(events[0].kind, Event::COMMAND);
    assert_eq!(events[0].actor, "test");
    assert_eq!(p["mode"], "shadow");
    assert_eq!(p["reason"], "because");
    assert_eq!(p["inputs"]["t"], 18.2);
    assert_eq!(p["hold"], "next_schedule_change");
    assert_eq!(p["result"]["outcome"], "shadowed");
}

#[tokio::test]
async fn live_delivers_after_durable_audit() {
    let r = rig(SafetyPolicy::default()).await;
    let mut cmds = live(&r.act, "heat").await;
    let out = r
        .act
        .submit_as("test", req("heat", set_target("zone-a", 20.0)))
        .await
        .unwrap();
    assert!(matches!(out, Outcome::Delivered { .. }));
    let got = cmds.recv().await.unwrap();
    assert_eq!(got.id, out.id());
    assert_eq!(got.command, set_target("zone-a", 20.0));
    assert_eq!(r.log.records()[0].result, AuditResult::Delivered);
}

#[tokio::test]
async fn read_only_cannot_be_live_and_is_never_capable() {
    let r = rig(SafetyPolicy::default()).await;
    let ro = |mode| InstanceSpec::new("ro", "test", Capabilities::read_only(), mode);
    assert_eq!(
        r.act.register(ro(Mode::Live)).unwrap_err(),
        RegisterError::LiveButReadOnly("ro".into())
    );
    r.act.register(ro(Mode::Shadow)).unwrap();
    assert!(!r.act.attach("ro").await.unwrap().is_live());
    let out = r
        .act
        .submit_as("test", req("ro", set_target("z", 20.0)))
        .await;
    assert!(matches!(out, Err(Rejection::NotCapable { .. })));
    assert!(r.act.register(ro(Mode::Shadow)).is_err());
    assert!(
        r.act
            .register(InstanceSpec::new(
                "Bad Name",
                "t",
                Capabilities::read_only(),
                Mode::Shadow
            ))
            .is_err()
    );
}

#[tokio::test]
async fn policy_rejections_are_audited() {
    let policy = SafetyPolicy::default().with_setpoint(SetpointBand::new(17.0, 21.0).unwrap());
    let r = rig(policy).await;
    let safety = InstanceSafety::new().with_heating_critical(dev("boiler"));
    r.act
        .register(InstanceSpec::new("heat", "test", all_caps(), Mode::Live).with_safety(safety))
        .unwrap();
    let mut cmds = r.act.attach("heat").await.unwrap();
    let submit = |cmd: Command| r.act.submit_as("test", req("heat", cmd));

    let too_hot = submit(set_target("z", 22.0)).await;
    assert!(matches!(too_hot, Err(Rejection::OutsideBand { .. })));
    // The named escape builds the value, the actuator still refuses it.
    let escaped = ClimateCommand::set_target(
        zone("y"),
        Setpoint::envelope_only(28.0).unwrap(),
        Hold::NextScheduleChange,
    );
    assert!(matches!(
        submit(escaped.into()).await,
        Err(Rejection::OutsideBand { .. })
    ));
    let off = submit(ClimateCommand::turn_off(zone("x"), Hold::NextScheduleChange).into()).await;
    assert_eq!(off, Err(Rejection::HeatingOffNotAllowed));
    let boiler_off = submit(SwitchCommand::set(dev("boiler"), false).into()).await;
    assert_eq!(boiler_off, Err(Rejection::HeatingOffNotAllowed));
    let long = Hold::for_duration(SignedDuration::from_hours(3)).unwrap();
    let long_hold =
        submit(ClimateCommand::set_target(zone("w"), Setpoint::new(19.0).unwrap(), long).into())
            .await;
    assert!(matches!(long_hold, Err(Rejection::InvalidHold { .. })));
    // A non-positive or over-long hold cannot even be built.
    assert!(HoldDuration::new(SignedDuration::from_mins(-1)).is_err());
    assert!(HoldDuration::new(SignedDuration::from_hours(25)).is_err());
    let level = submit(
        FanCommand::set_mode(dev("fan"), FanMode::Manual(6), Hold::NextScheduleChange).into(),
    )
    .await;
    assert!(matches!(level, Err(Rejection::UnsupportedFanMode { .. })));
    let preset = submit(
        FanCommand::set_mode(
            dev("fan"),
            FanMode::preset("turbo"),
            Hold::NextScheduleChange,
        )
        .into(),
    )
    .await;
    assert!(matches!(preset, Err(Rejection::UnsupportedFanMode { .. })));
    let unknown = r
        .act
        .submit_as("test", req("nope", set_target("z", 20.0)))
        .await;
    assert!(matches!(unknown, Err(Rejection::UnknownAdapter { .. })));

    let log = r.log.records();
    assert_eq!(log.len(), 8);
    assert!(
        log.iter()
            .all(|rec| matches!(rec.result, AuditResult::Rejected { .. }))
    );
    assert!(nothing_arrives(&mut cmds).await);

    // Allowed: switching a non-critical device off, or the boiler on.
    assert!(
        submit(SwitchCommand::set(dev("lamp"), false).into())
            .await
            .is_ok()
    );
    assert!(
        submit(SwitchCommand::set(dev("boiler"), true).into())
            .await
            .is_ok()
    );
    let s = r.act.stats("heat").unwrap().snapshot();
    assert_eq!(s.commands_rejected_policy, 7);
}

#[tokio::test]
async fn heating_off_allowed_only_by_policy() {
    let r = rig(SafetyPolicy::default().with_heating_off(true)).await;
    let safety = InstanceSafety::new().with_heating_critical(dev("boiler"));
    r.act
        .register(
            InstanceSpec::new("heat", "test", all_caps(), Mode::Shadow).with_safety(safety.clone()),
        )
        .unwrap();
    assert!(
        r.act
            .submit_as(
                "test",
                req("heat", SwitchCommand::set(dev("boiler"), false).into())
            )
            .await
            .is_ok()
    );
    // One instance may forbid what the global policy allows.
    r.act
        .register(
            InstanceSpec::new("strict", "test", all_caps(), Mode::Shadow)
                .with_safety(safety.with_heating_off(false)),
        )
        .unwrap();
    let off = r
        .act
        .submit_as(
            "test",
            req("strict", SwitchCommand::set(dev("boiler"), false).into()),
        )
        .await;
    assert_eq!(off, Err(Rejection::HeatingOffNotAllowed));
}

#[tokio::test]
async fn per_instance_policy_only_narrows() {
    let r = rig(SafetyPolicy::default()).await;
    let spec = |name: &str, s: InstanceSafety| {
        InstanceSpec::new(name, "test", all_caps(), Mode::Shadow).with_safety(s)
    };
    let widen = [
        InstanceSafety::new().with_setpoint(SetpointBand::new(10.0, 20.0).unwrap()),
        InstanceSafety::new().with_min_interval(SignedDuration::from_mins(1)),
        InstanceSafety::new().with_max_per_hour(13),
        InstanceSafety::new().with_max_hold(SignedDuration::from_hours(3)),
        InstanceSafety::new().with_heating_off(true),
    ];
    for (i, s) in widen.into_iter().enumerate() {
        let err = r.act.register(spec(&format!("w{i}"), s)).unwrap_err();
        assert!(
            matches!(err, RegisterError::Policy(PolicyError::Widens(_))),
            "{err}"
        );
    }
    let narrow = InstanceSafety::new()
        .with_setpoint(SetpointBand::new(16.0, 20.0).unwrap())
        .with_max_per_hour(1);
    r.act.register(spec("n", narrow)).unwrap();
    let p = r.act.policy_for("n").unwrap();
    assert_eq!((p.setpoint.max(), p.max_per_hour), (20.0, 1));
    assert!(matches!(
        r.act.submit_as("test", req("n", set_target("z", 21.0))).await,
        Err(Rejection::OutsideBand { max, .. }) if max == 20.0
    ));
    assert!(
        r.act
            .submit_as("test", req("n", set_target("z", 19.0)))
            .await
            .is_ok()
    );
    assert!(matches!(
        r.act
            .submit_as("test", req("n", set_target("y", 19.0)))
            .await,
        Err(Rejection::RateLimited { .. })
    ));
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Behaviour {
    Ok,
    Fail,
    Hang,
}

/// An audit log that follows a script: succeed, fail, or never confirm
/// (the record is then not durable).
#[derive(Debug, Clone, Default)]
struct ScriptedLog {
    inner: MemoryAuditLog,
    script: Arc<Mutex<VecDeque<Behaviour>>>,
}

impl ScriptedLog {
    fn push(&self, b: Behaviour) {
        self.script.lock().unwrap().push_back(b);
    }
}

impl AuditLog for ScriptedLog {
    fn append(&self, record: AuditRecord) -> BoxFuture<Result<(), AuditError>> {
        let b = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Behaviour::Ok);
        match b {
            Behaviour::Ok => self.inner.append(record),
            Behaviour::Fail => Box::pin(std::future::ready(Err(AuditError::Unavailable(
                "disk full".into(),
            )))),
            Behaviour::Hang => Box::pin(std::future::pending()),
        }
    }

    fn tail(&self, since: Timestamp) -> BoxFuture<Result<AuditTail, AuditError>> {
        self.inner.tail(since)
    }
}

#[tokio::test(start_paused = true)]
async fn audit_failure_or_timeout_withholds_command() {
    let log = ScriptedLog::default();
    let clock = ManualClock::new(t0());
    let policy = SafetyPolicy::default()
        .with_min_interval(SignedDuration::ZERO)
        .with_max_per_hour(100);
    let (act, mut sink) = rig_with(policy, Arc::new(log.clone()), clock, false).await;
    let mut cmds = live(&act, "heat").await;

    log.push(Behaviour::Fail);
    assert_eq!(
        act.submit_as("test", req("heat", set_target("a", 20.0)))
            .await,
        Err(Rejection::AuditUnavailable)
    );
    assert!(nothing_arrives(&mut cmds).await);

    log.push(Behaviour::Hang);
    let started = tokio::time::Instant::now();
    assert_eq!(
        act.submit_as("test", req("heat", set_target("b", 20.0)))
            .await,
        Err(Rejection::AuditTimedOut)
    );
    assert_eq!(started.elapsed(), AUDIT_TIMEOUT);
    assert!(nothing_arrives(&mut cmds).await);

    // The withheld command is recorded as dropped, never as delivered.
    let recs = log.inner.records();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].kind, AuditKind::Dropped);
    assert!(
        sink_events(&mut sink)
            .iter()
            .all(|e| e.kind == Event::COMMAND_DROPPED)
    );

    // The queue slots were released: the queue is not leaked.
    for i in 0..COMMAND_QUEUE {
        let out = act
            .submit_as("test", req("heat", set_target(&format!("z{i}"), 20.0)))
            .await;
        assert!(matches!(out, Ok(Outcome::Delivered { .. })), "{out:?}");
    }
    let s = act.stats("heat").unwrap().snapshot();
    assert_eq!(s.commands_audit_failed, 2);
}

#[tokio::test]
async fn rate_limits_per_target_and_per_hour() {
    let policy = SafetyPolicy::default().with_max_per_hour(3);
    let r = rig(policy).await;
    register(&r.act, "heat", Mode::Shadow);
    let submit = |z: &'static str| r.act.submit_as("test", req("heat", set_target(z, 20.0)));
    assert!(submit("a").await.is_ok());
    assert_eq!(
        submit("a").await,
        Err(Rejection::RateLimited {
            retry_after: SignedDuration::from_mins(5)
        })
    );
    assert!(submit("b").await.is_ok());
    assert!(submit("c").await.is_ok());
    // Fourth distinct zone within the hour: per-instance cap.
    assert!(matches!(
        submit("d").await,
        Err(Rejection::RateLimited { .. })
    ));
    r.clock.advance(SignedDuration::from_mins(60));
    assert!(submit("d").await.is_ok());
}

#[tokio::test]
async fn resume_schedule_is_never_rate_limited_away() {
    let policy = SafetyPolicy::default().with_max_per_hour(1);
    let log = MemoryAuditLog::new();
    let clock = ManualClock::new(t0());
    // Boot hold-off on: everything else is refused right after start.
    let (act, _sink) = rig_with(policy, Arc::new(log.clone()), clock.clone(), true).await;
    let mut cmds = live(&act, "heat").await;
    assert!(matches!(
        act.submit_as("test", req("heat", set_target("a", 20.0)))
            .await,
        Err(Rejection::RateLimited { .. })
    ));
    for _ in 0..5 {
        let out = act.submit_as("test", req("heat", resume("a"))).await;
        assert!(matches!(out, Ok(Outcome::Delivered { .. })), "{out:?}");
        assert_eq!(cmds.recv().await.unwrap().command, resume("a"));
    }
    clock.advance(SignedDuration::from_mins(10));
    // Resumes still count: the hourly budget (1) is spent.
    assert!(matches!(
        act.submit_as("test", req("heat", set_target("b", 20.0)))
            .await,
        Err(Rejection::RateLimited { .. })
    ));
    assert!(
        act.submit_as("test", req("heat", resume("b")))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn wall_clock_steps_neither_widen_nor_block_limits() {
    let r = rig(SafetyPolicy::default().with_max_per_hour(2)).await;
    register(&r.act, "heat", Mode::Shadow);
    assert!(
        r.act
            .submit_as("test", req("heat", set_target("a", 20.0)))
            .await
            .is_ok()
    );
    // A forward step of a day does not reopen the per-target interval…
    r.clock.step_wall(SignedDuration::from_hours(24));
    assert!(matches!(
        r.act
            .submit_as("test", req("heat", set_target("a", 20.0)))
            .await,
        Err(Rejection::RateLimited { .. })
    ));
    assert!(
        r.act
            .submit_as("test", req("heat", set_target("b", 20.0)))
            .await
            .is_ok()
    );
    // …nor the hourly cap.
    r.clock.step_wall(SignedDuration::from_hours(24));
    assert!(matches!(
        r.act
            .submit_as("test", req("heat", set_target("c", 20.0)))
            .await,
        Err(Rejection::RateLimited { .. })
    ));
    // A backward step does not block once real time has passed.
    r.clock.step_wall(SignedDuration::from_hours(-72));
    r.clock.advance(SignedDuration::from_mins(61));
    assert!(
        r.act
            .submit_as("test", req("heat", set_target("c", 20.0)))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn limits_and_ids_survive_a_restart() {
    let policy = SafetyPolicy::default().with_max_per_hour(2);
    let log = MemoryAuditLog::new();
    let clock = ManualClock::new(t0());
    let (act, _s) = rig_with(policy.clone(), Arc::new(log.clone()), clock.clone(), true).await;
    register(&act, "heat", Mode::Shadow);
    clock.advance(SignedDuration::from_mins(5));
    let first = act
        .submit_as("test", req("heat", set_target("a", 20.0)))
        .await
        .unwrap();
    clock.advance(SignedDuration::from_mins(5));
    let second = act
        .submit_as("test", req("heat", set_target("b", 20.0)))
        .await
        .unwrap();
    assert!(first.id() < second.id());
    drop(act);

    // Restart one minute later (same wall second as the first boot + 11 min).
    clock.advance(SignedDuration::from_mins(1));
    let (act, _s) = rig_with(policy, Arc::new(log.clone()), clock.clone(), true).await;
    register(&act, "heat", Mode::Shadow);
    clock.advance(SignedDuration::from_mins(5)); // past the hold-off
    // The hourly cap remembers both commands from before the restart.
    assert!(matches!(
        act.submit_as("test", req("heat", set_target("c", 20.0)))
            .await,
        Err(Rejection::RateLimited { .. })
    ));
    clock.advance(SignedDuration::from_mins(50));
    let third = act
        .submit_as("test", req("heat", set_target("c", 20.0)))
        .await
        .unwrap();
    assert!(third.id() > second.id());
    assert!(third.id().boot() > second.id().boot());
    let ids: Vec<_> = log.records().iter().map(|r| r.id).collect();
    let unique: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len());
}

#[tokio::test]
async fn detach_drops_queued_commands_and_audits_them() {
    let r = rig(SafetyPolicy::default()).await;
    let mut first_run = live(&r.act, "heat").await;
    let a = r
        .act
        .submit_as("test", req("heat", set_target("a", 20.0)))
        .await
        .unwrap();
    let b = r
        .act
        .submit_as("test", req("heat", set_target("b", 20.0)))
        .await
        .unwrap();
    assert_eq!(first_run.recv().await.unwrap().id, a.id());
    // The run ends with `b` still queued.
    r.act.detach("heat").await;
    assert_eq!(first_run.recv().await, None);
    assert!(matches!(
        r.act
            .submit_as("test", req("heat", set_target("c", 20.0)))
            .await,
        Err(Rejection::AdapterGone { .. })
    ));
    // A new run never sees `b`.
    let mut second_run = r.act.attach("heat").await.unwrap();
    assert!(nothing_arrives(&mut second_run).await);
    let dropped: Vec<_> = r
        .log
        .records()
        .into_iter()
        .filter(|rec| rec.kind == AuditKind::Dropped)
        .collect();
    assert_eq!(dropped.len(), 1);
    assert_eq!(dropped[0].id, b.id());
    assert_eq!(dropped[0].actor, ACTUATOR);
    let s = r.act.stats("heat").unwrap().snapshot();
    assert_eq!(
        (s.commands_delivered, s.commands_dropped, s.commands_failed),
        (2, 1, 1)
    );
    // Attaching again without detaching also drops what the old run left.
    r.clock.advance(SignedDuration::from_mins(5));
    r.act
        .submit_as("test", req("heat", set_target("d", 20.0)))
        .await
        .unwrap();
    let _third_run = r.act.attach("heat").await.unwrap();
    assert_eq!(second_run.recv().await, None);
    assert_eq!(r.act.stats("heat").unwrap().snapshot().commands_dropped, 2);
}

#[tokio::test]
async fn queue_full_is_busy() {
    let r = rig(SafetyPolicy::default()
        .with_min_interval(SignedDuration::ZERO)
        .with_max_per_hour(100))
    .await;
    let _cmds = live(&r.act, "heat").await;
    for i in 0..COMMAND_QUEUE {
        assert!(
            r.act
                .submit_as("test", req("heat", set_target(&format!("z{i}"), 20.0)))
                .await
                .is_ok()
        );
    }
    assert!(matches!(
        r.act
            .submit_as("test", req("heat", set_target("over", 20.0)))
            .await,
        Err(Rejection::AdapterBusy { .. })
    ));
}

#[tokio::test]
async fn boot_holdoff_is_on_by_default() {
    let log = MemoryAuditLog::new();
    let clock = ManualClock::new(t0());
    let act = Actuator::builder(
        SafetyPolicy::default(),
        Arc::new(clock.clone()),
        Arc::new(log),
    )
    .build()
    .await
    .unwrap();
    register(&act, "heat", Mode::Shadow);
    let planner = act.submitter("planner").unwrap();
    assert!(matches!(
        planner.submit(req("heat", set_target("a", 20.0))).await,
        Err(Rejection::RateLimited { .. })
    ));
    // The safe direction is exempt; after the interval, others pass too.
    assert!(planner.submit(req("heat", resume("a"))).await.is_ok());
    clock.advance(SafetyPolicy::default().min_interval);
    assert!(
        planner
            .submit(req("heat", set_target("b", 20.0)))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn reserved_names_and_actors_are_refused() {
    let r = rig(SafetyPolicy::default()).await;
    for name in ["actuator", "core", "heimr", "heimr-app"] {
        assert_eq!(
            r.act
                .register(InstanceSpec::new(name, "test", all_caps(), Mode::Shadow)),
            Err(RegisterError::ReservedName(name.to_owned())),
        );
        assert_eq!(
            r.act.submitter(name).map(|_| ()),
            Err(ActorError::Reserved(name.to_owned()))
        );
    }
    assert!(matches!(
        r.act.submitter("not an id"),
        Err(ActorError::Invalid(..))
    ));
    // The actor is the handle's, whatever the request says.
    register(&r.act, "heat", Mode::Shadow);
    let user = r.act.submitter("user:alice").unwrap();
    assert_eq!(user.actor(), "user:alice");
    user.submit(req("heat", set_target("a", 20.0)))
        .await
        .unwrap();
    assert_eq!(r.log.records()[0].actor, "user:alice");
}

/// A log whose tail also reports unreadable entries.
#[derive(Debug, Clone)]
struct UnreadableLog(Vec<UnreadableEntry>);

impl AuditLog for UnreadableLog {
    fn append(&self, _: AuditRecord) -> BoxFuture<Result<(), AuditError>> {
        Box::pin(std::future::ready(Ok(())))
    }
    fn tail(&self, _: Timestamp) -> BoxFuture<Result<AuditTail, AuditError>> {
        let tail = AuditTail::new(None, vec![]).with_unknown(self.0.clone());
        Box::pin(std::future::ready(Ok(tail)))
    }
}

#[tokio::test]
async fn unreadable_audit_entries_count_as_accepted_commands() {
    let policy = SafetyPolicy::default()
        .with_max_per_hour(1)
        .with_min_interval(SignedDuration::from_mins(5));
    let try_with = async |entries: Vec<UnreadableEntry>, target: &str| {
        let clock = ManualClock::new(t0());
        let (act, _) = rig_with(
            policy.clone(),
            Arc::new(UnreadableLog(entries)),
            clock,
            false,
        )
        .await;
        register(&act, "heat", Mode::Shadow);
        act.submit_as("test", req("heat", set_target(target, 20.0)))
            .await
    };
    // Nothing known: counts against every instance and target.
    assert!(matches!(
        try_with(vec![UnreadableEntry::unknown()], "a").await,
        Err(Rejection::RateLimited { .. })
    ));
    // Known to be another instance's: no effect here.
    let other = UnreadableEntry::new(None, Some("other".into()), None);
    assert!(try_with(vec![other], "a").await.is_ok());
    // Known instance, unknown target: hourly cap and every target's interval.
    let heat = UnreadableEntry::new(None, Some("heat".into()), None);
    assert!(try_with(vec![heat], "a").await.is_err());
    // Older than an hour: ignored.
    let old = UnreadableEntry::new(Some(t0() - SignedDuration::from_mins(61)), None, None);
    assert!(try_with(vec![old], "a").await.is_ok());
    // Known target only matters for the interval; the cap still applies.
    let policy2 = SafetyPolicy::default()
        .with_max_per_hour(5)
        .with_min_interval(SignedDuration::from_mins(5));
    let clock = ManualClock::new(t0());
    let e = UnreadableEntry::new(Some(t0()), Some("heat".into()), Some("a".into()));
    let (act, _) = rig_with(policy2, Arc::new(UnreadableLog(vec![e])), clock, false).await;
    register(&act, "heat", Mode::Shadow);
    assert!(
        act.submit_as("test", req("heat", set_target("a", 20.0)))
            .await
            .is_err()
    );
    assert!(
        act.submit_as("test", req("heat", set_target("b", 20.0)))
            .await
            .is_ok()
    );
}

/// A log whose appends take `delay` (tokio time) to become durable.
#[derive(Debug, Clone)]
struct SlowLog(MemoryAuditLog, std::time::Duration);

impl AuditLog for SlowLog {
    fn append(&self, record: AuditRecord) -> BoxFuture<Result<(), AuditError>> {
        let (inner, delay) = (self.0.clone(), self.1);
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            inner.append(record).await
        })
    }
    fn tail(&self, since: Timestamp) -> BoxFuture<Result<AuditTail, AuditError>> {
        self.0.tail(since)
    }
}

#[tokio::test(start_paused = true)]
async fn a_dropped_submit_still_completes() {
    let log = MemoryAuditLog::new();
    let slow = SlowLog(log.clone(), std::time::Duration::from_millis(100));
    let (act, _) = rig_with(
        SafetyPolicy::default(),
        Arc::new(slow),
        ManualClock::new(t0()),
        false,
    )
    .await;
    let mut cmds = live(&act, "heat").await;
    let planner = act.submitter("planner").unwrap();
    // The caller gives up while the audit write is in flight…
    let gave_up = tokio::time::timeout(
        std::time::Duration::from_millis(10),
        planner.submit(req("heat", set_target("a", 20.0))),
    )
    .await;
    assert!(gave_up.is_err());
    // …but the submission finishes: audited as delivered, and delivered.
    let got = tokio::time::timeout(std::time::Duration::from_secs(1), cmds.recv())
        .await
        .unwrap()
        .unwrap();
    let records = log.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, got.id);
    assert_eq!(records[0].result, AuditResult::Delivered);
}

#[tokio::test]
async fn reattach_closes_the_old_queue_at_once() {
    let r = rig(SafetyPolicy::default()).await;
    let mut old = live(&r.act, "heat").await;
    let mut new = r.act.attach("heat").await.unwrap();
    // The stale run's handle is closed before anything can reach it.
    assert!(old.recv().await.is_none());
    r.act
        .submit_as("test", req("heat", set_target("a", 20.0)))
        .await
        .unwrap();
    assert!(new.recv().await.is_some());
}

#[test]
fn rate_limiter_boot_holdoff() {
    let p = SafetyPolicy::default();
    let rl = RateLimiter::after_boot(SignedDuration::from_secs(100));
    assert!(
        rl.check("a", "z", SignedDuration::from_secs(100), &p)
            .is_err()
    );
    assert!(
        rl.check("a", "z", SignedDuration::from_secs(399), &p)
            .is_err()
    );
    assert!(
        rl.check("a", "z", SignedDuration::from_secs(400), &p)
            .is_ok()
    );
    let zero = SafetyPolicy::default().with_max_per_hour(0);
    assert!(
        RateLimiter::new()
            .check("a", "z", SignedDuration::ZERO, &zero)
            .is_err()
    );
}

#[derive(Debug, Clone)]
enum Op {
    Submit {
        advance_s: i64,
        adapter: usize,
        zone: usize,
        target: f64,
        kind: u8,
    },
    WallStepBack(i64),
    Restart(i64),
    AuditFails,
    AuditHangs,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        12 => (0i64..900, 0usize..3, 0usize..3, 10.0f64..30.0, 0u8..6).prop_map(
            |(advance_s, adapter, zone, target, kind)| Op::Submit { advance_s, adapter, zone, target, kind }
        ),
        1 => (1i64..7200).prop_map(Op::WallStepBack),
        1 => (0i64..600).prop_map(Op::Restart),
        1 => Just(Op::AuditFails),
        1 => Just(Op::AuditHangs),
    ]
}

fn command_for(kind: u8, zone_n: usize, target: f64, hold_s: i64) -> Option<Command> {
    let z = zone(&format!("zone-{zone_n}"));
    let d = DeviceId::from(z.clone());
    // Holds outside the type's bound cannot be built; use the shortest.
    let hold = Hold::for_duration(SignedDuration::from_secs(hold_s)).unwrap_or(Hold::For(
        HoldDuration::new(SignedDuration::from_secs(1)).ok()?,
    ));
    Some(match kind {
        0 => ClimateCommand::set_target(z, Setpoint::envelope_only(target).ok()?, hold).into(),
        1 => ClimateCommand::resume_schedule(z).into(),
        2 => ClimateCommand::turn_off(z, hold).into(),
        3 => FanCommand::set_mode(d, FanMode::Manual((target as u8) % 8), hold).into(),
        4 => SwitchCommand::set(d, target > 20.0).into(),
        _ => ClimateCommand::set_target(
            z,
            Setpoint::envelope_only(target).ok()?,
            Hold::NextScheduleChange,
        )
        .into(),
    })
}

struct Accepted {
    adapter: usize,
    target: String,
    mono: SignedDuration,
    /// Monotonic time of the start of the run that accepted it.
    boot: SignedDuration,
    exempt: bool,
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Over arbitrary command sequences with restarts, backward wall-clock
    /// steps and audit failures and timeouts:
    /// - shadow instances never receive anything;
    /// - nothing is delivered unless its decision record was durable first;
    /// - delivered setpoints stay inside the instance's band, holds within
    ///   the maximum, heating-off only when allowed;
    /// - per-target spacing, the hourly cap and the boot hold-off hold for
    ///   every accepted non-exempt command, across restarts, in real time;
    /// - command ids are unique and increasing across restarts.
    #[test]
    fn actuator_invariants(
        ops in proptest::collection::vec(op(), 1..80),
        modes in proptest::array::uniform3(any::<bool>()),
        band in (5.0f64..30.0, 0.0f64..10.0),
        min_interval_s in 0i64..1200,
        max_per_hour in 0u32..8,
        max_hold_s in 1i64..14_400,
        hold_s in -60i64..20_000,
        allow_off in any::<bool>(),
    ) {
        let policy = SafetyPolicy::default()
            .with_setpoint(SetpointBand::new(band.0, (band.0 + band.1).min(30.0)).unwrap())
            .with_min_interval(SignedDuration::from_secs(min_interval_s))
            .with_max_per_hour(max_per_hour)
            .with_max_hold(SignedDuration::from_secs(max_hold_s))
            .with_heating_off(allow_off);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        rt.block_on(async {
            let log = ScriptedLog::default();
            let clock = ManualClock::new(t0());
            let mut accepted: Vec<Accepted> = vec![];
            let mut boots: Vec<SignedDuration> = vec![];
            let mut ids: Vec<CommandId> = vec![];
            let start = |log: ScriptedLog, clock: ManualClock| {
                let policy = policy.clone();
                async move {
                let (act, _sink) = rig_with(policy, Arc::new(log), clock, true).await;
                let mut handles = vec![];
                for (i, live) in modes.iter().enumerate() {
                    let mode = if *live { Mode::Live } else { Mode::Shadow };
                    register(&act, &format!("a{i}"), mode);
                    handles.push(act.attach(&format!("a{i}")).await.unwrap());
                }
                (act, handles)
                }
            };
            boots.push(clock.monotonic());
            let (mut act, mut handles) = start(log.clone(), clock.clone()).await;
            for (h, live) in handles.iter().zip(modes) {
                prop_assert_eq!(h.is_live(), live);
            }
            for op in &ops {
                match op {
                    Op::WallStepBack(s) => clock.step_wall(SignedDuration::from_secs(-*s)),
                    Op::AuditFails => log.push(Behaviour::Fail),
                    Op::AuditHangs => log.push(Behaviour::Hang),
                    Op::Restart(after_s) => {
                        drop(act);
                        clock.advance(SignedDuration::from_secs(*after_s));
                        boots.push(clock.monotonic());
                        (act, handles) = start(log.clone(), clock.clone()).await;
                    }
                    Op::Submit { advance_s, adapter, zone: z, target, kind } => {
                        clock.advance(SignedDuration::from_secs(*advance_s));
                        let Some(cmd) = command_for(*kind, *z, *target, hold_s) else { continue };
                        let res = act.submit_as("prop", CommandRequest::new(format!("a{adapter}"), cmd.clone(), "r")).await;
                        let exempt = cmd.kind() == CommandKind::ClimateSchedule;
                        if let Ok(out) = res {
                            {
                                ids.push(out.id());
                                accepted.push(Accepted { adapter: *adapter, target: cmd.target().to_owned(), mono: clock.monotonic(), boot: *boots.last().unwrap(), exempt });
                                if let Outcome::Delivered { id } = out {
                                    prop_assert!(modes[*adapter], "delivered to a shadow adapter");
                                    let got = handles[*adapter].recv().await.unwrap();
                                    prop_assert_eq!(got.id, id);
                                    // Durable before delivery.
                                    let durable = log.inner.records().iter().any(|r| r.id == id && r.result == AuditResult::Delivered);
                                    prop_assert!(durable, "delivered without a durable audit record");
                                    match &got.command {
                                        Command::Climate(ClimateCommand::SetTarget { target, .. }) => {
                                            prop_assert!(policy.setpoint.contains(*target));
                                        }
                                        Command::Climate(ClimateCommand::TurnOff { .. }) => prop_assert!(allow_off),
                                        _ => {}
                                    }
                                    if let Some(Hold::For(d)) = got.command.hold() {
                                        prop_assert!(d.get() > SignedDuration::ZERO && d.get() <= policy.max_hold);
                                    }
                                } else {
                                    prop_assert!(!modes[*adapter], "shadowed on a live adapter");
                                }
                            }
                        }
                    }
                }
            }
            for (i, h) in handles.iter_mut().enumerate() {
                let left = tokio::time::timeout(std::time::Duration::from_millis(1), h.recv()).await;
                prop_assert!(left.is_err(), "adapter a{} had an undelivered or shadow command", i);
            }
            for (i, a) in accepted.iter().enumerate() {
                if a.exempt {
                    continue;
                }
                prop_assert!(a.mono - a.boot >= policy.min_interval, "inside the boot hold-off");
                for b in &accepted[..i] {
                    if a.adapter == b.adapter && a.target == b.target {
                        prop_assert!(a.mono - b.mono >= policy.min_interval);
                    }
                }
                let in_hour = accepted[..=i]
                    .iter()
                    .filter(|b| b.adapter == a.adapter && a.mono - b.mono < SignedDuration::from_hours(1))
                    .count();
                prop_assert!(in_hour <= max_per_hour as usize);
            }
            for w in ids.windows(2) {
                prop_assert!(w[0] < w[1], "ids not increasing: {} then {}", w[0], w[1]);
            }
            Ok(())
        })?;
    }
}
