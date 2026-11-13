# Open-Meteo API notes

Behaviour the client relies on, from Open-Meteo's documentation (`/en/docs`, `/en/docs/historical-weather-api`, `/en/docs/historical-forecast-api`, `/en/docs/previous-runs-api`, `/en/terms`, `/en/licence`) and the CRAN `openmeteo` package's test responses. The tests use hand-built fixtures in the documented shapes.

## Covered

- **Forecast**: `GET https://api.open-meteo.com/v1/forecast`; `forecast_days` (default 7, maximum 16); rows start at 00:00 today in the requested time zone.
- **Archive**: `GET https://archive-api.open-meteo.com/v1/archive`; inclusive `start_date`/`end_date` as `yyyy-mm-dd`; data from 1940; reanalysis lags real time by a few days.
- **Historical forecast**: the forecast parameters on `https://historical-forecast-api.open-meteo.com/v1/forecast`, with `start_date`/`end_date`. Successive model runs are stitched into one continuous series, so rows carry no issue time.
- **Hourly variables**: `temperature_2m`, `relative_humidity_2m`, `wind_speed_10m`, `wind_direction_10m`, `precipitation` (sum over the preceding hour), `cloud_cover` (instant, %), `shortwave_radiation` (mean over the preceding hour, W/m²), `surface_pressure` (instant, hPa, not sea level).
- **Units**: `temperature_unit` `celsius`/`fahrenheit`; `wind_speed_unit` `kmh` (default), `ms`, `mph` or `kn`; `precipitation_unit` `mm`/`inch`. `hourly_units` reports `°C`, `%`, `m/s`, `°`, `mm`, `W/m²` and `hPa`; `W/m2` and `W/m^2` are accepted too.
- **Time**: `timeformat=unixtime` gives epoch seconds (UTC); `timezone` defaults to `GMT`. The client sends both explicitly.
- **Response**: `latitude` and `longitude` (of the grid cell, not the request), `generationtime_ms`, `utc_offset_seconds`, `timezone`, `timezone_abbreviation`, `elevation`, `hourly_units`, and `hourly` as parallel arrays with `null` for missing values.
- **Errors**: HTTP 400 with `{"error": true, "reason": "…"}` for bad parameters; HTTP 429 over the rate limit.
- **Commercial use**: an API key, passed as `&apikey=…`, on the `customer-` hosts (`customer-api.open-meteo.com` and the matching archive and historical-forecast hosts).

## Known gaps

- Past forecasts at a fixed lead time come from the Previous Runs API (`previous-runs-api.open-meteo.com`, variables such as `temperature_2m_previous_day1`) or the Single Runs API; neither is implemented.
- Free-tier limits: under 10 000 calls a day, 5 000 an hour and 600 a minute; large requests count as several calls. The client does not throttle for you.
- Data is licensed CC BY 4.0 (credit Open-Meteo, link the licence, say if the data was changed); the free API is for non-commercial use only.
- There is no recorded-response test yet; error `reason` texts in the fixtures are paraphrased.
