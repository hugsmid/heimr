# heimr-weather

`open-meteo`: a read-only heimr adapter for the [Open-Meteo](https://open-meteo.com) weather APIs, built on the [`weather`](../weather) client. It needs no API key.

- **Forecasts**: every `forecast_interval` it fetches the hourly forecast and emits each hour as a `Forecast` with `Basis::Modelled`. Hours already past, and hours beyond the app's `forecast_horizon`, are dropped.
- **Recent hours**: every `observed_interval` it fetches the last two days (`LOOKBACK` = 48 hours) from the historical-forecast API (archived short-range forecasts, which cover the days the reanalysis archive has not reached) and emits each hour that has passed as a `Reading` with `Quality::Estimated`.
- **Series**: each metric has a forecast series template (role `Forecast`, versioned, keyed by valid time) and a reading template (versioned, because reanalysis later replaces the stitched forecast for an hour; stamped by the source). The templates state the default intervals. Precipitation is an interval series of one hour (an amount, stamped at the hour's start); radiation, a mean over the hour, and the instants are gauges.
- **Revisions**: Open-Meteo reports no model run time or revision, so the adapter keeps one per series and hour in its state directory (`revisions.json`, a `heimr_core::Revisions` map bounded to the two windows): the last value seen and its `changed_at`, the time that value was first seen (strictly increasing per hour). A forecast's `issued_at` is that time too, so an unchanged hour is not re-issued by every fetch. The map is replaced durably (temporary file synced, renamed, directory synced on unix) before what it minted is sent.
- **Re-sending**: on every start, and after a failed fetch, the next good fetch of each endpoint re-sends its whole window, identically (same time, value bits, quality and `changed_at`, and a forecast's issue time), so the store will drop the repeats and recovers anything lost. Later fetches send only what is new or changed; a changed hour goes out with a higher `changed_at`. An empty or unreadable map is logged as an error naming the file, health is `degraded` until the next good round, and a new map is started: values seen again get new `changed_at` values that look newer than they are (the same values, re-issued; nothing is reverted).

All records are for device `outdoor`:

| Metric | Unit | Period |
|---|---|---|
| `temperature` | °C | the hour's instant |
| `humidity` (relative) | % | the hour's instant |
| `wind_speed` (10 m) | m/s | the hour's instant |
| `wind_direction` (10 m, where it blows from) | ° | the hour's instant |
| `cloud_cover` | % | the hour's instant |
| `pressure` (surface) | hPa | the hour's instant |
| `precipitation` | mm | sum over the hour before the row's time: a forecast valid for that hour, a reading stamped at its start |
| `shortwave_radiation` | W/m² | mean over the hour before the row's time, likewise |

A value missing from a row is not sent; a later fetch that has it sends it then. A row whose interval cannot be computed (a time at the edge of the representable range) is skipped with a warning. After the clock steps back, the request for recent hours never starts after now, so the adapter never sends an inverted date range, and a forecast hour first seen at a time now in the future is held back until the clock passes it (its issue time would be refused); a value that changes meanwhile still gets a higher `changed_at`. A request the service itself rejects (any 4xx other than 429) is treated as a configuration error and stops the instance.

## Configuration

```toml
[adapters.weather]
type = "open-meteo"
# Generic placeholder coordinates, not a real site: set the site's own,
# in its private configuration only.
latitude = 0.0                # -90 ..= 90
longitude = 0.0               # -180 ..= 180
# forecast_interval = "1h"    # 5m ..= 24h
# observed_interval = "3h"    # 15m ..= 24h
# forecast_days = 7           # 1 ..= 16
# models = "best_match"       # Open-Meteo model name(s); default: its choice
# forecast_url = "https://api.open-meteo.com"
# historical_url = "https://historical-forecast-api.open-meteo.com"
```

The coordinates identify a home: keep them out of anything you publish. `cargo run -p heimr-weather --example settings` validates settings without making a request.

## Failures

A failed request is retried after 30 s, doubling up to the forecast interval, with health `degraded` meanwhile. A rate limit (HTTP 429) is handled the same way, waiting at least as long as `Retry-After` asks (at most an hour). Malformed responses are failures like any other and never panic. HTTP 401 or 403, or a request Open-Meteo rejects as invalid (HTTP 400, such as an unknown model), is fatal: the instance stays down.

## Testing

Behaviour tests on `heimr_testkit::Harness` (re-sends after restarts and errors, bit for bit; revisions of changed hours; an unreadable map; a backward clock step; missing values) and the conformance suite (which declares `records.keep_alive` as a note: nothing is record-on-change) run against replay cassettes built from the `weather` crate's hand-built fixtures (`crates/weather/tests/fixtures/`, see its `README.md`), with their time axes moved to the kit's clock. No test makes a live request.

## Terms

Open-Meteo data is licensed under [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/): credit "Weather data by Open-Meteo.com" when you show or share it. The free API is for non-commercial use. Not affiliated with Open-Meteo.

## Licence

Licensed under either of [Apache-2.0](../../LICENSE-APACHE) or [MIT](../../LICENSE-MIT) at your option.
