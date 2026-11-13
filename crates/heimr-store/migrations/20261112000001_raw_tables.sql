-- architecture §4: raw tables are hypertables and are never deleted.
-- All timestamps are UTC (timestamptz). Units: SI + °C, p/kWh.

CREATE EXTENSION IF NOT EXISTS timescaledb;

-- Narrow/long format for heterogeneous sensors.
CREATE TABLE reading (
    time      timestamptz      NOT NULL,
    source    text             NOT NULL,  -- adapter instance, e.g. 'demo', 'octopus'
    device    text             NOT NULL,  -- stable device id within the source
    metric    text             NOT NULL,  -- e.g. 'temperature', 'humidity', 'pm25'
    value_f64 double precision NOT NULL,
    unit      text             NOT NULL,  -- e.g. '°C', '%', 'µg/m³'
    quality   text             NOT NULL DEFAULT 'good'
              CHECK (quality IN ('good', 'estimated', 'suspect', 'bad')),
    -- Idempotent upserts: a replayed sample collapses onto the original.
    CONSTRAINT reading_uniq UNIQUE (source, device, metric, time)
);
SELECT create_hypertable('reading', by_range('time', INTERVAL '7 days'));

-- Half-hourly energy per fuel. rate_p/cost_p are filled once prices are known.
CREATE TABLE energy_halfhour (
    time    timestamptz      NOT NULL,  -- slot start, aligned to a UTC half-hour
    fuel    text             NOT NULL CHECK (fuel IN ('electricity', 'gas', 'electricity_export')),
    kwh     double precision NOT NULL,
    rate_p  double precision,           -- p/kWh inc. VAT
    cost_p  double precision,           -- kwh × rate_p
    source  text             NOT NULL,  -- adapter instance that supplied it, e.g. 'octopus'
    -- Epoch arithmetic (numeric, exact): independent of the session TimeZone
    -- (date_part on timestamptz is not) and rejects fractional seconds.
    CONSTRAINT energy_halfhour_slot CHECK (
        extract(epoch FROM time) % 1800 = 0
    ),
    CONSTRAINT energy_halfhour_uniq UNIQUE (fuel, source, time)
);
SELECT create_hypertable('energy_halfhour', by_range('time', INTERVAL '30 days'));

-- Unit rates per tariff and half-hour slot (Agile; flat tariffs expand to slots).
CREATE TABLE price_halfhour (
    time          timestamptz      NOT NULL,
    tariff        text             NOT NULL,  -- full tariff code, e.g. E-1R-AGILE-24-10-01-A
    rate_p        double precision NOT NULL,  -- inc. VAT; may be negative
    rate_p_ex_vat double precision,
    CONSTRAINT price_halfhour_slot CHECK (
        extract(epoch FROM time) % 1800 = 0
    ),
    CONSTRAINT price_halfhour_uniq UNIQUE (tariff, time)
);
SELECT create_hypertable('price_halfhour', by_range('time', INTERVAL '30 days'));

-- Standing charges: low volume, plain table.
CREATE TABLE standing_charge (
    tariff        text             NOT NULL,
    valid_from    timestamptz      NOT NULL,
    valid_to      timestamptz,                -- NULL = open-ended
    p_per_day     double precision NOT NULL,  -- inc. VAT
    p_per_day_ex_vat double precision,
    PRIMARY KEY (tariff, valid_from),
    CHECK (valid_to IS NULL OR valid_to > valid_from)
);

-- Observed weather.
CREATE TABLE weather_obs (
    time            timestamptz NOT NULL,
    location        text        NOT NULL,
    source          text        NOT NULL,  -- e.g. 'open-meteo'
    temperature_c   double precision,
    humidity_pct    double precision,
    wind_speed_ms   double precision,
    wind_dir_deg    double precision,
    precip_mm       double precision,
    cloud_cover_pct double precision,
    shortwave_wm2   double precision,
    pressure_hpa    double precision,
    CONSTRAINT weather_obs_uniq UNIQUE (location, source, time)
);
SELECT create_hypertable('weather_obs', by_range('time', INTERVAL '30 days'));

-- Forecasts are kept (every issue) to evaluate accuracy later.
CREATE TABLE weather_forecast (
    valid_at        timestamptz NOT NULL,
    issued_at       timestamptz NOT NULL,
    location        text        NOT NULL,
    source          text        NOT NULL,
    temperature_c   double precision,
    humidity_pct    double precision,
    wind_speed_ms   double precision,
    wind_dir_deg    double precision,
    precip_mm       double precision,
    cloud_cover_pct double precision,
    shortwave_wm2   double precision,
    pressure_hpa    double precision,
    CHECK (valid_at >= issued_at - INTERVAL '1 day'),
    CONSTRAINT weather_forecast_uniq UNIQUE (location, source, issued_at, valid_at)
);
SELECT create_hypertable('weather_forecast', by_range('valid_at', INTERVAL '30 days'));

-- Near-real-time electricity (smart-meter telemetry, ~10 s).
CREATE TABLE power_live (
    time       timestamptz      NOT NULL,
    device     text             NOT NULL,
    watts      double precision,
    import_kwh double precision,          -- cumulative register
    CONSTRAINT power_live_uniq UNIQUE (device, time)
);
SELECT create_hypertable('power_live', by_range('time', INTERVAL '7 days'));

-- Decisions and actions with inputs + reasons (architecture §8 failsafes).
CREATE TABLE event (
    time    timestamptz NOT NULL,
    id      bigint      GENERATED ALWAYS AS IDENTITY,
    source  text        NOT NULL,  -- component that emitted it: an adapter instance or a core part, e.g. 'actuator'
    kind    text        NOT NULL,  -- e.g. 'setpoint_change', 'planner_decision'
    actor   text        NOT NULL,  -- e.g. 'planner', 'assistant', 'user'
    payload jsonb       NOT NULL DEFAULT '{}'::jsonb,
    PRIMARY KEY (time, id)
);
SELECT create_hypertable('event', by_range('time', INTERVAL '30 days'));
CREATE INDEX event_kind_time ON event (kind, time DESC);
CREATE INDEX event_source_time ON event (source, time DESC);
