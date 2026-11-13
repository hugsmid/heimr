# heimr-core

Domain types, the adapter contract and the command safety layer for heimr.

- **Types**: `ZoneId`, `DeviceId`, `Reading` (time, source, device, metric, value, unit, quality), `Event` (kind, actor, JSON payload), climate, fan (air treatment) and switch state and commands. Every command that overrides a device carries a `Hold`: until the device's next schedule change, or for a bounded duration. Setpoints are `Setpoint`s, built against a `SetpointBand` (conservative default 15–24 °C, absolute envelope 5–30 °C) and re-checked by the actuator. All types adapters build or match are `#[non_exhaustive]`, with constructors.
- **Adapter API**: implement the typed `Adapter` trait (`descriptor()`, `new(&Config)`, `async fn run(self, ctx)`); `TypedFactory` adapts it to the object-safe `AdapterFactory`, which a host for adapters in other processes implements directly with a descriptor received at run time. Descriptors are owned data. The `AdapterContext` holds the typed config, an emitter, a command handle, a clock, a shutdown signal and a state directory. Adapters never see the bus, the store or each other, and cannot emit as another source or actor, or with a core-reserved event kind.
- **Telemetry for free**: the core counts what each instance emits and every command offered, delivered, shadowed, rejected, withheld or dropped, plus runs and restarts, in one serialisable `AdapterStats`. Adapters report health in one line (`ctx.health().degraded("…").await?`) and can keep custom counters and gauges (`ctx.metrics()`).
- **Safety**: the `Actuator` sits between deciders and adapters; deciders get a submit-only `CommandSubmitter`, bound to their actor name when created (core names are refused). It enforces each instance's `SafetyPolicy` (the global one, possibly narrowed per instance): setpoint band, maximum hold, declared fan levels and presets, the heating-off rule (which also covers heating-critical switches) and rate limits on monotonic time. `ResumeSchedule` is never rate-limited. Every decision is written to an `AuditLog` and awaited until durable before anything is delivered; a command whose record is not durable in time is withheld. Delivery happens only to instances configured `live` and running. **Shadow mode is the default**, and a shadow instance has no command queue at all.
- **Restarts**: command ids (`CommandId`: boot number + sequence) are unique and ordered across restarts; rate limits are rebuilt from the audit tail, counting unreadable entries as accepted commands, with a hold-off after start (on by default). A missing audit trail loses the hourly history; the hold-off is then the only limit.
- **Adapters see a restricted view**: `ContextParts` carries `InstanceStats` (health and metrics only), never the supervisor's `StatsHandle`. `AdapterFactory::prepare` is async, so an out-of-process host can validate settings and discover capabilities over its protocol.

The crate docs carry runnable examples (the crate root, `Adapter`, `SafetyPolicy`, `Setpoint`); `heimr::demo` is a complete adapter.

## Design notes
- `run` is a native `async fn` in a trait (`-> impl Future + Send`): no macro. `TypedFactory` boxes the one future per run.
- Command ids are a boot number (above every id in the audit trail, and at least the Unix second at start) plus a sequence, rather than random ULIDs: no extra dependency, and their order does not depend on the wall clock moving forwards.
- Clocks have wall time (for records) and monotonic time (for intervals): `SystemClock`, `TokioClock` (deterministic under `tokio::time::pause`), `ManualClock` (moves only when told; can step the wall clock alone).

## Testing
Unit tests plus property tests: setpoints never leave their band; over arbitrary command sequences with restarts, backward clock steps and audit failures and timeouts, shadow instances receive nothing, nothing is delivered without a durable audit record, delivered setpoints and holds stay within policy, rate limits and the boot hold-off hold in real time, and ids stay unique and ordered; provenance cannot be forged.

## Licence
MIT OR Apache-2.0.
