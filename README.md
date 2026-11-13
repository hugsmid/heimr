# heimr

A self-hosted, local-first home "brain" written in Rust. heimr is being built to record everything a home produces (temperature, humidity, air quality, weather, energy use and cost), serve dashboards over that history, and control devices against time-of-use energy prices — through adapters, with safety enforced in the core.

*heimr* is Old Norse for "home".

> **Status: early.** Interfaces change without notice and nothing is released yet. See [Status](#status).

## Principles

- **Local control.** The control path runs on your own hardware. Cloud calls are limited to data that only exists in the cloud (energy prices, weather forecasts, an optional LLM).
- **No inbound exposure.** Nothing listens on the internet: no port forwards, no public URL. Remote access is over a private overlay network / VPN that you run.
- **Heating fails safe.** The heating system's own schedule keeps the home warm if heimr is stopped, wrong or mid-deploy. heimr adjusts within a configured band, and every override carries a hold that the device itself ends; heimr never becomes the only thing standing between you and a cold house.
- **Verification over trust.** Protocol and API code is to be tested against specification examples, hand-built fixtures, recordings stripped of identifiers and, where one exists, a reference implementation, not just reviewed.
- **Shadow mode before live writes.** Every adapter instance that can write starts in shadow mode: its commands are checked and audited, never delivered, until you switch that instance to live.
- **Record everything.** Raw data is never thrown away; aggregates are derived and can be rebuilt.
- **Everything in code.** Configuration and deployment are declared, not clicked together.

## Architecture at a glance

```
            ┌──────────────────────── heimr (one process) ───────────────────────────┐
 devices ──▶│ adapters ──▶ event bus ──▶ state cache ──▶ dashboard / API / assistant │
 & APIs  ◀──│    ▲                │                                                  │
            │    └── commands ◀── planner (price-aware, clamped, shadow/live)        │
            │                     │                                                  │
            │                     └──▶ recorder ──▶ store ──▶ Parquet archive        │
            └─────────────────────────────────────────────────────────────────────────┘
```

This is the target design; [Status](#status) says what exists.

- **Adapters** turn a device or service into typed readings, state and events, and (if they can write) carry out commands. A site runs *instances* of an adapter kind, one `[adapters.<instance>]` section each with `type = "<kind>"`. An instance sees only what the core hands it and cannot emit as anyone else.
- **The core** owns the safety layer and supervision. Deciders (planner, assistant, user) get a submit-only handle. Every command is checked against the instance's capabilities, a setpoint band, a maximum hold and rate limits, written to an audit journal and fsynced before delivery, and delivered only to instances configured live. Each instance runs under its own supervisor: a failing one is restarted with backoff, or kept down, while the rest run on.
- **The recorder** (planned) writes every reading and decision to the store, with a local spool if the store is down.
- **Front-ends** (planned) read the in-memory state for live views and the store for history.

More detail: [`docs/architecture.md`](docs/architecture.md).

## How the pieces fit

| Piece | Path | What |
|---|---|---|
| Core | `crates/heimr-core` | Domain types, the adapter contract and the command safety layer |
| Test kit | `crates/heimr-testkit` | Adapter conformance suite, direct-drive harness, record/replay with redaction, snapshot rendering |
| Adapters | `crates/<name>` | Generic protocols and public services (see below) |
| App | `crates/heimr` | The application as a library (`App` builder, typed `Config`) plus a default binary with a synthetic `demo` adapter |
| Example configuration | `crates/heimr/heimr.example.toml` | A minimal configuration that runs with fictional values |
| NixOS modules | `nix/modules` (upcoming) | Reusable NixOS modules for the app and its supporting services, exported as flake `nixosModules` |
| Adapter template | `templates/adapter` | Starting point for a new adapter crate, with a conformance test |

Listing a crate here means it exists, nothing more. `octopus-energy` and `weather` are standalone client libraries, not yet wired into the app as adapters; `heimr-system` is an adapter, registered in the default binary. Which adapters run is decided entirely by your configuration.

| Crate | Integration | State |
|---|---|---|
| `octopus-energy` | Energy prices and smart-meter data from Octopus Energy (UK): tariffs, half-hourly Agile prices, consumption, live demand | Client library; implemented and tested against hand-built fixtures |
| `weather` | Weather observations and forecasts from Open-Meteo | Client library; implemented and tested against hand-built fixtures |
| `heimr-system` | The machine's own health: CPU, memory, disks, network, temperatures, processes and services (Linux) | Adapter, registered in the default binary as `system`; tested against hand-built fixtures |

Planned: price-aware planning of heating and switchable loads, derived analytics (cost, degree-days, heat-loss estimates), a provider-neutral LLM client for the assistant, and adapters for generic local protocols (HomeKit Accessory Protocol, Matter, ESPHome native API, MQTT).

## Status

- **Exists:** the adapter contract (kinds, configured instances, typed commands with holds); the command safety layer (policy checks, rate limits, a durable audit journal written before delivery, shadow mode by default, a submit-only handle for deciders); per-instance supervision and telemetry; typed configuration and the `App` builder; a synthetic `demo` adapter; a `system` adapter for host metrics; the adapter test kit and template; energy-price and weather clients.
- **Next:** reusable NixOS modules.
- **Under evaluation:** the storage engine. An embedded time-series engine plus a Parquet archive is the working assumption; candidates are being benchmarked. See [`docs/architecture.md`](docs/architecture.md#4-data-model-and-retention).
- **Not started:** the recorder (writing records to the store), dashboard, planner logic, analytics, assistant, native protocol stacks.

## Getting started

```bash
nix develop                   # toolchain and tools
nix flake check -L            # everything CI runs
cargo run -p heimr -- check-config crates/heimr/heimr.example.toml
```

To watch the demo adapter, point `state_dir` in a copy of the example at a writable directory and run `RUST_LOG=debug cargo run -p heimr -- --config <your copy>`; readings are logged at debug level, so without it you see only start-up and events. A deployment guide is still to come.

## Private adapters

Adapters for specific devices do not have to live here. An adapter is an ordinary crate that depends on `heimr-core`; a small binary in your own (private) repository registers it alongside the public ones:

```rust
fn main() -> std::process::ExitCode {
    heimr::cli::main("my-heimr", |b| {
        b.public_adapters().adapter::<my_adapter::MyDeviceAdapter>()
    })
}
```

Configuration decides which instances of the registered kinds run, and whether any is live. See [`docs/writing-an-adapter.md`](docs/writing-an-adapter.md).

## Contributing and security

- [`CONTRIBUTING.md`](CONTRIBUTING.md) — setup, checks, fixtures policy.
- [`SECURITY.md`](SECURITY.md) — threat model and how to report a vulnerability.
- [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md).

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

heimr is not affiliated with, endorsed by or sponsored by any device vendor or service provider named in this repository. Names are used only to describe compatibility.
