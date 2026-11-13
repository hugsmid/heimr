# Writing an adapter

> **Draft.** The adapter API in `heimr-core` may still change before its first release.

An adapter connects one device family or service to heimr. It turns what the device says into typed readings, state and events and, if it can write, turns heimr's commands into device calls. Everything else (routing, storage, safety, auditing, restarts, telemetry) belongs to the core.

The worked example throughout is the `demo` adapter in [`crates/heimr/src/demo.rs`](../crates/heimr/src/demo.rs): a synthetic sensor with simulated heating zones that accepts setpoint commands. Every code block below is quoted from real, compiled and tested code; a test (`crates/heimr/tests/guide.rs`) fails if a quote drifts.

## 1. Start a crate

An adapter is an ordinary library crate that depends on `heimr-core`:

```bash
cargo new --lib my-adapter
cd my-adapter
cargo add heimr-core --git https://github.com/hugsmid/heimr
cargo add jiff --features serde
cargo add serde --features derive
cargo add tokio --features macros,time
```

Add `#![forbid(unsafe_code)]`, a README, docs and an example, and a `tests/fixtures/` directory with a `README.md` saying how each fixture was made.

## 2. Kinds, instances and the trait

An adapter crate provides a **kind** (e.g. `demo`). A site configures **instances** of it, one `[adapters.<instance>]` section each with `type = "<kind>"`; several instances of one kind may run side by side. Instance names `actuator`, `core` and `heimr…` are reserved for the core. The instance name is the `source` stamped on everything the instance emits, and names its state directory.

Rust adapters implement the typed trait `heimr_core::Adapter`:

<!-- quote: crates/heimr-core/src/adapter.rs#adapter-trait -->
```rust
pub trait Adapter: Sized + Send + 'static {
    /// The adapter's own keys in its instance section (everything except
    /// `type`, `enabled`, `mode` and `safety`, which the app handles).
    /// Unknown keys are rejected.
    type Config: DeserializeOwned + Send + Sync + 'static;

    /// Kind name, summary and the most any instance can do.
    fn descriptor() -> AdapterDescriptor;

    /// Build the adapter from its config. Validate here; do no I/O.
    fn new(config: &Self::Config) -> Result<Self, AdapterError>;

    /// What this instance can do, if it depends on its config (e.g. the
    /// levels of the configured device model). Must not exceed the
    /// descriptor. Defaults to the descriptor's capabilities.
    fn capabilities(&self) -> Capabilities {
        Self::descriptor().capabilities
    }

    /// Run until [`AdapterContext::shutdown`] fires, then return `Ok(())`
    /// promptly. Handle routine failures (reconnects) inside. Returning
    /// `Err`, panicking, or returning `Ok(())` before shutdown marks the
    /// instance unhealthy; transient errors restart it with backoff, fatal
    /// ones (and repeated panics) keep it down.
    fn run(
        self,
        ctx: AdapterContext<Self::Config>,
    ) -> impl Future<Output = Result<(), AdapterError>> + Send;
}
```

`run` is a native `async fn` in a trait: implement it as a plain `async fn run(self, ctx) -> …`, with no macro. The app wraps a typed adapter in `TypedFactory`, an implementation of the object-safe `AdapterFactory` trait; adapters running in another process are registered through that trait directly, with a descriptor received at run time. Its `prepare` is asynchronous and fallible, so such a host can validate an instance's settings and learn its capabilities over its protocol (`heimr check-config` waits for it).

The **descriptor** names the kind and declares its **capabilities**: whether it reads, and which command kinds it carries out. Descriptors are owned data, built with constructors. The demo declares the three climate command kinds:

<!-- quote: crates/heimr/src/demo.rs#descriptor -->
```rust
impl Adapter for Demo {
    type Config = DemoConfig;

    fn descriptor() -> AdapterDescriptor {
        AdapterDescriptor::new(
            "demo",
            "synthetic temperature/humidity sensor with simulated heating zones",
            Capabilities::read_write([
                CommandKind::ClimateTarget,
                CommandKind::ClimateSchedule,
                CommandKind::ClimateOff,
            ]),
        )
    }
```

A sensor declares `Capabilities::read_only()`. An instance with no write capability never gets a command channel, whatever its configuration says, and `mode = "live"` on it is a configuration error. An air-treatment adapter that accepts `CommandKind::FanMode` must also declare its fan levels (`with_fan_levels(1, 5)`) or named presets (`with_fan_presets(["night"])`); the core refuses others. If an instance can do less than its kind (say, the configured model has fewer levels), override `Adapter::capabilities`.

**Configuration** is a plain `serde` type for the adapter's own keys. The app handles `type`, `enabled`, `mode` and `safety` (so adapters cannot use those names) and rejects unknown keys by name, inside your config type too:

<!-- quote: crates/heimr/src/demo.rs#config -->
```rust
/// The demo's instance settings.
#[derive(Debug, Clone, Deserialize)]
pub struct DemoConfig {
    /// Time between samples (1 s ..= 1 day).
    #[serde(default = "default_interval")]
    pub interval: SignedDuration,
    /// Simulated zones (1–32, unique).
    #[serde(default = "default_zones")]
    pub zones: Vec<ZoneId>,
    /// The simulated schedule's target, which zones start with and return
    /// to when an override ends.
    #[serde(default = "default_target")]
    pub initial_target: Setpoint,
    /// Length of a simulated schedule block (1 min ..= 1 day): overrides
    /// held "until the next schedule change" end at the next multiple of it.
    #[serde(default = "default_block")]
    pub schedule_block: SignedDuration,
    /// Noise seed.
    #[serde(default)]
    pub seed: u64,
}
```

Validate in `new`, which runs for `heimr check-config` and before every restart, and must not do I/O. Name the offending key with `AdapterError::config_key`, so the message reads `adapters.<instance>.<key>: …`:

<!-- quote: crates/heimr/src/demo.rs#new -->
```rust
fn new(config: &DemoConfig) -> Result<Self, AdapterError> {
    if config.interval < SignedDuration::from_secs(1)
        || config.interval > SignedDuration::from_hours(24)
    {
        return Err(AdapterError::config_key(
            "interval",
            "must be between 1s and 24h",
        ));
    }
    if config.zones.is_empty() || config.zones.len() > 32 {
        return Err(AdapterError::config_key("zones", "must list 1 to 32 zones"));
    }
    if config.zones.iter().collect::<BTreeSet<_>>().len() != config.zones.len() {
        return Err(AdapterError::config_key("zones", "must be unique"));
    }
    if config.schedule_block < SignedDuration::from_mins(1)
        || config.schedule_block > SignedDuration::from_hours(24)
    {
        return Err(AdapterError::config_key(
            "schedule_block",
            "must be between 1m and 24h",
        ));
    }
    let zones = config
        .zones
        .iter()
        .map(|id| Zone {
            id: id.clone(),
            device: DeviceId::from(id.clone()),
            mode: ClimateMode::Schedule,
            target: config.initial_target,
            override_until: None,
            temperature: config.initial_target.get() - START_BELOW_C,
            last: None,
        })
        .collect();
    Ok(Self {
        zones,
        rng: XorShift::new(config.seed),
    })
}
```

The matching instance section:

<!-- quote: crates/heimr/src/demo.rs#toml -->
```toml
[adapters.demo]
type = "demo"
mode = "shadow"        # or "live"
interval = "1m"
zones = ["zone-a", "zone-b"]
initial_target = 19.0
schedule_block = "3h"
seed = 1
```

## 3. `run(ctx)`

An instance's whole life is one async function. It returns `Ok(())` when shutdown is signalled. Handle routine failures (reconnects, timeouts) inside `run`. If `run` returns an error, panics, or returns `Ok(())` while the app is still running, the instance is marked unhealthy and the core restarts it with exponential backoff and jitter, rebuilding it from its configuration; the rest of the app keeps running. Return `AdapterError::transient(..)` for failures a restart may fix (device offline) and `AdapterError::fatal(..)` for those it cannot (credentials rejected): a fatal error keeps the instance down and unhealthy until the app restarts. Panics are restarted too, but an instance that panics `max_consecutive_panics` times (`[supervision]`, default 5) without a healthy run in between is also kept down.

`AdapterContext` gives the instance exactly these:

| Method | Use |
|---|---|
| `ctx.config()` | The typed config, already parsed and validated |
| `ctx.reading(..)`, `ctx.event(..)`, `ctx.emit(..)`, `ctx.emitter()` | Build and send readings, state and events. `reading` and `event` stamp the instance name; the core refuses records that name another source or actor, and events of a reserved kind (`command`, `command_*`, `adapter_*`, `heimr*`). `emit` waits when the app is behind (back-pressure) |
| `ctx.take_commands()` | The command handle. Yields only for a write-capable instance configured `live` |
| `ctx.now()`, `ctx.sleep_until(..)`, `ctx.sleep(..)` | All time comes from the context clock (wall time for timestamps, monotonic for `sleep`), so tests can use a fake one |
| `ctx.shutdown()` | Finish in-flight work and return promptly |
| `ctx.state_dir()` | Private, persistent directory for pairing keys, cursors or caches |
| `ctx.health()`, `ctx.metrics()` | Telemetry (section 5); the adapter can report health and metrics but not change its run state |

Adapters never see the bus, the store or other adapters. State records are keyed by (instance, zone or device id), so an instance cannot overwrite another's state whatever ids it uses.

A minimal read-only adapter polls, emits and honours shutdown. This is the example from the `Adapter` docs, compiled as a doc test:

<!-- quote: crates/heimr-core/src/adapter.rs#thermometer -->
```rust
use heimr_core::{Adapter, AdapterContext, AdapterDescriptor, AdapterError, Capabilities};
use heimr_core::{DeviceId, Metric, Unit};
use jiff::SignedDuration;

#[derive(Debug, serde::Deserialize)]
struct Config { interval: SignedDuration }

struct Thermometer;

impl Adapter for Thermometer {
    type Config = Config;

    fn descriptor() -> AdapterDescriptor {
        AdapterDescriptor::new("thermometer", "Reads one temperature sensor", Capabilities::read_only())
    }

    fn new(config: &Config) -> Result<Self, AdapterError> {
        if config.interval <= SignedDuration::ZERO {
            return Err(AdapterError::config_key("interval", "must be positive"));
        }
        Ok(Self)
    }

    async fn run(self, ctx: AdapterContext<Config>) -> Result<(), AdapterError> {
        let device = DeviceId::new("sensor-1").map_err(AdapterError::fatal)?;
        let mut next = ctx.now();
        loop {
            tokio::select! {
                () = ctx.shutdown().wait() => return Ok(()),
                () = ctx.sleep_until(next) => {}
            }
            let r = ctx.reading(device.clone(), Metric::TEMPERATURE, 20.0, Unit::Celsius);
            ctx.emit(r).await?;
            next += ctx.config().interval;
        }
    }
}

// The object-safe form the app registers:
let factory = heimr_core::TypedFactory::<Thermometer>::new();
assert_eq!(heimr_core::AdapterFactory::descriptor(&factory).kind, "thermometer");
```

A write-capable adapter takes the command handle and selects over shutdown, commands and its own timer (or device events). This is the demo's loop:

<!-- quote: crates/heimr/src/demo.rs#run -->
```rust
async fn run(mut self, mut ctx: AdapterContext<DemoConfig>) -> Result<(), AdapterError> {
    let interval = ctx.config().interval;
    let mut commands = ctx.take_commands();
    let mut next = ctx.now();
    ctx.health().ok().await?;
    loop {
        tokio::select! {
            biased;
            () = ctx.shutdown().wait() => return Ok(()),
            Some(cmd) = commands.recv() => self.apply(&ctx, cmd).await?,
            () = ctx.sleep_until(next) => {
                let now = ctx.now();
                self.tick(&ctx, now).await?;
                ctx.metrics().counter("ticks", 1);
                next = next.saturating_add(interval).unwrap_or(Timestamp::MAX);
                if next <= now {
                    // Fell behind (suspend, clock step): skip missed ticks.
                    next = now.saturating_add(interval).unwrap_or(Timestamp::MAX);
                }
            }
        }
    }
}
```

`Commands::recv` never resolves when the handle is disconnected (shadow mode or read-only), so the same loop is correct in both modes. Each command arrives as a `CommandEnvelope` with the actuator's `id` (it matches the audit record) and the typed `Command`. Commands and state types are `#[non_exhaustive]`: build them with their constructors and give every `match` a wildcard arm. The demo's handler:

<!-- quote: crates/heimr/src/demo.rs#apply -->
```rust
async fn apply(
    &mut self,
    ctx: &AdapterContext<DemoConfig>,
    cmd: CommandEnvelope,
) -> Result<(), EmitError> {
    let now = ctx.now();
    let Command::Climate(c) = &cmd.command else {
        return Ok(()); // the actuator only sends declared kinds
    };
    let schedule_target = ctx.config().initial_target;
    let until = cmd.command.hold().map(|h| self.override_end(ctx, now, h));
    let Some(z) = self.zones.iter_mut().find(|z| &z.id == c.zone()) else {
        return ctx
            .health()
            .degraded(format!(
                "command {} names unknown zone {}",
                cmd.id,
                c.zone()
            ))
            .await;
    };
    ctx.health().ok().await?;
    match c {
        ClimateCommand::SetTarget { target, .. } => {
            z.mode = ClimateMode::Manual;
            z.target = *target; // already checked against the instance's band
            z.override_until = until;
        }
        ClimateCommand::TurnOff { .. } => {
            z.mode = ClimateMode::Off;
            z.override_until = until;
        }
        // ResumeSchedule, and anything newer: back to the schedule.
        _ => {
            z.mode = ClimateMode::Schedule;
            z.target = schedule_target;
            z.override_until = None;
        }
    }
    ctx.emit(state(z, now, None)).await
}
```

### Holds: never leave a device overridden

Every command that overrides a device (a setpoint, heating off, a fan mode) carries a `Hold`: `Hold::NextScheduleChange`, or `Hold::For(duration)` bounded by the policy's `max_hold` (2 hours by default, never more than 24). **Map it to the device's native timed or "until the next schedule block" override, so the device reverts on its own if heimr stops. Never implement an open-ended override with a timer of your own.** If a device supports only one of the two, use the one that ends sooner. `ClimateCommand::ResumeSchedule`, the safe direction, has no hold and is never refused by a rate limit.

### Guidelines

- Prefer push (subscriptions, observe, event streams) over polling; when polling, respect the service's rate limits and make the interval configurable.
- Reconnect with backoff; never emit duplicates across a reconnect.
- Use the context clock for every timestamp and sleep.
- Clone `ctx.emitter()` into sub-tasks rather than passing the context around.
- No secrets in logs; credentials come from configuration, which a site fills from its secret store.

## 4. Shadow mode, live mode and the safety layer

Every write-capable instance starts in **shadow mode**. Commands are computed, checked, audited and counted, and never delivered. Switching an instance to **live** is a configuration change (`mode = "live"`) made deliberately by the site owner.

Deciders (a planner, an assistant, a user) never talk to an adapter. They hold a submit-only `CommandSubmitter`, bound to their actor name when it is created (`App::submitter("planner")`; names reserved for the core, such as `actuator`, `core` and `heimr…`, are refused), and submit requests; the actuator checks the instance's capabilities, its safety policy (the global `[safety]`, possibly narrowed per instance: setpoint band, hold length, fan levels and presets, whether heating may be turned off, including switching off devices declared heating-critical) and rate limits; writes an audit record and waits until it is durable; and only then delivers it, if the instance is live and running:

<!-- quote: crates/heimr/tests/demo.rs#submit -->
```rust
let target = planner.policy_for("demo").unwrap().setpoint(21.0).unwrap();
let hold = Hold::for_duration(SignedDuration::from_mins(10)).unwrap();
let cmd = ClimateCommand::set_target(ZoneId::new("zone-a").unwrap(), target, hold);
let request = CommandRequest::new("demo", cmd, "boost before the cheap slot ends");
let outcome = planner.submit(request).await;
// Live: Ok(Outcome::Delivered { id }); shadow: Ok(Outcome::Shadowed { id });
// refused (capability, band, hold, rate limit, …): Err(Rejection::…).
assert!(matches!(outcome, Ok(Outcome::Delivered { .. })));
```

This is enforced in the core, not by the adapter:

- In shadow mode no command queue exists, so `ctx.take_commands()` returns a handle that can never yield, and an adapter cannot construct a connected one.
- A command whose audit record cannot be made durable in time is withheld. A submission completes even if the decider stops waiting for it.
- Each run of an instance gets a fresh queue. Commands an instance had not received when it stopped are audited as dropped and never replayed to the next run.
- Shadowed commands count against rate limits, so shadow mode rehearses exactly what live mode would do. Limits survive restarts (they are rebuilt from the audit journal; lines it cannot read count as accepted commands), and right after start no command but "resume schedule" is accepted for the per-target interval. If the journal is missing altogether, that hold-off is the only limit until new history builds up.

What this means for you: write the command path as if it were live, test it, and let the configuration decide when it runs.

## 5. Telemetry you get for free

The app runs each instance inside a `tracing` span (`adapter`, with fields `adapter`, `kind` and `mode`), so everything it logs is attributed to it without extra code.

The core also keeps an `AdapterStats` per instance, counted at the seams it owns: readings, state records and events emitted; emit errors and refused records; last record time; commands offered, delivered, shadowed, rejected (by policy or rate limit), dropped, and withheld for want of a durable audit record; run state, start, exit, error, restarts and consecutive failures. `App::telemetry()` returns snapshots.

Two things are up to the adapter, each one line:

- **Health**: `ctx.health().ok().await?`, `ctx.health().degraded("…").await?` or `ctx.health().failed("…").await?`. Changes are recorded in the stats and emitted as an `adapter_health` event; repeating the same status and reason does nothing, so call it freely.
- **Custom metrics**: `ctx.metrics().counter("ticks", 1)` and `ctx.metrics().gauge("queue_depth", 4.0)`, kept in the same stats (names follow the id rules: ASCII letters, digits, `.` `_` `:` `-`).

## 6. Testing

Unit-test configuration and parsing directly, and run the adapter end to end inside the app on a fake clock with an in-memory sink. The demo's tests in [`crates/heimr/tests/demo.rs`](../crates/heimr/tests/demo.rs) do this in both modes, starting the app like this under `#[tokio::test(start_paused = true)]`:

<!-- quote: crates/heimr/tests/demo.rs#app-under-test -->
```rust
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
```

Drive time with tokio's paused clock, submit commands through the submitter, then assert on the sink's contents, the audit journal in the state directory and `telemetry`. `App::builder(..).audit_log(..)` takes a `MemoryAuditLog` instead of the journal. [`crates/heimr/tests/supervision.rs`](../crates/heimr/tests/supervision.rs) shows how to test restart behaviour.

Rules for fixtures:

- hand-built, or recorded **and** redacted, never raw;
- never real readings, schedules or presence: occupancy patterns are the most sensitive data a home produces;
- hand-built fixtures say so in a sibling `README.md`.

## 7. Keeping an adapter private

An adapter does not need to be published. Keep it in your own repository as an ordinary crate depending on `heimr-core`, and build a small binary that registers it next to the public adapters. It gets the same command line (`--config <file>`, `check-config <file>`) as the `heimr` binary, whose `main` is:

<!-- quote: crates/heimr/src/main.rs#main -->
```rust
fn main() -> std::process::ExitCode {
    heimr::cli::main("heimr", |b| b.public_adapters())
}
```

A site binary adds its kinds in the closure, e.g. `|b| b.public_adapters().adapter::<my_adapter::MyAdapter>()`. Configuration decides which instances run. Keep private recordings in your private repository too: redacted, and still never real schedules or presence.

You can propose a private adapter for inclusion later; see [`CONTRIBUTING.md`](../CONTRIBUTING.md).

## 8. Checklist

- [ ] Descriptor declares only the capabilities the adapter uses (fan levels and presets included).
- [ ] `new` validates the config, names bad keys and does no I/O.
- [ ] Holds map to the device's native timed or next-block override; no open-ended overrides.
- [ ] All time from the context clock; shutdown honoured promptly.
- [ ] Transient and fatal errors distinguished; no duplicates across reconnect; health reported.
- [ ] `match`es on core types have wildcard arms.
- [ ] Tests over redacted or hand-built fixtures, in shadow and live mode.
- [ ] Fixtures directory has a `README.md` saying how each fixture was made.
- [ ] README, docs and an example; `#![forbid(unsafe_code)]`.
