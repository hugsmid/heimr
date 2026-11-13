# Architecture

The design of heimr: what it is for, the decisions behind it, how data and control flow, and how it is verified. Parts of this design are not built yet; where something is undecided it says so.

## Goals

A local, fully code-defined home brain in Rust that:
1. records detailed, permanent history of everything (temperature, humidity, air quality, weather, energy, cost);
2. serves a home dashboard and historical reporting;
3. controls devices (heating, ventilation, lighting, EV charging and other flexible loads) against time-of-use energy prices;
4. offers an LLM-backed assistant that can query history and, with confirmation, act.

---

## 1. Principles

1. **Rust everywhere it can go**, including protocol stacks that do not yet exist in Rust. Other languages as test oracles and as bridges over MQTT ("sidecars").
2. **Everything in code**: NixOS modules, the Cargo workspace, dashboards as code. UI only for unavoidable physical steps (device set-up, pairing codes).
3. **Record everything, forever**: raw data is never deleted; aggregates are derived and rebuildable. History is a first-class product.
4. **Verification over trust**: protocol code is to be proven against specification vectors, recorded device traffic and reference implementations (§10 says what is in place).
5. **Heating fails safe**: the heating system's own schedule keeps the home warm if heimr is dead, wrong or mid-deploy. Shadow mode before live writes.
6. **Local control path, no inbound exposure**: nothing listens on the internet; remote access only over a private overlay network / VPN. Cloud calls only where the data lives in the cloud (prices, weather, the LLM).
7. **Logic public, values private**: every line of logic builds and tests with example configuration; who, where and which devices live only in a site's own configuration (§6).

---

## 2. Decisions

| Topic | Decision |
|---|---|
| Deployment | One or more always-on machines; the reference NixOS modules (planned) assume a single host |
| Operating system | **NixOS** (flakes) as the reference platform; heimr runs as a static binary under systemd. Other Linux systems and containers work too |
| Language | **Rust stable (2024 edition)**, tokio |
| Home Assistant | Not used; heimr is a standalone system |
| Bus | **MQTT** for sidecars and MQTT devices (planned); any standard broker |
| Time-series store | **Under evaluation** (§4). Current implementation: PostgreSQL + TimescaleDB via `sqlx` |
| Archive and analysis | **Parquet**, partitioned by date and measurement, on local disk and object storage; readable by DuckDB, Polars and other engines |
| Dashboards | **Custom Rust web app (Leptos, SSR + WASM)** for live and historical views; an off-the-shelf operations dashboard for service health |
| Assistant | LLM-backed, through a minimal provider-neutral client (tool use, prompt caching, streaming) |
| HomeKit | A **HAP accessory bridge** exposing heimr's zones and sensors to HomeKit controllers (planned) |
| Remote access | A private overlay network / VPN; SSH keys |
| Licence | **MIT OR Apache-2.0** |

---

## 3. Integrations

heimr talks to devices and services through adapters (§7). *Supported* means an adapter exists or is planned; a site enables only what it has. The classes below are generic; no deployment needs all of them.

| Class | Data | Transport | Notes |
|---|---|---|---|
| **Energy prices** (e.g. Octopus Energy's Agile and other tariffs) | Half-hourly unit rates, standing charges, tariff changes | REST | `octopus-energy` |
| **Energy metering** | Half-hourly electricity and gas consumption; live demand where the supplier exposes it | REST, GraphQL | `octopus-energy` |
| **Weather** (e.g. Open-Meteo) | Observed and forecast temperature, humidity, wind, precipitation, cloud, shortwave radiation, pressure | REST | `weather`; forecasts keep `issued_at` so accuracy can be evaluated |
| **Climate** | Temperature, humidity, setpoint, heating demand, mode | Local protocols (HomeKit, Matter) preferred; cloud APIs as optional adapters | Planned |
| **Air quality** | PM2.5, CO₂, VOC, humidity; state of ventilation or air-treatment devices | Local protocols preferred | Planned; vendor-specific adapters are separate crates |
| **Lighting and switches** | On/off, level, colour, power where reported | HomeKit, Matter, MQTT | Planned; switchable loads for price-aware scheduling |
| **Covers** | Blinds, shutters, awnings: position, tilt | HomeKit, Matter, MQTT | Planned |
| **EV charging and storage** | Charge state, power, start/stop, current limit | Local protocols (OCPP, Modbus) preferred; vendor APIs as optional adapters | Planned; flexible loads for price-aware scheduling |
| **Irrigation** | Valve state, flow, soil moisture | MQTT, ESPHome native API | Planned |
| **Sensors** | Anything the device exposes | Matter, ESPHome native API, MQTT | Planned |
| **Derived** | Cost per half-hour, standing charges, degree-days, indoor–outdoor ΔT, heat-loss estimates, what-if comparisons between tariffs or appliances | Computed | Planned |

Sidecars are temporary bridges (often an existing Python or Node.js library) that publish to MQTT while a native Rust adapter is built. They double as differential oracles (§10).

---

## 4. Data model and retention

**The storage engine is undecided.** A benchmark compares, on the same seeded synthetic dataset (two years of home, power and ops data, with late revisions):

1. TimescaleDB (the current implementation) plus a nightly Parquet export;
2. GreptimeDB;
3. InfluxDB 3 Core;
4. DataFusion + Delta Lake or Iceberg with a hot buffer.

Criteria: correctness (idempotent writes, late and revised data, DST), memory footprint on modest hardware, disk per year, query latency over two or more years, direct Parquet reads from DuckDB/Polars, OTLP ingestion, Grafana support, crash recovery, backup and restore to object storage, licence and maturity. The result will be published with its reasoning.

The logical model is the same whichever engine wins:

- **Raw tables, never deleted:**
  - `reading(time, source, device, metric, value, unit, quality)` — narrow format for heterogeneous sensors.
  - Typed tables where queries benefit: `energy_halfhour(time, fuel, kwh, rate_p, cost_p, source)`, `price_halfhour(time, tariff, rate_p)`, `standing_charge`, `weather_obs`, `weather_forecast(issued_at, valid_at, …)`, `power_live(time, watts, import_kwh)`.
  - `event(time, kind, actor, payload)` — setpoint and mode changes, planner decisions with inputs and reasons, assistant actions.
- **Aggregates:** 5-minute, hourly and daily per metric; daily and monthly energy and cost per fuel. Aggregates keep `sum` and `n` so roll-ups are exact, and are refreshed explicitly after back-fills.
- **Compression** on older data; raw data retained indefinitely (small: tens of MB per month for a typical home).
- **Archive (planned):** nightly Parquet export of the previous day to local disk and object storage, schema-versioned, with a documented and tested restore and replay path.
- **Units and time:** SI units, °C, p/kWh. All timestamps UTC (`jiff`); daily and monthly buckets currently follow UK local time (`Europe/London`, the zone of UK half-hourly settlement); a configurable zone is planned. Half-hourly price slots are aligned to UTC half-hours; DST transitions (46/48/50-slot days) are tested.

Live views and stream analytics are to come from heimr's in-memory state and event streams (planned), not from the store.

---

## 5. Deployment

heimr runs on one or more always-on machines. The reference NixOS modules (planned) assume a single NixOS host running heimr and its supporting services; containers or several machines work as well.

- **Components, by role:** heimr itself; a time-series store (PostgreSQL with TimescaleDB, which `heimr-store` targets today); an MQTT broker where sidecars or MQTT devices are used; optionally a telemetry collector and an operations dashboard. How machines are provisioned is up to each site.
- **Network:** keep the control path local; nothing listens on the internet; a segregated IoT network is recommended where devices allow it.
- **Secrets:** an encrypted secret store. Never in the public repository, never in public CI.
- **Backups:** regular database dumps and the Parquet archive, copied off the machine; restore drills are part of operating it.

---

## 6. Repositories: public logic, private values

**Public — `heimr`:** all logic. Builds, tests and runs with example configuration only.
```
crates/heimr-core/      domain types, traits, adapter API, safety rules
crates/heimr-store/     schema, migrations, idempotent writes
crates/heimr/           the app as a library + default binary; heimr.example.toml
crates/<integration>/   generic protocols and public services
```
Upcoming: a test kit (conformance suite, record/replay, redaction), an adapter template, analytics, the assistant and reusable NixOS modules.

**Private — your own site repository:** values, infrastructure and any adapters you do not want to publish. A suggested layout:
```
flake.nix               pins heimr; builds the site binary; host configurations
site/                   network values, keys, heimr.toml, provisioning code
adapters/               private adapter crates
secrets/                encrypted
```

Deploys always come from the site repository, with heimr pinned in `flake.lock`; updates are deliberate.

Publishing rules for this repository: no secrets; no real readings, schedules or presence data; fixtures hand-built, or recorded and stripped of identifiers. CI runs gitleaks. Reverse-engineered protocols cite the public prior work they build on; vendor names appear only descriptively, with a "not affiliated" notice.

---

## 7. Core traits and the adapter model

Planner, assistant and recorder never depend on a specific adapter. They work with the domain types in `heimr-core`:

- **Records** an adapter emits: readings (shaped like the store's `reading` row: time, source, device, metric, value, unit, quality), typed state per device class, and events.
- **Device classes and commands**: climate (heating zones: set target, resume schedule, turn off), fan (air-treatment devices such as purifiers, fans or ventilation units: off, auto, a manual level or a named preset, limited to the levels and presets the adapter declares) and switch (on or off). An adapter declares which command kinds it carries out; the core refuses the rest.
- **Holds**: every command that overrides a device (a setpoint, heating off, a fan mode) carries a `Hold`: until the device's next schedule change, or for a bounded time (at most the policy's `max_hold`, 2 hours by default and never more than 24). Adapters map a hold to the device's own timed override, so the device reverts by itself if heimr stops.
- **Setpoints** are `Setpoint`s: they cannot leave an absolute envelope of 5–30 °C and are checked against the policy's band (15–24 °C by default, narrowed by configuration).

The exact types are documented in `heimr-core`.

- **Adapters** implement a small API. An adapter crate provides a *kind*; a site configures *instances* of it, one `[adapters.<instance>]` section each with `type = "<kind>"`, several per kind if needed. An instance runs with a context the core hands it and never sees the bus, the store or other instances. Its name is stamped as the source of everything it emits; records claiming another source or actor, or a kind reserved for core events, are refused. Guide: [`writing-an-adapter.md`](writing-an-adapter.md).
- **Safety lives in the core.** Deciders (planner, assistant, user) get a submit-only handle bound to their actor name. Every command goes through one `Actuator`, which checks the instance's capabilities, its safety policy (setpoint band, hold length, fan levels and presets, whether heating may be turned off) and rate limits (a minimum interval per zone or device and an hourly cap per instance; after a restart they are rebuilt from the audit journal, counting unreadable lines as accepted commands, with a hold-off after start). Resuming the schedule, the safe direction, is never rate-limited. Every decision (delivered, shadowed or rejected) is appended to a JSON-lines audit journal with actor, reason and inputs, and fsynced before the command is delivered; a command whose record is not durable within a deadline is withheld.
- **Shadow mode is the default.** A shadow instance has no command queue: its commands are checked, audited and counted against rate limits, never delivered. Going live is a per-instance configuration change.
- **Supervision**: each instance runs under its own supervisor. A failing instance is marked unhealthy and restarted with exponential backoff and jitter; a fatal error, or repeated panics, keeps it down while the rest of the app runs on. Commands it had not received are audited as dropped, never replayed. Only core failures stop the app.
- **Telemetry for free**: each instance runs in its own `tracing` span, and the core keeps per-instance stats (records emitted, commands delivered, shadowed, rejected, withheld or dropped, health, runs and restarts).

Planned, not yet built:

- An adapter may have a sidecar implementation (via MQTT) and a native one, chosen by configuration.
- The **recorder** subscribes to every record stream and writes to the store: batched, back-pressured, with a durable local spool when the store is unavailable.
- An in-memory **state cache** (`tokio::sync::watch`) serves reads instantly.

---

## 8. Control logic

None of this is built yet; it is the design the planner and automations will follow. The safety layer it relies on (§7) exists.

- **Climate:** comfort bands per zone, time-of-day profiles, weather-aware pre-heating or pre-cooling (forecast plus the building's learned response, so it starts just early enough), reacting to open windows or doors where sensors report them.
- **Price-aware load shifting:** EV charging, hot-water heating, batteries and other flexible loads scheduled into the cheapest or negative-price slots; alerts for unusually cheap periods.
- **Ventilation and air quality:** fans, ventilation units or purifiers driven by measured PM2.5, CO₂ or humidity, with quiet hours and maintenance alerts.
- **Lighting and covers:** scenes by time of day or daylight; blinds driven by sun position and indoor temperature.
- **Irrigation:** watering shortened or skipped when rain is forecast or recorded.
- **What-if analysis:** from recorded energy, indoor conditions, weather and prices, estimate what other tariffs or appliances would have cost.
- **Choosing between sources (where a site has more than one):** pick the cheapest way to deliver the same energy (price divided by efficiency at current conditions), use negative prices, coast through peaks.

**Failsafes (non-negotiable):**
- Devices keep their own schedules or safe defaults (a thermostat's fallback schedule, a charger's own timer); heimr never disables them.
- Writes are clamped and rate-limited; missing data means conservative defaults.
- Shadow mode first; live writes only after a deliberate switch and a failsafe drill.
- systemd watchdog plus alerts; every decision is logged to `event` with inputs and reason.

---

## 9. Dashboards, reporting and assistant

Planned; none of this is built yet.

**Primary UI — Leptos app:**
- Leptos (SSR + hydration), an axum server inside heimr, a Tailwind design system; charts via ECharts (`charming`) or uPlot for dense series; custom SVG/canvas where charts do not fit (price strip, per-zone maps).
- Data: typed JSON/WebSocket API over the in-memory state (live, pushed) plus store aggregates (history), with server-side downsampling (LTTB) so charts stay fast over years.
- Views: live state, energy and cost, comfort and air quality, weather, building performance, what-if comparisons, reports (period picker, comparisons, CSV/Parquet export). Controls with confirmation.
- Targets: any modern browser; responsive, installable as a PWA, usable in kiosk mode. Reachable on the LAN or the private overlay network only.

**Operations dashboard (provisioned as code; Grafana is the reference):** service health, adapter latency (OpenTelemetry), store size, rate-limit headroom for external APIs, ad-hoc exploration.

**View contents:**
- *Now:* per-zone temperature, humidity and setpoint; device states (lights, covers, chargers, fans); air quality; outdoor weather; live power; current and next prices.
- *Energy and cost:* half-hourly kWh and cost per fuel; daily and monthly totals including standing charges; average price achieved against a flat-rate benchmark; load-shift savings.
- *Comfort and air:* temperature and humidity distributions, time in band, air-quality exceedances, device runtime.
- *Building performance:* indoor–outdoor ΔT against heating energy, degree-day-normalised consumption, estimated heat-loss coefficient per zone, forecast accuracy.
- *What-if:* simulated cost of another tariff or appliance against actual cost by day and month.

**Reports:** weekly and monthly summaries written by the assistant from SQL/Polars results, sent to chat; CSV/Parquet export endpoints.

**Assistant (LLM-backed):** tools such as `get_zone_states`, `set_zone_target` (confirmation required), `get_prices`, `energy_report(period)`, `cost_breakdown(period)`, `air_quality(room, period)`, `explain_decision`. SQL-backed tools run read-only, parameterised queries. A fast-path router handles common commands without the LLM.
- **Front-ends:** chat integrations and a small authenticated HTTP endpoint for voice assistants or other automations; per-user roles; confirmation for anything that writes; LAN or overlay network only.
- **Latency targets:** fast path < 300 ms; LLM < 3 s p95. Persistent HTTP/2 client, prompt caching, OpenTelemetry spans per stage.

---

## 10. Verification strategy

In place today: `insta` contract snapshots and `proptest` in the API clients and the core, Docker-backed store tests, the safety-layer property tests, and the static checks below. Rows and items marked *planned* are not in the tree yet.

| Layer | Method |
|---|---|
| Crypto | *Planned, with the first protocol crates:* audited crates only (RustCrypto, `snow`, `rustls`); RFC test vectors (SRP RFC 5054, HKDF RFC 5869, ChaCha20-Poly1305 RFC 8439, Noise vectors) |
| Protocols | *Planned:* specification-example unit tests (e.g. HAP TLV8 and HAP-HTTP, protobuf frames) |
| Differential | *Planned:* existing open-source implementations of a protocol run on the same inputs as the Rust code; tests assert equivalence |
| Record/replay | *Planned:* device traffic captured into fixtures with identifiers removed; native adapters must replay identically |
| API contracts | Service responses snapshot-tested (`insta`) against hand-built fixtures; a scheduled live-contract job to flag schema drift is planned |
| Property and fuzz | `proptest` (parsers, DST slot alignment, the safety layer never exceeding its limits; cost maths and planner clamps as they land); `cargo-fuzz` on parsers is planned |
| Data integrity | Idempotent upserts and DST-correct buckets are tested; no gaps or duplicates on reconnect, spool replay and reconciliation against supplier totals are planned |
| Adapter contract | *Planned:* `heimr-testkit` conformance: monotonic timestamps, valid units, no duplicates across reconnect, no command without capability, nothing delivered in shadow mode, clean shutdown |
| Static | `clippy -D warnings`, `cargo-deny`, `#![forbid(unsafe_code)]`; `cargo-semver-checks` on published crates is planned |
| Shadow run | *Planned:* a native adapter is run in shadow alongside an existing integration and compared before it takes over |


---

## 11. Releases

- Publish a crate to crates.io once its docs are complete, examples run, parsing and maths are well covered by tests, fixtures are hand-built or stripped of identifiers, and a semver policy is set.
- Planned: `release-plz` for releases and changelogs; `cargo-semver-checks` in CI.
- First candidates: the energy-price and weather clients, then protocol crates as they mature.

---

## 12. Roadmap

In order, each stage ending with its tests and checks green:

1. **Foundation and data** — workspace, CI, NixOS modules, store and archive with a tested restore; energy-price, metering and weather data recorded and reconciled; storage decision published.
2. **Adapters and recorder** — adapter API, test kit and template; first device adapters, with fixtures stripped of identifiers.
3. **Dashboard** — design first, then live, energy-and-cost and comfort views on real data; later reports, building performance and what-if comparisons.
4. **Control in shadow** — climate scheduling, load shifting and ventilation automation compute and log decisions without writing; simulations and property tests green.
5. **Control live** — clamped writes enabled per adapter after a failsafe drill.
6. **Assistant** — tools, confirmations for writes, front-ends, periodic reports.
7. **Native protocol adapters** — HomeKit, Matter, ESPHome and other local protocols in Rust, each taking over only after a clean shadow run.
8. **Open-source releases** — crates published as they meet §11.
9. **Source optimisation (optional)** — choosing between sources or appliances that can do the same job by cost of delivered energy, with efficiency fitted from metered data.

---

## 13. Conventions

Rust stable, 2024 edition; tokio; `thiserror` in libraries, `anyhow` in binaries; `tracing`; `jiff`; `sqlx` with compile-time checked queries. `#![forbid(unsafe_code)]` in every crate. Small changes with tests; CI green. Concise docs, British English. Details: [`CONTRIBUTING.md`](../CONTRIBUTING.md).
