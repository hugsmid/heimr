# heimr-store

TimescaleDB schema for the data model in `docs/architecture.md` §4 as `sqlx` migrations, plus idempotent write helpers.

- `migrations/20261112000001_raw_tables.sql`: raw hypertables (`reading`, `energy_halfhour`, `price_halfhour`, `weather_obs`, `weather_forecast`, `power_live`, `event`) + `standing_charge`. Every `event` row names its `source` (the adapter instance or core component that emitted it) and its `actor` (who asked).
- `migrations/20261112000002_continuous_aggregates.sql`: continuous aggregates: `reading_5m → 1h → 1d`, `energy_daily → monthly`, `price_daily`. Daily/monthly buckets are Europe/London days; aggregates keep `sum`+`n` so roll-ups are exact.
- **Back-fills**: refresh policies only look back a fixed window (1 day for `reading_5m` … 3 months for `energy_monthly`). After writing rows older than that (spool replay, historic import, late supplier revisions) call `refresh_aggregates(pool, from, to)`, which refreshes every aggregate bottom-up over whole buckets. If a background policy is refreshing the same aggregate at that moment, TimescaleDB refuses with SQLSTATE `55P03`; `refresh_aggregates` retries with bounded backoff (`refresh_aggregates_with` takes an explicit `RefreshRetry`).
- `migrations/20261112000003_compression.sql`: compression policies. **No retention policies** — raw data is kept forever (a test enforces this).

## Changing queries
Queries use `sqlx::query!` (compile-time checked). After editing SQL, regenerate the offline cache against a migrated database and commit `.sqlx/`:
```bash
docker run -d --name tsdb -e POSTGRES_PASSWORD=pw -e POSTGRES_DB=home -p 55432:5432 timescale/timescaledb:2.27.1-pg17
```
```bash
DATABASE_URL=postgres://postgres:pw@localhost:55432/home sqlx migrate run --source crates/heimr-store/migrations && DATABASE_URL=postgres://postgres:pw@localhost:55432/home cargo sqlx prepare --workspace
```

## Tests
`cargo test -p heimr-store` starts a TimescaleDB container with background workers disabled, so policies never race the tests (needs Docker; skips without it unless `REQUIRE_DOCKER_TESTS=1`). Covers: schema shape, idempotent inserts (incl. into compressed chunks), slot alignment constraints (session-time-zone independent), back-fill refresh at every aggregate level, retry after a (deterministically provoked) concurrent refresh, `event.source` required, DST-correct daily buckets (46/48/50 slots), exact hierarchical means.
