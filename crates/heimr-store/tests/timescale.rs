//! Schema tests against a real TimescaleDB container (architecture §10 "Data integrity").
//!
//! Needs Docker. Without it the tests skip, unless `REQUIRE_DOCKER_TESTS=1`
//! (set in CI), in which case they fail. Image: `TIMESCALE_IMAGE` (default
//! pinned below).

#![allow(clippy::unwrap_used)]

use heimr_store::{
    MIGRATOR, Quality, Reading, RefreshRetry, StoreError, insert_event, insert_readings, migrate,
    refresh_aggregates, refresh_aggregates_with,
};
use jiff::{Timestamp, ToSpan};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

const DEFAULT_IMAGE: &str = "timescale/timescaledb:2.27.1-pg17";

struct Db {
    pool: PgPool,
    _container: ContainerAsync<GenericImage>,
}

async fn db() -> Option<Db> {
    let required = std::env::var_os("REQUIRE_DOCKER_TESTS").is_some();
    let image = std::env::var("TIMESCALE_IMAGE").unwrap_or_else(|_| DEFAULT_IMAGE.to_owned());
    let (name, tag) = image
        .rsplit_once(':')
        .expect("TIMESCALE_IMAGE must be name:tag");

    let started = GenericImage::new(name, tag)
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_PASSWORD", "test")
        .with_env_var("POSTGRES_DB", "home")
        // No background workers: refresh and compression policies never run,
        // so tests drive refreshes deterministically (a policy refresh racing
        // a manual one fails with "concurrent refresh").
        .with_cmd(["postgres", "-c", "timescaledb.max_background_workers=0"])
        .start()
        .await;
    let container = match started {
        Ok(c) => c,
        Err(e) if !required => {
            eprintln!("skipping TimescaleDB test (no Docker?): {e}");
            return None;
        }
        Err(e) => panic!("REQUIRE_DOCKER_TESTS is set but the container failed: {e}"),
    };
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:test@127.0.0.1:{port}/home");

    // The image restarts Postgres once after init; retry until it settles.
    let mut last_err = None;
    for _ in 0..60 {
        match PgPoolOptions::new().max_connections(4).connect(&url).await {
            Ok(pool) if sqlx::query("SELECT 1").execute(&pool).await.is_ok() => {
                return Some(Db {
                    pool,
                    _container: container,
                });
            }
            Ok(_) => {}
            Err(e) => last_err = Some(e),
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    panic!("TimescaleDB never accepted connections: {last_err:?}");
}

async fn migrated() -> Option<Db> {
    let db = db().await?;
    // Background workers are disabled at server start (see `db`), before any
    // policy exists, so no policy run can start while a test refreshes.
    let workers: String = sqlx::query_scalar("SHOW timescaledb.max_background_workers")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(workers, "0", "background workers must be off in tests");
    migrate(&db.pool).await.expect("migrations apply");
    Some(db)
}

fn ts(s: &str) -> Timestamp {
    s.parse().expect("valid timestamp")
}

fn reading(time: Timestamp, metric: &str, value: f64) -> Reading {
    Reading {
        time,
        source: "test".into(),
        device: "dev-1".into(),
        metric: metric.into(),
        value,
        unit: "°C".into(),
        quality: Quality::Good,
    }
}

async fn refresh(pool: &PgPool, cagg: &str) {
    // NULL window = refresh everything. Must run outside a transaction.
    sqlx::query("CALL refresh_continuous_aggregate($1::regclass, NULL, NULL)")
        .bind(cagg)
        .execute(pool)
        .await
        .expect("refresh");
}

#[tokio::test]
async fn migrations_create_the_schema_and_are_idempotent() {
    let Some(db) = migrated().await else { return };
    // Re-running is a no-op.
    MIGRATOR.run(&db.pool).await.expect("second run");

    let hypertables: Vec<String> = sqlx::query_scalar(
        "SELECT hypertable_name::text FROM timescaledb_information.hypertables ORDER BY 1",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        hypertables,
        [
            "energy_halfhour",
            "event",
            "power_live",
            "price_halfhour",
            "reading",
            "weather_forecast",
            "weather_obs"
        ]
    );

    let caggs: Vec<String> = sqlx::query_scalar(
        "SELECT view_name::text FROM timescaledb_information.continuous_aggregates ORDER BY 1",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        caggs,
        [
            "energy_daily",
            "energy_monthly",
            "price_daily",
            "reading_1d",
            "reading_1h",
            "reading_5m"
        ]
    );

    // Every hypertable compresses; nothing is ever dropped.
    let uncompressed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM timescaledb_information.hypertables WHERE NOT compression_enabled",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(uncompressed, 0);

    let jobs: Vec<(String, i64)> = sqlx::query_as(
        "SELECT proc_name::text, count(*) FROM timescaledb_information.jobs
         WHERE hypertable_name IS NOT NULL
         GROUP BY 1 ORDER BY 1",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        jobs,
        [
            ("policy_compression".to_owned(), 7),
            ("policy_refresh_continuous_aggregate".to_owned(), 6)
        ],
        "no retention policies may exist (raw data is kept forever)"
    );
}

#[tokio::test]
async fn reading_inserts_are_idempotent() {
    let Some(db) = migrated().await else { return };
    let t0 = ts("2026-11-12T10:00:00Z");
    let batch = vec![
        reading(t0, "temperature", 20.5),
        reading(t0, "humidity", 55.0),
        reading(t0 + 1.minute(), "temperature", 20.6),
    ];
    assert_eq!(insert_readings(&db.pool, &batch).await.unwrap(), 3);
    // Replay (reconnect / spool replay) inserts nothing.
    assert_eq!(insert_readings(&db.pool, &batch).await.unwrap(), 0);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM reading")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(n, 3);
    assert_eq!(insert_readings(&db.pool, &[]).await.unwrap(), 0);
}

#[tokio::test]
async fn half_hour_tables_reject_misaligned_slots() {
    let Some(db) = migrated().await else { return };
    let ok = sqlx::query(
        "INSERT INTO energy_halfhour (time, fuel, kwh, source) VALUES ('2026-11-12T10:30:00Z', 'gas', 1.2, 't')",
    )
    .execute(&db.pool)
    .await;
    assert!(ok.is_ok());
    for bad in ["2026-11-12T10:15:00Z", "2026-11-12T10:30:01Z"] {
        let res = sqlx::query(
            "INSERT INTO price_halfhour (time, tariff, rate_p) VALUES ($1::timestamptz, 'T', 10)",
        )
        .bind(bad)
        .execute(&db.pool)
        .await;
        assert!(res.is_err(), "{bad} should be rejected");
    }
}

/// Daily energy is bucketed on Europe/London days: a clocks-back day has 50
/// half-hour slots, a clocks-forward day 46, a normal day 48.
#[tokio::test]
async fn energy_daily_follows_uk_days_across_dst() {
    let Some(db) = migrated().await else { return };
    // UTC range covering UK days 2026-03-28 .. 2026-03-30 and 2026-10-24 .. 2026-10-26.
    for (from, to) in [
        ("2026-03-28T00:00:00Z", "2026-03-30T23:00:00Z"),
        ("2026-10-23T23:00:00Z", "2026-10-27T00:00:00Z"),
    ] {
        sqlx::query(
            "INSERT INTO energy_halfhour (time, fuel, kwh, rate_p, cost_p, source)
             SELECT t, 'electricity', 0.5, 20, 10, 'octopus'
             FROM generate_series($1::timestamptz, $2::timestamptz - interval '30 minutes', interval '30 minutes') t",
        )
        .bind(from)
        .bind(to)
        .execute(&db.pool)
        .await
        .unwrap();
    }
    refresh(&db.pool, "energy_daily").await;
    refresh(&db.pool, "energy_monthly").await;

    let days: Vec<(String, i64, f64)> = sqlx::query_as(
        "SELECT to_char(day AT TIME ZONE 'Europe/London', 'YYYY-MM-DD'), slots, kwh
         FROM energy_daily ORDER BY day",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    let expect = [
        ("2026-03-28", 48),
        ("2026-03-29", 46), // clocks forward
        ("2026-03-30", 48),
        ("2026-10-24", 48),
        ("2026-10-25", 50), // clocks back
        ("2026-10-26", 48),
    ];
    assert_eq!(days.len(), expect.len(), "{days:?}");
    for ((day, slots, kwh), (eday, eslots)) in days.iter().zip(expect) {
        assert_eq!((day.as_str(), *slots), (eday, eslots));
        assert!((kwh - *slots as f64 * 0.5).abs() < 1e-9);
    }

    // Monthly roll-up conserves totals.
    let (slots, cost): (i64, f64) =
        sqlx::query_as("SELECT sum(slots)::bigint, sum(cost_p) FROM energy_monthly")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(slots, 288);
    assert!((cost - 2880.0).abs() < 1e-9);
}

/// 5m → 1h → 1d roll-ups keep sum/count, so means are exact at every level.
#[tokio::test]
async fn hierarchical_reading_aggregates_are_exact() {
    let Some(db) = migrated().await else { return };
    let start = ts("2026-07-01T00:00:00Z");
    let batch: Vec<Reading> = (0..(24 * 60))
        .map(|i| {
            reading(
                start + (i as i64).minutes(),
                "temperature",
                15.0 + (i % 97) as f64 / 10.0,
            )
        })
        .collect();
    insert_readings(&db.pool, &batch).await.unwrap();
    for cagg in ["reading_5m", "reading_1h", "reading_1d"] {
        refresh(&db.pool, cagg).await;
    }
    let raw: f64 = sqlx::query_scalar(
        "SELECT avg(value_f64) FROM reading
         WHERE time >= '2026-06-30T23:00:00Z' AND time < '2026-07-01T23:00:00Z'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    // UK day 2026-07-01 starts at 23:00Z the day before (BST).
    let (n, mean, min, max): (f64, f64, f64, f64) = sqlx::query_as(
        "SELECT n::float8, sum / n, min, max FROM reading_1d
         WHERE bucket = '2026-07-01 00:00 Europe/London'::timestamptz",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(n, 23.0 * 60.0); // data starts at 00:00Z = 01:00 BST
    assert!((mean - raw).abs() < 1e-9, "{mean} vs {raw}");
    assert_eq!((min, max), (15.0, 24.6));
}

/// Compressed chunks still read back, and replaying old samples into them is
/// still idempotent.
#[tokio::test]
async fn compressed_chunks_stay_readable_and_idempotent() {
    let Some(db) = migrated().await else { return };
    let old = Timestamp::now() - (60 * 24).hours();
    let old = old.round(jiff::Unit::Second).unwrap();
    let batch: Vec<Reading> = (0..100)
        .map(|i| {
            reading(
                old + (i as i64).minutes(),
                "temperature",
                18.0 + i as f64 / 100.0,
            )
        })
        .collect();
    insert_readings(&db.pool, &batch).await.unwrap();

    let compressed: i64 = sqlx::query_scalar(
        "SELECT count(compress_chunk(c)) FROM show_chunks('reading', older_than => interval '7 days') c",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(compressed >= 1);

    let (n, sum): (i64, f64) = sqlx::query_as("SELECT count(*), sum(value_f64) FROM reading")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(n, 100);
    assert!((sum - batch.iter().map(|r| r.value).sum::<f64>()).abs() < 1e-9);

    assert_eq!(insert_readings(&db.pool, &batch).await.unwrap(), 0);
}

/// The slot constraints must not depend on the session TimeZone: in a zone
/// with a :45 offset, `date_part('minute', time)` of a valid UTC slot is 15.
#[tokio::test]
async fn half_hour_constraints_ignore_session_timezone() {
    let Some(db) = migrated().await else { return };
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("SET LOCAL TimeZone = 'Asia/Kathmandu'")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO price_halfhour (time, tariff, rate_p) VALUES ('2026-11-12T10:30:00Z', 'T', 10)",
    )
    .execute(&mut *tx)
    .await
    .expect("a UTC half-hour is valid in any session time zone");
    tx.commit().await.unwrap();
    for bad in ["2026-11-12T10:30:00+05:45", "2026-11-12T10:30:00.5Z"] {
        let res = sqlx::query(
            "INSERT INTO energy_halfhour (time, fuel, kwh, source) VALUES ($1::timestamptz, 'gas', 1, 't')",
        )
        .bind(bad)
        .execute(&db.pool)
        .await;
        assert!(res.is_err(), "{bad} should be rejected");
    }
}

/// Policies only look back a fixed window, so back-filled rows (60 days old
/// readings, months-old energy) are invisible to the aggregates until
/// `refresh_aggregates` runs; afterwards every level reflects them.
#[tokio::test]
async fn refresh_aggregates_picks_up_backfilled_rows_at_every_level() {
    let Some(db) = migrated().await else { return };
    let pool = &db.pool;
    let now = Timestamp::now().round(jiff::Unit::Hour).unwrap();

    // Recent data, materialised everywhere: moves each aggregate's watermark
    // past the back-fill range, so the real-time union can't mask the gap.
    insert_readings(pool, &[reading(now - 2.hours(), "temperature", 1.0)])
        .await
        .unwrap();
    insert_slots(pool, now - 2.hours(), now - 2.hours()).await;
    for cagg in AGGREGATES {
        refresh(pool, cagg).await;
    }
    assert_eq!(aggregate_counts(pool).await, [1.0; 6]);

    // Back-fill, older than every policy's start_offset: two days of
    // readings ~60 days ago; ~150 days of energy and prices from ~200 days ago.
    let old = now - (60 * 24).hours();
    let readings: Vec<Reading> = (0..(2 * 24 * 60))
        .map(|i| reading(old + (i as i64).minutes(), "temperature", 10.0))
        .collect();
    insert_readings(pool, &readings).await.unwrap();
    let e_from = now - (200 * 24).hours();
    let e_to = now - (50 * 24).hours();
    insert_slots(pool, e_from, e_to).await;

    // Before: the back-fill is invisible at every level.
    assert_eq!(aggregate_counts(pool).await, [1.0; 6]);

    refresh_aggregates(pool, old, old + 48.hours())
        .await
        .unwrap();
    refresh_aggregates(pool, e_from, e_to).await.unwrap();
    // Repeating (or overlapping) a refresh is harmless.
    refresh_aggregates(pool, e_from, e_to).await.unwrap();

    let n_read = scalar(pool, "SELECT count(*)::float8 FROM reading").await;
    let n_slots = scalar(pool, "SELECT count(*)::float8 FROM energy_halfhour").await;
    assert_eq!(n_read, 1.0 + 2.0 * 24.0 * 60.0);
    assert!(n_slots > 150.0 * 48.0);
    assert_eq!(
        aggregate_counts(pool).await,
        [n_read, n_read, n_read, n_slots, n_slots, n_slots]
    );

    // Totals are conserved through the roll-ups.
    let kwh = scalar(pool, "SELECT sum(kwh) FROM energy_monthly").await;
    let raw_kwh = scalar(pool, "SELECT sum(kwh) FROM energy_halfhour").await;
    assert!((kwh - raw_kwh).abs() < 1e-6, "{kwh} vs {raw_kwh}");
    let sum = scalar(pool, "SELECT sum(sum) FROM reading_1d").await;
    assert!(
        (sum - (1.0 + 2.0 * 24.0 * 60.0 * 10.0)).abs() < 1e-6,
        "{sum}"
    );
}

const AGGREGATES: [&str; 6] = [
    "reading_5m",
    "reading_1h",
    "reading_1d",
    "energy_daily",
    "energy_monthly",
    "price_daily",
];

async fn scalar(pool: &PgPool, sql: &str) -> f64 {
    // Test-only SQL built from literals.
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Rows represented at each aggregate level, in [`AGGREGATES`] order.
async fn aggregate_counts(pool: &PgPool) -> [f64; 6] {
    let mut out = [0.0; 6];
    for (o, (cagg, col)) in out.iter_mut().zip([
        ("reading_5m", "n"),
        ("reading_1h", "n"),
        ("reading_1d", "n"),
        ("energy_daily", "slots"),
        ("energy_monthly", "slots"),
        ("price_daily", "slots"),
    ]) {
        *o = scalar(
            pool,
            &format!("SELECT coalesce(sum({col}), 0)::float8 FROM {cagg}"),
        )
        .await;
    }
    out
}

/// One electricity and one price row per half-hour slot in `[from, to]`.
async fn insert_slots(pool: &PgPool, from: Timestamp, to: Timestamp) {
    for sql in [
        "INSERT INTO energy_halfhour (time, fuel, kwh, cost_p, source)
         SELECT t, 'electricity', 0.5, 10, 'octopus'
         FROM generate_series(date_bin('30 minutes', $1::timestamptz, 'epoch'),
                              $2::timestamptz, interval '30 minutes') t",
        "INSERT INTO price_halfhour (time, tariff, rate_p)
         SELECT t, 'T', 20
         FROM generate_series(date_bin('30 minutes', $1::timestamptz, 'epoch'),
                              $2::timestamptz, interval '30 minutes') t",
    ] {
        sqlx::query(sql)
            .bind(from.to_string())
            .bind(to.to_string())
            .execute(pool)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn events_are_appended_with_payload() {
    let Some(db) = migrated().await else { return };
    let payload =
        serde_json::json!({"zone": "zone-a", "from": 19.0, "to": 20.5, "reason": "pre-heat"});
    let t = ts("2026-11-12T06:00:00Z");
    let a = insert_event(
        &db.pool,
        t,
        "actuator",
        "setpoint_change",
        "planner",
        &payload,
    )
    .await
    .unwrap();
    let b = insert_event(
        &db.pool,
        t,
        "actuator",
        "setpoint_change",
        "planner",
        &payload,
    )
    .await
    .unwrap();
    assert_ne!(a, b, "events are append-only, not deduplicated");
    let (source, actor, reason): (String, String, String) =
        sqlx::query_as("SELECT source, actor, payload->>'reason' FROM event WHERE id = $1")
            .bind(a)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        (source.as_str(), actor.as_str(), reason.as_str()),
        ("actuator", "planner", "pre-heat")
    );
}

#[tokio::test]
async fn events_require_a_source() {
    let Some(db) = migrated().await else { return };
    let res = sqlx::query(
        "INSERT INTO event (time, kind, actor) VALUES ('2026-11-12T06:00:00Z', 'k', 'user')",
    )
    .execute(&db.pool)
    .await;
    let err = res.expect_err("source is NOT NULL");
    assert_eq!(
        err.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("23502"),
        "{err}"
    );
}

/// A refresh that collides with a concurrent one (in production: the
/// background policy) fails with SQLSTATE 55P03; `refresh_aggregates` retries
/// it until the other refresh is gone.
///
/// The collision is provoked deterministically: TimescaleDB records every
/// in-progress refresh in `continuous_aggs_jobs_refresh_ranges` together with
/// the backend pid running it, and refuses an overlapping refresh while that
/// backend is alive. The test registers such a range for a connection it
/// holds open, then removes it. (Catalog internals of the pinned image; if an
/// upgrade changes them, this test fails loudly rather than passing vacuously.)
#[tokio::test]
async fn refresh_aggregates_retries_a_concurrent_refresh() {
    let Some(db) = migrated().await else { return };
    let pool = &db.pool;
    let t = ts("2026-01-10T12:00:00Z");
    insert_readings(pool, &[reading(t, "temperature", 21.0)])
        .await
        .unwrap();

    // A live backend standing in for the policy worker.
    let mut holder = pool.acquire().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO _timescaledb_catalog.continuous_aggs_jobs_refresh_ranges
         SELECT mat_hypertable_id, -9223372036854775808, 9223372036854775807, $1, 0, now()
         FROM _timescaledb_catalog.continuous_agg WHERE user_view_name = 'reading_5m'",
    )
    .bind(pid)
    .execute(pool)
    .await
    .unwrap();

    // Without retries the collision surfaces as 55P03.
    let once = RefreshRetry {
        max_attempts: 1,
        ..RefreshRetry::default()
    };
    match refresh_aggregates_with(pool, t, t + 1.hour(), once).await {
        Err(StoreError::Db(e)) => assert_eq!(
            e.as_database_error().and_then(|d| d.code()).as_deref(),
            Some("55P03"),
            "{e}"
        ),
        other => panic!("expected a concurrent-refresh error, got {other:?}"),
    }

    // The "policy" finishes after a while; the retrying refresh then succeeds.
    let release = {
        let pool = pool.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(600)).await;
            sqlx::query("DELETE FROM _timescaledb_catalog.continuous_aggs_jobs_refresh_ranges")
                .execute(&pool)
                .await
                .unwrap();
        })
    };
    let patient = RefreshRetry {
        max_attempts: 20,
        initial_delay: Duration::from_millis(50),
        max_delay: Duration::from_millis(200),
    };
    refresh_aggregates_with(pool, t, t + 1.hour(), patient)
        .await
        .expect("retried until the concurrent refresh was gone");
    release.await.unwrap();
    drop(holder);

    assert_eq!(aggregate_counts(pool).await, [1.0, 1.0, 1.0, 0.0, 0.0, 0.0]);
}
