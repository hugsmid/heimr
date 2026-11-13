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

- **Conformance suite** (`Suite`): a fixed catalogue of named scenarios derived from the adapter contract, run inside the real app (supervisor, actuator, stats) on a fake clock. Each reports pass; pass with a note naming the part the fixture could not observe (for example device writes, when the upstream does not count traffic); fail with the reason; skip with the reason; or expected failure. The summary counts each separately. `Report::assert_passed` is strict: every skip, note and accepted failure must be declared on the suite by id with a reason (`expect_skip`, `expect_note`, `expect_failure`); it fails on anything undeclared, on a declaration for an unknown id, and on a declaration that did not happen, listing each offending id with its reason. `HEIMR_TESTKIT_ONLY=<id>[,<id>…]` runs only those scenarios (the rest are reported as filtered, their declarations unchecked); an unknown id fails the run.
- **Harness** (`Harness`): one adapter with a real context on a manual clock, records in memory, commands through a real actuator with an in-memory audit log.
- **Record/replay** (`replay`, feature `replay`, default): JSON cassettes served by a local `wiremock` server that counts requests and writes and can simulate outages, rejected credentials and garbage. `Cassette::save` redacts, then scans the request path, query parameter names and values, response header names and values, JSON keys and values at any depth, and text bodies, and refuses to write while anything looks identifying: digit runs of ten or more (epoch times under time-like keys excepted), UUIDs, MAC and e-mail addresses, tokens, IP addresses (loopback and documentation ranges excepted), host names under host-like keys, unredacted credentials, values under id-like, serial, address and name keys, and precise coordinates. A location or key can be allowed explicitly. The scan is a backstop, not a guarantee: it knows common shapes of identifying data only, so read what you record.
- **Rendering** (`render`): stable line-per-record text for snapshot tests.
- **Checks** (`checks`): the record checks as plain functions.

Run the suite against the `demo` adapter: `cargo run -p heimr-testkit --example conformance`.

## Scenarios

| Id | The adapter must |
|---|---|
| `descriptor.valid` | have a valid kind name, summary and capabilities |
| `config.accepted` | accept the example settings, with instance capabilities within the descriptor |
| `config.rebuild_stable` | give the same capabilities when rebuilt from the same settings (every restart rebuilds) |
| `config.unknown_key` | refuse an unknown key as a configuration error naming it |
| `config.reserved_keys` | not declare keys owned by the app (`type`, `enabled`, `mode`, `safety`) or the site (time zone, currency) |
| `config.invalid_values` | refuse each listed invalid setting as a fatal configuration error naming its key |
| `config.malformed_no_panic` | refuse settings of the wrong shape or type without panicking, never as a transient error |
| `config.no_io_in_new` | do no upstream I/O while an instance is built from settings (valid and invalid); skipped without a traffic count |
| `run.stays_up` | neither return nor fail before shutdown |
| `run.emits` | emit records (opt out with `.emits(false)`) |
| `run.reports_health` | report health, and report it ok when healthy |
| `records.no_refusals` | never have a record refused by the core |
| `records.well_formed` | name itself, use valid identifiers and finite values (state included), stamp records no later than the clock and no earlier than the allowed back-fill |
| `records.units` | use the documented unit for well-known metrics and one unit per series |
| `records.monotonic` | strictly increase reading timestamps per series and never move state back in time, over a healthy run, an injected outage and the recovery after it (whether the adapter reconnects or is restarted by the supervisor); noted as one run only when the upstream cannot simulate an outage |
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
- no duplicate readings across restarts of the whole app (each run starts a fresh app and clock; restarts and reconnects within a run are checked);
- that the audit record is durable before an adapter receives a command (the core's guarantee, tested in the core; `commands.live` sees only outcomes and the audit trail);
- I/O at other points than building an instance and running it, or I/O not counted by the upstream;
- vendor rate limits beyond the retry rate during an outage;
- units of metrics the kit does not know (only well-known metrics have a documented unit; any series must keep one unit).

Scenarios use only the `Driver` trait, so a runner for adapters in other processes can implement it and reuse the same catalogue with `run_scenarios`.

## Licence

Licensed under either of [Apache-2.0](../../LICENSE-APACHE) or [MIT](../../LICENSE-MIT) at your option.
