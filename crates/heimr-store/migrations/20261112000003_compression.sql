-- architecture §4: compress chunks older than 7 days. Raw data is retained
-- indefinitely: there are deliberately NO retention policies.

ALTER TABLE reading SET (
    timescaledb.compress,
    timescaledb.compress_segmentby = 'source, device, metric',
    timescaledb.compress_orderby = 'time DESC'
);
SELECT add_compression_policy('reading', compress_after => INTERVAL '7 days');

ALTER TABLE energy_halfhour SET (
    timescaledb.compress,
    timescaledb.compress_segmentby = 'fuel, source',
    timescaledb.compress_orderby = 'time DESC'
);
-- Consumption is revised for a few weeks; compress later.
SELECT add_compression_policy('energy_halfhour', compress_after => INTERVAL '60 days');

ALTER TABLE price_halfhour SET (
    timescaledb.compress,
    timescaledb.compress_segmentby = 'tariff',
    timescaledb.compress_orderby = 'time DESC'
);
SELECT add_compression_policy('price_halfhour', compress_after => INTERVAL '30 days');

ALTER TABLE weather_obs SET (
    timescaledb.compress,
    timescaledb.compress_segmentby = 'location, source',
    timescaledb.compress_orderby = 'time DESC'
);
SELECT add_compression_policy('weather_obs', compress_after => INTERVAL '7 days');

ALTER TABLE weather_forecast SET (
    timescaledb.compress,
    timescaledb.compress_segmentby = 'location, source',
    timescaledb.compress_orderby = 'valid_at DESC, issued_at DESC'
);
SELECT add_compression_policy('weather_forecast', compress_after => INTERVAL '14 days');

ALTER TABLE power_live SET (
    timescaledb.compress,
    timescaledb.compress_segmentby = 'device',
    timescaledb.compress_orderby = 'time DESC'
);
SELECT add_compression_policy('power_live', compress_after => INTERVAL '7 days');

ALTER TABLE event SET (
    timescaledb.compress,
    timescaledb.compress_segmentby = 'kind',
    timescaledb.compress_orderby = 'time DESC, id DESC'
);
SELECT add_compression_policy('event', compress_after => INTERVAL '30 days');
