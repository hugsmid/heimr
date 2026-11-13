-- architecture §4: 5-min, hourly, daily per metric; daily energy/cost per fuel;
-- monthly summaries. Aggregates store sum + count (not avg) so that
-- hierarchical roll-ups stay exact. Daily and monthly buckets follow
-- Europe/London calendar days (DST-aware); sub-daily buckets are UTC.

CREATE MATERIALIZED VIEW reading_5m
WITH (timescaledb.continuous, timescaledb.materialized_only = false) AS
SELECT time_bucket(INTERVAL '5 minutes', time) AS bucket,
       source, device, metric, unit,
       count(*)        AS n,
       sum(value_f64)  AS sum,
       min(value_f64)  AS min,
       max(value_f64)  AS max,
       last(value_f64, time) AS last
FROM reading
WHERE quality IN ('good', 'estimated')
GROUP BY bucket, source, device, metric, unit
WITH NO DATA;

CREATE MATERIALIZED VIEW reading_1h
WITH (timescaledb.continuous, timescaledb.materialized_only = false) AS
SELECT time_bucket(INTERVAL '1 hour', bucket) AS bucket,
       source, device, metric, unit,
       sum(n)   AS n,
       sum(sum) AS sum,
       min(min) AS min,
       max(max) AS max,
       last(last, bucket) AS last
FROM reading_5m
GROUP BY 1, source, device, metric, unit
WITH NO DATA;

CREATE MATERIALIZED VIEW reading_1d
WITH (timescaledb.continuous, timescaledb.materialized_only = false) AS
SELECT time_bucket(INTERVAL '1 day', bucket, 'Europe/London') AS bucket,
       source, device, metric, unit,
       sum(n)   AS n,
       sum(sum) AS sum,
       min(min) AS min,
       max(max) AS max,
       last(last, bucket) AS last
FROM reading_1h
GROUP BY 1, source, device, metric, unit
WITH NO DATA;

CREATE MATERIALIZED VIEW energy_daily
WITH (timescaledb.continuous, timescaledb.materialized_only = false) AS
SELECT time_bucket(INTERVAL '1 day', time, 'Europe/London') AS day,
       fuel, source,
       count(*)     AS slots,
       sum(kwh)     AS kwh,
       sum(cost_p)  AS cost_p,
       count(cost_p) AS priced_slots
FROM energy_halfhour
GROUP BY 1, fuel, source
WITH NO DATA;

CREATE MATERIALIZED VIEW energy_monthly
WITH (timescaledb.continuous, timescaledb.materialized_only = false) AS
SELECT time_bucket(INTERVAL '1 month', day, 'Europe/London') AS month,
       fuel, source,
       sum(slots)        AS slots,
       sum(kwh)          AS kwh,
       sum(cost_p)       AS cost_p,
       sum(priced_slots) AS priced_slots
FROM energy_daily
GROUP BY 1, fuel, source
WITH NO DATA;

CREATE MATERIALIZED VIEW price_daily
WITH (timescaledb.continuous, timescaledb.materialized_only = false) AS
SELECT time_bucket(INTERVAL '1 day', time, 'Europe/London') AS day,
       tariff,
       count(*)    AS slots,
       avg(rate_p) AS mean_rate_p,
       min(rate_p) AS min_rate_p,
       max(rate_p) AS max_rate_p
FROM price_halfhour
GROUP BY 1, tariff
WITH NO DATA;

-- Refresh policies. Octopus consumption lands next day (sometimes later), so
-- energy aggregates look back further. Policies only ever look back
-- start_offset: writes older than that (backfills, spool replays) must be
-- followed by heimr_store::refresh_aggregates for their time range.
SELECT add_continuous_aggregate_policy('reading_5m',
    start_offset => INTERVAL '1 day', end_offset => INTERVAL '5 minutes',
    schedule_interval => INTERVAL '5 minutes');
SELECT add_continuous_aggregate_policy('reading_1h',
    start_offset => INTERVAL '3 days', end_offset => INTERVAL '1 hour',
    schedule_interval => INTERVAL '30 minutes');
SELECT add_continuous_aggregate_policy('reading_1d',
    start_offset => INTERVAL '7 days', end_offset => INTERVAL '1 day',
    schedule_interval => INTERVAL '6 hours');
SELECT add_continuous_aggregate_policy('energy_daily',
    start_offset => INTERVAL '35 days', end_offset => INTERVAL '1 day',
    schedule_interval => INTERVAL '6 hours');
SELECT add_continuous_aggregate_policy('energy_monthly',
    start_offset => INTERVAL '3 months', end_offset => INTERVAL '1 month',
    schedule_interval => INTERVAL '1 day');
SELECT add_continuous_aggregate_policy('price_daily',
    start_offset => INTERVAL '7 days', end_offset => INTERVAL '1 day',
    schedule_interval => INTERVAL '6 hours');
