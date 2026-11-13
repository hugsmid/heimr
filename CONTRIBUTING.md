# Contributing

Thanks for your interest. heimr is early and its interfaces move; for anything larger than a small fix, open an issue first so we can agree the approach.

## Development set-up

You need [Nix](https://nixos.org/download) with flakes enabled. Everything else comes from the dev shell:

```bash
nix develop        # Rust toolchain (pinned in rust-toolchain.toml) and tools
```

`direnv allow` enters the shell automatically if you use direnv.

## Checks

Run before pushing:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace          # store tests need Docker and are skipped without it
cargo insta review              # accept snapshot changes deliberately
nix flake check -L              # everything CI runs, hermetically
```

CI also runs `cargo-deny` and gitleaks. A pull request needs CI green.

## Code

- Rust stable, 2024 edition; tokio; `thiserror` in libraries, `anyhow` in binaries; `tracing`; `jiff` for time.
- `#![forbid(unsafe_code)]` in every crate.
- Crypto only from audited crates (RustCrypto, `snow`, `rustls`).
- Every crate has a README and crate-level docs; client libraries also ship a runnable example.
- Tests come with the change: `proptest` for parsers and maths, `insta` snapshots for API contracts; specification vectors for protocols and record/replay for adapters once those exist (planned). See [`docs/architecture.md`](docs/architecture.md#10-verification-strategy).
- Docs: concise, British English.

## Commits and pull requests

- Small, focused commits, each building and passing tests.
- Commit messages: a short imperative subject with an area prefix (`octopus-energy: handle paginated tariffs`, `docs: …`), a blank line, then why — not what — if it is not obvious.
- No sign-off (DCO) is required.

## Fixtures

Recorded API or device data lives in the owning crate's `tests/fixtures/`.

- **Hand-built, or recorded and stripped of identifiers.** Never commit raw recordings.
- **Never real readings, schedules or presence** — occupancy patterns are the most sensitive data a home produces. Synthesise them.
- Replace identifiers with obvious placeholders: all-zero meter numbers and serials (`0000000000000`, `0000000000`), `A-00000000` for accounts, documentation-range IP and MAC addresses (`192.0.2.0/24`, `00:00:5e:00:53:xx`), `example.com` addresses. Drop names, addresses and locations.
- Say how each fixture was made in a sibling `README.md`.
- Prefer ISO 8601 timestamps unless the API itself returns another format.

## Adapters

New adapters for generic protocols and public services are welcome here. Adapters for specific vendors' devices are equally welcome, but you can also keep one in your own repository; see [`docs/writing-an-adapter.md`](docs/writing-an-adapter.md). Use vendor names descriptively only and cite the public prior work a reverse-engineered protocol builds on.

## Licence

heimr is dual-licensed under MIT or Apache-2.0. Unless you state otherwise, any contribution you intentionally submit for inclusion is licensed the same way, without additional terms or conditions.

## Conduct

This project follows the [Code of Conduct](CODE_OF_CONDUCT.md). Security issues go through [`SECURITY.md`](SECURITY.md), not public issues.
