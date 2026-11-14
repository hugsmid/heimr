# example-adapter (template)

A minimal, copyable heimr adapter crate: it polls a sensor's HTTP JSON endpoint and emits temperature and humidity readings. It is read-only; see `crates/heimr/src/demo.rs` for the command path.

## Requirements

Rust 1.99 or newer (heimr's minimum supported version; the template's `Cargo.toml` says so in `rust-version`). A copied template has no toolchain file, so install a recent stable Rust with [rustup](https://rustup.rs) (`rustup toolchain install stable`, or `rustup update stable` if you have it) and check with `cargo --version`.

## Use it

1. Copy this directory, rename the crate, the `ExampleSensor` type and the `example-sensor` kind.
2. Replace the path dependencies in `Cargo.toml` with git ones (the comment there shows how).
3. Replace `poll` with your device's protocol, and declare one series template per metric you emit on the descriptor (`SeriesTemplate::gauge(..)`; see the guide's "How heimr talks to the store"). Keep the rest of the shape: validate in `new` (no I/O), take all time from the context clock, report health, return `Ok(())` promptly on shutdown, return `AdapterError::fatal` only for what a restart cannot fix.
4. Record your device into `tests/fixtures/` through `Cassette::save` with a `Redactor`, and update `tests/fixtures/README.md`.
5. Register the kind in your site binary: `heimr::cli::main("my-site", |b| b.public_adapters().adapter::<my_adapter::MyAdapter>())`.

## Tests

- `tests/conformance.rs` runs the whole heimr conformance suite (configuration, lifecycle, shutdown at arbitrary points, record checks, stats, outages, rejected credentials, garbage) against a replay of the fixture. It is the only test most adapters need to start with. The sensor is read-only, so the command scenarios are skipped, and the test declares both skips with `expect_skip`; it is polled, so `records.keep_alive` passes with a note, declared with `expect_note`. An undeclared skip, note or failure fails the suite. When you add commands, or a record-on-change series, remove those declarations.
- `tests/behaviour.rs` shows the direct-drive `Harness` and stable rendering for snapshot-style assertions.

```bash
cargo test
```

## Licence

Licensed under either of Apache-2.0 or MIT at your option.
