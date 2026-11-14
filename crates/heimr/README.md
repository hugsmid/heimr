# heimr

A config-driven home brain: registered adapter kinds run as supervised instances behind the command safety layer from `heimr-core`, with a durable audit journal, and feed sinks, wired together from one validated TOML file. Library plus the default `heimr` binary.

```bash
heimr check-config heimr.example.toml     # validate against the adapter kinds in this build
heimr --config heimr.toml                 # run until SIGINT/SIGTERM
```

A site with its own adapters builds a binary of three lines:

```rust,no_run
fn main() -> std::process::ExitCode {
    heimr::cli::main("my-site", |b| b.public_adapters() /* .adapter::<MyAdapter>() */)
}
```

## Config
See [`heimr.example.toml`](heimr.example.toml) (fictional values; every key documented). Unknown keys are errors, including inside adapter instance sections. Each `[adapters.<instance>]` section needs `type = "<kind>"`; several instances of one kind may run, and the instance name is the source of everything it emits. An instance runs unless `enabled = false`. The names `actuator`, `core` and `heimr…` are reserved for the core. Deciders get a submit-only handle bound to their actor name: `app.submitter("planner")?`.

`public_adapters()` registers `demo`, `system` (host metrics, `heimr-system`) and `open-meteo` (weather, `heimr-weather`). A kind that quotes prices declares its source currency, and an instance of it is refused unless `[site].currency` matches; `[heimr] forecast_horizon` (default 16 days) bounds how far ahead of their issue time forecasts may reach.

**Commands are delivered only with `mode = "live"` in that instance's section.** The default, shadow, sends every command through the same checks and audits it, but the instance has no command channel. `[safety]` sets the setpoint band (within 5.0–30.0 °C), the maximum hold, rate limits and whether heating may be turned off; `[adapters.<instance>.safety]` may narrow it and list heating-critical devices, never widen it.

## Audit journal
Every command decision (delivered, shadowed or rejected) is appended to `<state_dir>/audit/audit-NNNNNNNN.jsonl` and fsynced before the command is delivered; a command whose record is not durable within `[audit] append_timeout` is withheld. Every line carries the format version (`"v": 1`). Files rotate by size and the newest `retain_files` are kept. At start the journal's last hour rebuilds rate limits and the id sequence; lines that cannot be read (torn, unparsable, another version) count as accepted commands. A missing journal loses the hourly history, leaving the hold-off after start as the only limit.

## Supervision
A failing instance (error, panic, or returning while the app runs) is marked unhealthy and restarted with exponential backoff and jitter (`[supervision]`), rebuilt from its configuration; commands it had not received are audited as dropped and never replayed. A fatal error, or repeated panics (`max_consecutive_panics`, default 5, without a healthy run in between), keeps it down. Only core failures stop the app.

## Demo adapter
`demo` simulates heating zones: temperature and humidity readings (gauges stamped by the host clock, declared as series templates) plus zone state on the context clock, and it accepts setpoint commands with holds that expire like a real device's. Run the example config (point `state_dir` somewhere writable) to watch it: with the default `filter = "info"` you see lifecycle, health and command events; readings and state are logged at debug level, so use `RUST_LOG=info,heimr::sink=debug` (or that `[log] filter`) to see them. `tests/demo.rs` runs it on a fake clock and shows shadow mode withholding a setpoint that live mode applies.

## Sinks
Every record (readings, state, events, lifecycle events and copies of the audit records) goes to each sink: `LoggingSink` (on by default) and `MemorySink` (programmatic). Sinks never slow down the audit path. A storage sink is not yet implemented.

## Telemetry
Each instance's `run` executes in a `tracing` span carrying its name, kind and mode. `App::telemetry()` returns per-instance `AdapterStats` snapshots (records emitted, commands delivered/shadowed/rejected/dropped, health, run state, restarts, custom metrics); adapters write no counting code.

## Licence
MIT OR Apache-2.0.
