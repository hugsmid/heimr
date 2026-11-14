# weather

Rust client for the [Open-Meteo](https://open-meteo.com) weather APIs.

- **Forecast**: hourly forecast rows (`ForecastPoint`) with an explicit `issued_at`.
- **Historical weather (archive)**: reanalysis as `Observation` rows.
- **Historical forecast**: archived short-range forecasts as one continuous `Observation` series, covering days the archive has not reached yet.
- **Degree-days**: heating degree-days over hourly rows with a base temperature of your choice (constants for 15.5 °C, 18 °C and 65 °F), in total or per local day.

Hourly variables: temperature (2 m), relative humidity, wind speed (m/s) and direction (10 m), precipitation, cloud cover, shortwave radiation, surface pressure.

```rust,no_run
use weather::{Client, ForecastOptions, IssuedAt, Location};

# async fn run() -> Result<(), weather::Error> {
let client = Client::new()?;
let reykjavik = Location::new(64.15, -21.94)?; // an example location
let points = client
    .forecast(reykjavik, &ForecastOptions::days(2), IssuedAt::FetchTime)
    .await?;
println!("{} rows, first at {}", points.len(), points[0].valid_at);
# Ok(()) }
```

Example (Reykjavík): `cargo run -p weather --example forecast -- 64.15 -21.94`.

## Behaviour worth knowing
- **Time**: every request sends `timeformat=unixtime` and `timezone=GMT`; all times are `jiff::Timestamp` (UTC). Forecast rows start at 00:00 UTC today. Precipitation and radiation describe the hour *before* the timestamp.
- **Units**: requests ask for °C, m/s and mm, and `hourly_units` is checked. A unit change, a missing column, a column whose length differs from `time`, or a non-increasing time axis is an error rather than silently wrong data. Missing values are `None`.
- **`issued_at`**: `IssuedAt::FetchTime` records the instant before the request; `IssuedAt::At(t)` uses the instant you give. Conversion (`HourlyResponse::forecast_points`) is pure. Open-Meteo does not report the model run time per row, so `issued_at` is when you asked, not when the model ran.
- **Past forecasts**: the historical forecast API is a stitched series without issue times. For skill at a fixed lead time, Open-Meteo's Previous Runs API is the right source (not implemented yet).
- **Errors**: `{"error": true, "reason": …}` maps to `Error::Api`; HTTP 429 maps to `Error::RateLimited`.
- **Hosts**: forecast, archive and historical forecast are separate hosts, each overridable on `ClientBuilder`. Setting `api_key` sends `apikey=` and switches unset hosts to the commercial `customer-*` hosts; the key never appears in `Debug` output.

## Terms
Open-Meteo data is licensed under [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/): credit "Weather data by Open-Meteo.com" with a link when you show or share it. The free API is for **non-commercial use** and is limited to fewer than 10 000 calls a day (5 000 an hour, 600 a minute). See `API_NOTES.md` for the behaviour the client relies on and known gaps.

## Testing
Contract tests run against a mock server with hand-built fixtures (see `tests/fixtures/README.md`) and `insta` snapshots. Column-to-row conversion and degree-day maths are property-tested.

Not affiliated with or endorsed by Open-Meteo.

## Licence
Licensed under either of [Apache-2.0](../../LICENSE-APACHE) or [MIT](../../LICENSE-MIT) at your option.
