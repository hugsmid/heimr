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

Add `#![forbid(unsafe_code)]`, a README, docs and an example, and a `tests/fixtures/` directory with a `README.md` saying how each fixture was made. Or start from a copy of [`templates/adapter`](../templates/adapter), which has all of that and a conformance test.

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

    /// Build the adapter for a site (its time zone and currency). The app
    /// calls this, not [`Adapter::new`]; the default ignores the site and
    /// calls `new`. Override it only to validate against or keep the site
    /// at build time; at run time it is [`AdapterContext::site`].
    fn build(config: &Self::Config, site: &Site) -> Result<Self, AdapterError> {
        let _ = site;
        Self::new(config)
    }

    /// What this instance can do, if it depends on its config (e.g. the
    /// levels of the configured device model). Must not exceed the
    /// descriptor. Defaults to the descriptor's capabilities.
    fn capabilities(&self) -> Capabilities {
        Self::descriptor().capabilities
    }

    /// This instance's series templates, if they depend on its config:
    /// the kind's, with the intervals the configuration sets (a poll
    /// interval becomes `expected_interval`, and so the staleness limit).
    /// Only the intervals may differ from the kind's, and an instance may
    /// leave out metrics it never emits. Defaults to the descriptor's.
    fn series(&self) -> Vec<SeriesTemplate> {
        Self::descriptor().series
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
        // Sampled every `interval` (the default's here), stamped by the
        // host clock.
        .with_series(SeriesTemplate::gauge(
            Metric::TEMPERATURE,
            Unit::Celsius,
            default_interval(),
        ))
        .with_series(SeriesTemplate::gauge(
            Metric::HUMIDITY,
            Unit::Percent,
            default_interval(),
        ))
    }
```

**Series templates.** Declare one `SeriesTemplate` per metric your kind emits as readings, and one per metric it emits as forecasts. A template says what the series holds (`SeriesKind::Gauge`, `Counter { wrap_at }`, `State` or `Text`), its unit, how often a value is expected (`expected_interval`), where `t` comes from (`TimeSource::Host` when you stamp it with the clock, `TimeSource::Device` when the source does), whether the source revises published values (`versioned()`), and whether it sends only changes (`record_on_change()`; `expected_interval` is then the keep-alive interval, not a poll interval). `with_staleness_limit` (default 3 × the interval) and `with_lateness_bound` (default 5 minutes) are optional. The core refuses a reading or forecast whose metric has no template, or whose unit differs from it. A template is fixed for the life of its series: a change of kind, value type, unit or versioning is a new series under a new metric name, never an edit. Where your configuration sets the poll interval, the descriptor's template states the default configuration's, and `Adapter::series` returns the instance's: the same templates with `with_expected_interval(configured)`, as the template crate does. Only the intervals may differ from the kind's; the app refuses anything else. A template's role says what the series is about: `SeriesRole::Measured` for readings (the default), `SeriesRole::Forecast` for modelled forecasts (`SeriesTemplate::forecast`: weather; always versioned, keyed by valid time) and `SeriesRole::Scheduled` for published values known in advance (`SeriesTemplate::scheduled`: tariff rates). The core refuses a `Basis::Modelled` forecast whose template is not a forecast template, and a `Basis::Published` one whose template is not scheduled. An optional retention class (`with_retention(Retention::Diagnostic)` for diagnostics such as host metrics; `LongTerm` by default) tells the store how long to keep the detail. Roles, retention classes and interval series are *provisional*, and forecast and scheduled series are not written anywhere yet.

**Provisional** means: shaped for the store heimr will write to, but not final. A provisional part may change when the store is added, and any change will be listed in the changelog. Use it as documented; everything not marked provisional is the contract as it stands.

A sensor declares `Capabilities::read_only()`. An instance with no write capability never gets a command channel, whatever its configuration says, and `mode = "live"` on it is a configuration error. An air-treatment adapter that accepts `CommandKind::FanMode` must also declare its fan levels (`with_fan_levels(1, 5)`) or named presets (`with_fan_presets(["night"])`); the core refuses others. If an instance can do less than its kind (say, the configured model has fewer levels), override `Adapter::capabilities`.

**Configuration** is a plain `serde` type for the adapter's own keys. The app handles `type`, `enabled`, `mode` and `safety` (so adapters cannot use those names) and rejects unknown keys by name, inside your config type too.

The site's time zone and currency are set once, in the required `[site]` section, and the core is their only source: **never declare a time-zone or currency key of your own**. They reach the adapter through `Adapter::build` (at prepare time) and `ctx.site()` (at run time). A standalone client library that takes a zone as an argument gets `ctx.site().time_zone()`. The demo's config:

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
        interval: config.interval,
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
| `ctx.reading(..)`, `ctx.forecast(..)`, `ctx.event(..)`, `ctx.emit(..)`, `ctx.emitter()` | Build and send readings, forecasts, state and events. `reading` and `event` stamp the instance name; the core refuses records that name another source or actor, and events of a reserved kind (`command`, `command_*`, `adapter_*`, `heimr*`). `emit` waits when the app is behind (back-pressure) |
| `ctx.forecast_horizon()` | How far past its issue time a forecast may reach (`[heimr] forecast_horizon`, 16 days by default); drop anything beyond it |
| `ctx.take_commands()` | The command handle. Yields only for a write-capable instance configured `live` |
| `ctx.now()`, `ctx.sleep_until(..)`, `ctx.sleep(..)` | All time comes from the context clock (wall time for timestamps, monotonic for `sleep`), so tests can use a fake one |
| `ctx.shutdown()` | Finish in-flight work and return promptly |
| `ctx.state_dir()` | Private, persistent directory for pairing keys, revision maps or caches |
| `ctx.site()` | The site's time zone and currency, from the required `[site]` section. Records stay UTC; use the zone only to find local days (`site.day_bounds(date)`, `site.local_date(t)`) and the currency for prices (in its major unit). Never hard-code either |
| `ctx.health()`, `ctx.metrics()` | Telemetry (section 5); the adapter can report health and metrics but not change its run state |

Adapters never see the bus, the store or other adapters. State records are keyed by (instance, zone or device id), so an instance cannot overwrite another's state whatever ids it uses.

A minimal read-only adapter polls, emits and honours shutdown. This is the example from the `Adapter` docs, compiled as a doc test:

<!-- quote: crates/heimr-core/src/adapter.rs#thermometer -->
```rust
use heimr_core::{Adapter, AdapterContext, AdapterDescriptor, AdapterError, Capabilities};
use heimr_core::{DeviceId, Metric, SeriesTemplate, Unit};
use jiff::SignedDuration;

#[derive(Debug, serde::Deserialize)]
struct Config { interval: SignedDuration }

struct Thermometer;

impl Adapter for Thermometer {
    type Config = Config;

    fn descriptor() -> AdapterDescriptor {
        AdapterDescriptor::new("thermometer", "Reads one temperature sensor", Capabilities::read_only())
            // One series per metric: what it holds, its unit, how often it comes.
            .with_series(SeriesTemplate::gauge(Metric::TEMPERATURE, Unit::Celsius, SignedDuration::from_secs(10)))
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

### Readings, forecasts and money

- A **reading** is a value at an instant. An amount over a fixed interval, such as consumption per half-hour or rainfall per hour, is still a reading, stamped at the **start** of the interval, in an interval series (`SeriesTemplate::interval(metric, unit, length, expected_interval)`, kind `SeriesKind::Interval`, *provisional*): `t` must be aligned to the length (a multiple of it since the Unix epoch, in UTC), and the value is the amount for `[t, t + length)`. A mean over an interval is not an amount: keep it a gauge. Intervals of varying length (local days, months) are not covered yet; leave those as gauges or scheduled values (a daily standing charge stays a scheduled gauge).
- **Versioned series.** A source that re-issues past values (a corrected price, consumption that settles later, every forecast) declares the series `versioned()` (forecast templates always are), and each reading or forecast carries `changed_at` (`with_changed_at`): the source's revision time or version, higher for each revision. It must be the same every time one revision is re-sent, so it is **never the time of the current fetch**. The core refuses a record of a versioned series without one, and a `changed_at` on any other series.
- **Non-finite values** (NaN, ±∞) are data, not errors: do not filter them out. The core passes them through unchanged and the store will record them as bad quality; `records.well_formed` notes them, so declare the note if your source produces them.
- A **forecast** (`Forecast`, built with `ctx.forecast(device, metric, value, unit, basis)`) is a value for a period `valid_from..valid_to` (end exclusive; equal for an instant), issued at `issued_at`. Its basis is `Basis::Published` for a value the source has committed to (a tariff rate) or `Basis::Modelled` for a prediction (weather). `issued_at` is when the source issued it, or when your adapter first saw it; the time-window rule for records applies to `issued_at`, not to the period. The core refuses a forecast whose period ends before it starts, or that ends further than the forecast horizon after its issue time.
- **Money** is in the site currency's **major unit**: `Unit::CurrencyPerKilowattHour` (`¤/kWh`) for prices, `Unit::Currency` (`¤`) for amounts such as a daily standing charge; 0.184, not 18.4 cents. The kit documents `unit_rate` in `¤/kWh` and `standing_charge` in `¤`, so money emitted as a plain number fails `records.units`. The currency a vendor quotes is a property of your kind, not configuration: declare it on the descriptor with `with_source_currency` (bind the code to a `const`, e.g. `const USD: Currency = Currency::of("USD");`). The app refuses an instance of your kind unless `[site].currency` matches, naming both currencies; heimr never converts. The core refuses money from a kind that declares no currency. A kind that emits no money declares none and must run for a site in any currency: never check the site's currency yourself. The kit's default test site quotes EUR, so a kind declaring another currency must run the kit with a site in that currency (`Suite::site`), or every scenario fails with the mismatch.

### Guidelines

- Prefer push (subscriptions, observe, event streams) over polling; when polling, respect the service's rate limits and make the interval configurable.
- Reconnect with backoff. On every start, and after an error, re-send what you may have lost (see below); never try to suppress repeats with a cursor.
- Use the context clock for every timestamp and sleep.
- Clone `ctx.emitter()` into sub-tasks rather than passing the context around.
- No secrets in logs; credentials come from configuration, which a site fills from its secret store.

### How heimr talks to the store

heimr will write what you emit to a time-series store, which is not part of heimr yet. It will be built so that sending something twice is free and losing something is not. These are the rules adapters follow today, so that their records are ready for it; the core and the test kit already enforce them:

- **Stable `t`.** A reading's time comes from the source (a device clock, a slot or hour in an API answer) or is taken once, when you sample. Never re-stamp a value when you send it again; for a forecast, keep its `issued_at` too.
- **Identical repeats.** The store will drop a repeat of the same series, `t`, value (bit for bit: `0.0` and `-0.0` differ), quality and `changed_at`. So a repeat must be identical in all of them, and a quality must not change between sends.
- **Re-send on start.** On every start, and after an error or an emit whose outcome you do not know, re-send the last few minutes, or for a polled source its whole last fetch window. `emit` returning `Ok` means the hub has the record, not that it is safely stored, so this is how anything lost in a crash will come back. Do not keep a cursor to avoid repeats; between starts, send only what is new or changed.
- **Versioned series and `changed_at`.** A source that revises published values declares the series `versioned()` and sends a `changed_at` with each record: the highest will be current, and an older one re-sent will never overwrite a newer correction. Use the source's revision time or version if it has one. If not, keep your own per key in `ctx.state_dir()`, written durably before you send: for each (series, `t`) only the last value and its `changed_at`; reuse it while the value is unchanged, and mint `max(now, stored + 1)` in nanoseconds when it changes, so a backward clock step cannot lower it. `heimr_core::Revisions` does exactly this. Never use the time of the current fetch. If the saved map is lost, start a new one and report health `degraded`: values seen again look newer, but nothing is reverted. If saving fails, send nothing you minted and retry. After the wall clock steps back, the marker of a changed value is still `stored + 1`, so it can run ahead of the clock (and of a forecast's `issued_at`); a forecast first seen at a time now in the future cannot be sent (its `issued_at` would be refused), so hold it back until the clock passes it again.
- **Back-pressure is a wait.** `emit` waits while the hub (and later the store) is behind. That is neither a refusal nor a failure: do not retry around it or drop records.

Over an interval series (*provisional*), heimr relies on window sums only, never on averages or integrals (the store will refuse them). A record-on-change source (one that sends only changes; never an interval series) also re-sends an unchanged value at least once per `expected_interval` of its template, as a keep-alive, or the series will read as stale. The test kit checks all of this: `records.monotonic` judges repeats run by run, and `records.keep_alive` the keep-alives.

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

## 6. Testing your adapter

Add `heimr-testkit` as a dev-dependency. Its conformance suite runs your adapter inside the real app, on a fake clock, through a fixed list of named scenarios derived from this contract: configuration handling (unknown, reserved and malformed keys, your own invalid cases, no I/O while building an instance), staying up, emitting, reporting health, record well-formedness (identifiers, documented units, non-finite values noted, money only from a kind with a source currency, forecast periods within the horizon, series templates and `changed_at` as declared, identical repeats and outranking corrections run by run across the reconnect or restart after an outage, keep-alives for record-on-change series), the site currency (a declared source currency must match the site's; a kind without one runs anywhere), stats, shutdown within a deadline at arbitrary points, shadow and live command handling through the actuator (commands audited, none delivered in shadow mode, no device writes in shadow mode or without a command), and behaviour when the upstream is unavailable, rejects credentials or returns garbage (no fatal exit on an outage, no hot retry loop, fatal on rejection, no panic). The test file of the template in [`templates/adapter`](../templates/adapter) is the whole suite against a replay of a recorded (here hand-built) sensor:

<!-- quote: templates/adapter/tests/conformance.rs#conformance -->
```rust
#[test]
fn conformance() {
    let cassette = Cassette::parse(include_str!("fixtures/sensor.json")).expect("a cassette");
    Suite::replay::<ExampleSensor>(
        cassette,
        |uri| json!({ "url": format!("{uri}/status"), "interval": "10s" }),
    )
    .invalid("url", json!({ "url": "not a url" }))
    .invalid("interval", json!({ "interval": "1s" }))
    .expect_skip("commands.shadow", "read-only sensor: no commands")
    .expect_skip("commands.live", "read-only sensor: no commands")
    .expect_note("records.keep_alive", "polled: no record-on-change series")
    .run()
    .assert_passed();
}
```

Each scenario reports pass, pass with a note, fail with the reason, or skip. A read-only adapter skips the command scenarios; an upstream that cannot simulate faults skips the fault scenarios; one that does not count traffic skips the write and I/O checks, or passes the scenario with a note naming what was not observed. `assert_passed` accepts none of these silently: declare each skip, note or accepted failure by scenario id with a reason (`.expect_skip(id, why)`, `.expect_note(id, why)`, `.expect_failure(id, why)`), as the template does for its command scenarios. The suite fails on anything undeclared, on a declaration for an unknown id, and on a declaration that does not happen (a declared skip that passes, a declared failure that passes or is skipped), so declarations cannot go stale; the failure message lists each offending id with its reason. `HEIMR_TESTKIT_ONLY=<id>[,<id>…]` runs only those scenarios; the rest are reported as filtered and their declarations are not checked, and an unknown id fails the run. Adapters whose data is synthetic use `Suite::new::<A>(settings)`; adapters with another kind of upstream (a socket, a file tree) implement the small `Upstream` trait to report traffic and simulate faults.

The kit cannot check every rule generically. Test these yourself: holds mapped to the device's native timed or next-block override; that a delivered command has the intended effect on the device; that a restart of the whole app re-sends your last window identically (restarts and reconnects within a run are checked; `Harness::restart` restarts an instance on the same state directory, and `checks::monotonic(&h.runs(), &series)` judges what it re-sent). That the audit record is durable before your adapter receives a command is the core's guarantee, not something your adapter or the kit has to prove.

For unit-level behaviour, `heimr_testkit::Harness` runs the adapter with a real context on a manual clock and keeps every record in memory; `heimr_testkit::render` turns records into stable text for snapshot tests, and `heimr_testkit::replay` serves recorded HTTP exchanges. Record through `Cassette::save` with a `Redactor`: it replaces what you name with placeholders, replaces credentials in headers and query strings, and scans what remains: the request path, query parameter names and values, response header names and values, JSON keys and values at any depth, and text bodies. It refuses to write while anything still looks identifying: runs of ten or more digits (epoch times under time-like keys, and dotted version strings under version keys, excepted), UUIDs, MAC and e-mail addresses, tokens, IP addresses (loopback and documentation ranges excepted), host names under host-like keys, unredacted credentials, values under id-like, serial, address and name keys, and precise coordinates. The scan is a backstop, not a guarantee: it only knows common shapes of identifying data, so read what you record. Resolve a finding by redacting the value or, if you have checked it identifies nothing, by allowing that location or key explicitly (`Redactor::allow`, `Redactor::allow_key`).

The app-level tests of the demo, in [`crates/heimr/tests/demo.rs`](../crates/heimr/tests/demo.rs), show how to go further by hand (driving time, submitting commands, reading the journal):

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
- [ ] Transient and fatal errors distinguished; the last window re-sent identically on start and after an error; health reported.
- [ ] One series template per metric emitted (readings, forecasts); `changed_at` on versioned series, stable across re-sends.
- [ ] Money in the site currency's major unit, with the kind's `source_currency` declared; no currency key in the config.
- [ ] `match`es on core types have wildcard arms.
- [ ] The `heimr-testkit` conformance suite passes (expected failures declared with a reason).
- [ ] Tests over redacted or hand-built fixtures, in shadow and live mode.
- [ ] Fixtures directory has a `README.md` saying how each fixture was made.
- [ ] README, docs and an example; `#![forbid(unsafe_code)]`.
