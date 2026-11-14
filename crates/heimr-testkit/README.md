# heimr-testkit

Test kit for heimr adapters. A new adapter's conformance test is one call:

```rust,ignore
#[test]
fn conformance() {
    Suite::new::<MyAdapter>(json!({ "interval": "10s" }))
        .invalid("interval", json!({ "interval": "0s" }))
        .expect_skip("fault.rejected_is_fatal", "the device has no credentials")
        .run()
        .assert_passed();
}
```

- **Conformance suite** (`Suite`): a fixed catalogue of named scenarios derived from the adapter contract, run inside the real app (supervisor, actuator, stats) on a fake clock. Each reports pass; pass with a note naming the part the fixture could not observe (for example device writes, when the upstream does not count traffic); fail with the reason; skip with the reason; or expected failure. The summary counts each separately. `Report::assert_passed` is strict: every skip, note and accepted failure must be declared on the suite by id with a reason (`expect_skip`, `expect_note`, `expect_failure`); it fails on anything undeclared, on a declaration for an unknown id, and on a declaration that did not happen, listing each offending id with its reason. `HEIMR_TESTKIT_ONLY=<id>[,<id>…]` runs only those scenarios (the rest are reported as filtered, their declarations unchecked); an unknown id fails the run. When `CI` is set, the variable is ignored and the run fails, so a filtered run cannot pass in CI.
- **Harness** (`Harness`): one adapter with a real context on a manual clock, records in memory, commands through a real actuator with an in-memory audit log. `Harness::restart` rebuilds the instance and runs it again on the same state directory and clock, as a restart of the whole app would; `Harness::runs` splits the records by run, so a test can check what is re-sent across it.
- **Record/replay** (`replay`, feature `replay`, default): JSON cassettes served by a local `wiremock` server that counts requests and writes and can simulate outages, rejected credentials and garbage. `Cassette::save` redacts, then scans the request path, query parameter names and values, response header names and values, JSON keys and values at any depth, and text bodies, and refuses to write while anything looks identifying: digit runs of ten or more (epoch times under time-like keys, and dotted version strings under version keys, excepted), UUIDs, MAC and e-mail addresses, tokens, IP addresses (loopback and documentation ranges excepted), host names under host-like keys, unredacted credentials, values under id-like, serial, address and name keys, and precise coordinates. A location or key can be allowed explicitly. The scan is a backstop, not a guarantee: it knows common shapes of identifying data only, so read what you record.
- **Rendering** (`render`): stable line-per-record text for snapshot tests.
- **Checks** (`checks`): the record checks as plain functions.

Run the suite against the `demo` adapter: `cargo run -p heimr-testkit --example conformance`.

## Scenarios

| Id | The adapter must |
|---|---|
| `descriptor.valid` | have a valid kind name, summary, capabilities and series templates (one per metric for readings and one for forecasts, each consistent) |
| `config.accepted` | accept the example settings, with instance capabilities within the descriptor |
| `config.rebuild_stable` | give the same capabilities when rebuilt from the same settings (every restart rebuilds) |
| `config.unknown_key` | refuse an unknown key as a configuration error naming it |
| `config.reserved_keys` | not declare keys owned by the app (`type`, `enabled`, `mode`, `safety`) or the site (time zone, currency) |
| `config.currency_matches_site` | if the kind declares a `source_currency`, run only for a site in that currency (the app refuses another, naming both); if it declares none, run for a site in any currency (never check the site's currency itself) |
| `config.invalid_values` | refuse each listed invalid setting as a fatal configuration error naming its key |
| `config.malformed_no_panic` | refuse settings of the wrong shape or type without panicking, never as a transient error |
| `config.no_io_in_new` | do no upstream I/O while an instance is built from settings (valid and invalid); skipped without a traffic count |
| `run.stays_up` | neither return nor fail before shutdown |
| `run.emits` | emit records (opt out with `.emits(false)`) |
| `run.reports_health` | report health, and report it ok when healthy |
| `records.no_refusals` | never have a record refused by the core (the reason names the last refusal) |
| `records.well_formed` | name itself, use valid identifiers and finite state values, stamp records no later than the clock and no earlier than the allowed back-fill (a forecast by its issue time); give forecasts an ordered period (`valid_from <= valid_to`) ending within the forecast horizon of their issue time; carry `changed_at` exactly on versioned series; stamp interval-series readings (and forecasts, by `valid_from`) at interval starts, aligned to the length. NaN and ±∞ in readings and forecasts pass with a note (passed through; the store will record them as bad quality) |
| `records.units` | declare series templates in the documented unit for well-known metrics (`unit_rate` in `¤/kWh` and `standing_charge` in `¤`, never a plain number); give every reading and forecast a template and its unit; use money units (`¤`, `¤/kWh`) only if the kind declares a source currency |
| `records.monotonic` | within one run, strictly increase reading timestamps per series that is not versioned; repeat any record identically (same value bits, quality and `changed_at`, and a forecast's issue time) across a restart, or on a versioned series within a run; give a changed value a higher `changed_at` on a versioned series (a changed value on any other series fails); never move state back in time. Checked over a healthy run, an injected outage and the recovery after it, run by run (whether the adapter reconnects or is restarted by the supervisor); noted as one run only when the upstream cannot simulate an outage |
| `records.restart` | after a restart of the whole app on the same state directory, upstream and clock, re-send identically (the `records.monotonic` rules over both runs): what the adapter keeps in its state directory, such as revision markers, must survive; skipped by a driver that cannot restart on the same state |
| `records.keep_alive` | for every record-on-change series template of the instance, send the value (at least once, and fail if never) and re-send it at least once per expected interval while it is unchanged (measured per device over four of the longest such intervals on the fake clock); passes with a note when no template is record-on-change, so polled adapters declare the note |
| `telemetry.counted` | have the core's stats match what it emitted (a check of the core's counting; no adapter can fail it) |
| `shutdown.prompt` | return `Ok` within the deadline after a healthy run |
| `shutdown.any_point` | return `Ok` within the deadline when shutdown comes at start-up, mid-interval, on a tick or at pseudo-random points |
| `commands.shadow` | in shadow mode: every command audited as shadowed, none delivered, no device writes (noted as unobserved without a traffic count) |
| `commands.no_unsolicited_writes` | write nothing without a command; skipped without a traffic count |
| `commands.live` | in live mode: every command delivered with a delivered audit record; survive every declared kind; stop promptly with a command in flight |
| `fault.transient_recovers` | not fail fatally on an outage, not report ok during it, emit again (and report ok) after it; the last part is noted as unchecked for adapters declared not to emit |
| `fault.no_hot_loop` | retry an unavailable upstream at a bounded rate |
| `fault.rejected_is_fatal` | stay down (fatal) without restarts when credentials are rejected |
| `fault.malformed_no_panic` | not panic or emit invalid records on garbage input |

## What the kit cannot check

Some rules of the adapter contract depend on the device, so no generic scenario covers them; test them in the adapter's own tests:

- holds mapped to the device's native timed or next-block override, and no open-ended overrides;
- that a delivered command has the intended effect on the device;
- that the audit record is durable before an adapter receives a command (the core's guarantee, tested in the core; `commands.live` sees only outcomes and the audit trail);
- I/O at other points than building an instance and running it, or I/O not counted by the upstream;
- vendor rate limits beyond the retry rate during an outage;

The kit keeps a short table of documented units for well-known metrics (`temperature` in °C, `unit_rate` in `¤/kWh`, …): these are project-wide conventions, so `records.units` applies them to the kind's series templates; every other unit comes from the templates.

In the app, the core already refuses bad forecasts, records without a series template or in another unit than their template's, and money from a kind without a source currency, so in-process those surface in `records.no_refusals`; `records.well_formed` and `records.units` check the records themselves for drivers that do not sit behind the core's emitter.

Scenarios use only the `Driver` trait, so a runner for adapters in other processes can implement it and reuse the same catalogue with `run_scenarios`.

## Licence

Licensed under either of [Apache-2.0](../../LICENSE-APACHE) or [MIT](../../LICENSE-MIT) at your option.
