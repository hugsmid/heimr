# Changelog

All notable changes to the heimr crates. The crates are pre-1.0: a minor release may break the API, and every break is listed here.

## Unreleased

Changes since commit 8ee1ef4 (the adapter test kit). The crates have no published version yet.

### Breaking

- **Series templates are required.** Every adapter kind declares a `SeriesTemplate` per metric it emits as readings (and per metric it emits as forecasts) on its descriptor (`AdapterDescriptor::with_series`). The core refuses a reading or forecast whose metric has no template, whose unit differs from it, or (for an interval series) whose time does not start an interval.
- **Non-finite values pass through.** NaN and ±∞ in readings are forwarded unchanged instead of refused. Removed: `ReadingError`, `Reading::validate` and `EmitError::Invalid`. The test kit's `records.well_formed` notes such values instead of failing them. In JSON a non-finite value is now a string (`"NaN"`, `"inf"`, `"-inf"`), so it reads back.
- **Versioned series carry `changed_at`.** On a series declared `versioned()`, every reading carries `changed_at` (`Reading::with_changed_at`); on any other series it must not. The core refuses either mistake.
- **Repeats are judged, not refused.** The kit's `records.monotonic` no longer fails a repeat across a restart. It fails a repeat that is not identical (value bits, quality, `changed_at`), and a changed value on a series that is not versioned. Within one run, new readings of such a series must still be newer than the last.
- `AdapterDescriptor` and `heimr::AdapterPlan` no longer implement `Eq` (a series template holds an `f64`); `PartialEq` remains. `AdapterPlan` gains `series`.
- `Adapter::series` and `AdapterInstance::series`: an instance whose configuration sets its poll interval returns its templates with that interval. Both have defaults, but the app now refuses instance templates that differ from the kind's in more than their intervals.
- `ContextParts::new` gives the emitter the context's clock.
- `system`: `top` (the top-N processes) now defaults to 0.

### Added

- `SeriesTemplate` (gauge, counter, interval, state and text kinds; unit, expected interval, time source, versioned, record-on-change, staleness limit, lateness bound; role and retention class, which are provisional) and `SeriesError`.
- `Forecast` records (`Basis::Published` or `Basis::Modelled`, a period and an issue time, checked against `[heimr] forecast_horizon`), money units (`¤`, `¤/kWh`) with a kind's declared `source_currency`, and units for irradiance and direction.
- `Revisions`: stable `changed_at` markers for sources that publish none (per key, the last value and its marker), with `Revisions::load` and `Revisions::save`, and `write_durably`, the shared durable writer for state files. `revision_time` turns a time into a marker.
- `record::value_serde`: record values round-trip through JSON bit for bit.
- The `open-meteo` weather adapter (`heimr-weather`): hourly forecasts and recent hours, re-sent on start with stable markers.
- `system`: a `series` allow-list and a `max_series` ceiling.
- Test kit: the scenarios `records.restart` (a restart on the same state directory re-sends identically), `records.keep_alive` (record-on-change series send keep-alives) and `config.currency_matches_site`; interval-alignment and versioning checks in `records.well_formed`; template checks in `records.units`; `Driver::runs`, `Driver::restart`, `Driver::series` and `Harness::runs`; `checks::versioning`, `checks::intervals` and `checks::non_finite`. `HEIMR_TESTKIT_ONLY` is refused when `CI` is set.
- Redaction scan: a dotted version string under a version key is no longer counted as a long digit run (a four-part version such as `1.2.3.4` still trips the IPv4 check); redaction replaces request and trace id header values (`x-request-id`, `cf-ray`, …) instead of refusing them.

### Changed

- Redaction scan: query parameters and headers under identifying names are checked as JSON keys are; a serial split into short groups is not taken for a version; in strings, digits after a decimal point count towards a long digit run (only JSON numbers keep the exemption), and any Unicode digits count.

### Migrating an adapter

1. Declare a template per metric on the descriptor: `SeriesTemplate::gauge(metric, unit, interval)` (or `counter`, `interval`, `forecast`, `scheduled`); `.with_t_source(TimeSource::Device)` if the source stamps the time.
2. If the poll interval is configurable, implement `Adapter::series` to return the templates with `with_expected_interval(configured)`.
3. Stop filtering NaN and ±∞; send them as they come.
4. If you kept a cursor to avoid repeats, drop it: on every start, and after an error, re-send your last window, identically. For a source that revises published values, declare the series `versioned()` and send `changed_at`, kept per key with `Revisions` and saved before you send.
5. Run the conformance suite, and declare `expect_note("records.keep_alive", …)` if no series is record-on-change.
