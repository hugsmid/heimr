//! TimescaleDB schema, migrations and idempotent writes for heimr.
//!
//! The schema (`docs/architecture.md` §4) is defined by the SQL migrations
//! in `migrations/` and applied with [`migrate`] when heimr starts. Writes
//! are idempotent: replaying the same samples (e.g. after a reconnect or
//! spool replay) never creates duplicates.
#![forbid(unsafe_code)]

use jiff::{SignedDuration, Timestamp};
use sqlx::PgPool;
use sqlx::migrate::Migrator;

/// Embedded migrations from `migrations/`.
pub static MIGRATOR: Migrator = sqlx::migrate!();

/// Errors from the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A database error.
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    /// A migration failed to apply.
    #[error("migration: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

/// Apply all pending migrations.
pub async fn migrate(pool: &PgPool) -> Result<(), StoreError> {
    MIGRATOR.run(pool).await?;
    Ok(())
}

/// Continuous aggregates in refresh order (each level reads the one before),
/// with how far to widen a refresh window so it covers whole buckets.
/// Daily buckets are Europe/London days (23–25 h); monthly buckets are at
/// most 31 days plus a DST hour.
const AGGREGATES: [(&str, SignedDuration); 6] = [
    ("reading_5m", SignedDuration::from_mins(5)),
    ("reading_1h", SignedDuration::from_hours(1)),
    ("reading_1d", SignedDuration::from_hours(2 * 24)),
    ("energy_daily", SignedDuration::from_hours(2 * 24)),
    ("energy_monthly", SignedDuration::from_hours(32 * 24)),
    ("price_daily", SignedDuration::from_hours(2 * 24)),
];

/// Re-materialise every continuous aggregate over `[from, to)`, bottom-up
/// (`reading_5m → 1h → 1d`, `energy_daily → monthly`, `price_daily`).
///
/// The background refresh policies only look back a fixed window (1 day for
/// `reading_5m`, 35 days for `energy_daily`, …), so rows written further in the
/// past — backfills, spool replays, late supplier revisions — would otherwise
/// never reach the aggregates. Call this after any such write with the time
/// range of the rows written. The window is widened to whole buckets at every
/// level; only ranges invalidated by writes are actually recomputed, so a
/// generous range is cheap.
///
/// A background policy refreshing the same aggregate at the same moment makes
/// TimescaleDB refuse ours with a lock error (SQLSTATE `55P03`). That refresh
/// is retried with bounded exponential backoff (see [`RefreshRetry`]); any
/// other error, or the same one after the last attempt, is returned.
///
/// Each refresh runs in its own implicit transaction (TimescaleDB refuses to
/// refresh inside a transaction block), so do not call this from within one.
pub async fn refresh_aggregates(
    pool: &PgPool,
    from: Timestamp,
    to: Timestamp,
) -> Result<(), StoreError> {
    refresh_aggregates_with(pool, from, to, RefreshRetry::default()).await
}

/// [`refresh_aggregates`] with an explicit retry policy.
pub async fn refresh_aggregates_with(
    pool: &PgPool,
    from: Timestamp,
    to: Timestamp,
    retry: RefreshRetry,
) -> Result<(), StoreError> {
    for (cagg, widen) in AGGREGATES {
        let start = from.checked_sub(widen).unwrap_or(Timestamp::MIN);
        let end = to.checked_add(widen).unwrap_or(Timestamp::MAX);
        let mut attempt = 1;
        loop {
            let res = sqlx::query!(
                "CALL refresh_continuous_aggregate($1::text::regclass, $2::timestamptz, $3::timestamptz)",
                cagg,
                jiff_sqlx::Timestamp::from(start) as jiff_sqlx::Timestamp,
                jiff_sqlx::Timestamp::from(end) as jiff_sqlx::Timestamp,
            )
            .execute(pool)
            .await;
            match res {
                Ok(_) => break,
                Err(e) if is_lock_not_available(&e) && attempt < retry.max_attempts => {
                    tokio::time::sleep(retry.delay(attempt)).await;
                    attempt += 1;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

/// How [`refresh_aggregates`] retries a refresh that collided with a
/// concurrent one: `max_attempts` tries in total, waiting `initial_delay`,
/// then twice as long each time, never more than `max_delay`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshRetry {
    /// Total attempts per aggregate, including the first (at least 1).
    pub max_attempts: u32,
    /// Wait before the second attempt.
    pub initial_delay: std::time::Duration,
    /// Upper bound for any single wait.
    pub max_delay: std::time::Duration,
}

impl Default for RefreshRetry {
    /// 8 attempts, 250 ms doubling up to 8 s: about 24 s of waiting in total,
    /// longer than a policy refresh of the recent window normally takes.
    fn default() -> Self {
        Self {
            max_attempts: 8,
            initial_delay: std::time::Duration::from_millis(250),
            max_delay: std::time::Duration::from_secs(8),
        }
    }
}

impl RefreshRetry {
    /// The wait after failed attempt `attempt` (1-based).
    pub fn delay(&self, attempt: u32) -> std::time::Duration {
        let factor = 1u32
            .checked_shl(attempt.saturating_sub(1))
            .unwrap_or(u32::MAX);
        self.initial_delay
            .checked_mul(factor)
            .unwrap_or(self.max_delay)
            .min(self.max_delay)
    }
}

/// SQLSTATE `55P03` (`lock_not_available`): what TimescaleDB raises when
/// another session is refreshing the same continuous aggregate.
fn is_lock_not_available(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(|d| d.code())
        .is_some_and(|c| c == "55P03")
}

/// Data quality flag stored with every reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    /// Measured and plausible.
    Good,
    /// Derived or interpolated.
    Estimated,
    /// Measured but outside expected bounds.
    Suspect,
    /// Known bad; kept for audit, excluded from aggregates.
    Bad,
}

impl Quality {
    fn as_str(self) -> &'static str {
        match self {
            Self::Good => "good",
            Self::Estimated => "estimated",
            Self::Suspect => "suspect",
            Self::Bad => "bad",
        }
    }
}

/// One sample in the narrow `reading` table.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// Sample time (UTC).
    pub time: Timestamp,
    /// Adapter instance, e.g. `demo`.
    pub source: String,
    /// Stable device id within the source.
    pub device: String,
    /// Metric name, e.g. `temperature`.
    pub metric: String,
    /// Value in `unit`.
    pub value: f64,
    /// SI unit or °C, %, µg/m³ …
    pub unit: String,
    /// Quality flag.
    pub quality: Quality,
}

/// Insert readings in one round trip. Duplicates (same source, device, metric
/// and time) are ignored; returns the number of rows actually inserted.
pub async fn insert_readings(pool: &PgPool, readings: &[Reading]) -> Result<u64, StoreError> {
    if readings.is_empty() {
        return Ok(0);
    }
    let n = readings.len();
    let mut time = Vec::with_capacity(n);
    let mut source = Vec::with_capacity(n);
    let mut device = Vec::with_capacity(n);
    let mut metric = Vec::with_capacity(n);
    let mut value = Vec::with_capacity(n);
    let mut unit = Vec::with_capacity(n);
    let mut quality = Vec::with_capacity(n);
    for r in readings {
        time.push(jiff_sqlx::Timestamp::from(r.time));
        source.push(r.source.as_str());
        device.push(r.device.as_str());
        metric.push(r.metric.as_str());
        value.push(r.value);
        unit.push(r.unit.as_str());
        quality.push(r.quality.as_str());
    }
    let res = sqlx::query!(
        r#"
        INSERT INTO reading (time, source, device, metric, value_f64, unit, quality)
        SELECT * FROM UNNEST($1::timestamptz[], $2::text[], $3::text[], $4::text[],
                             $5::float8[], $6::text[], $7::text[])
        ON CONFLICT ON CONSTRAINT reading_uniq DO NOTHING
        "#,
        &time as &[jiff_sqlx::Timestamp],
        &source as &[&str],
        &device as &[&str],
        &metric as &[&str],
        &value as &[f64],
        &unit as &[&str],
        &quality as &[&str],
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Append a decision or action to the `event` log (architecture §8: every
/// decision is logged with its inputs and reason). `source` is the component
/// that emitted it (an adapter instance or a core part such as `actuator`);
/// `actor` is who asked for it (`planner`, `user`, …). Returns the new row id.
pub async fn insert_event(
    pool: &PgPool,
    time: Timestamp,
    source: &str,
    kind: &str,
    actor: &str,
    payload: &serde_json::Value,
) -> Result<i64, StoreError> {
    let id = sqlx::query_scalar!(
        r#"INSERT INTO event (time, source, kind, actor, payload) VALUES ($1, $2, $3, $4, $5) RETURNING id"#,
        jiff_sqlx::Timestamp::from(time) as jiff_sqlx::Timestamp,
        source,
        kind,
        actor,
        payload,
    )
    .fetch_one(pool)
    .await?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::RefreshRetry;
    use std::time::Duration;

    #[test]
    fn retry_delay_doubles_and_is_capped() {
        let r = RefreshRetry::default();
        let delays: Vec<_> = (1..=8).map(|a| r.delay(a)).collect();
        assert_eq!(
            delays,
            [250, 500, 1000, 2000, 4000, 8000, 8000, 8000].map(Duration::from_millis)
        );
        // No overflow for absurd attempt numbers.
        assert_eq!(r.delay(u32::MAX), Duration::from_secs(8));
        assert_eq!(r.delay(0), Duration::from_millis(250));
    }
}
