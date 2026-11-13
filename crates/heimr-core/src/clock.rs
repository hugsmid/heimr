//! Clock abstraction, so adapters, the actuator and the supervisor can run on
//! fake time in tests.
//!
//! A clock has two time sources: **wall time** ([`Clock::now`]), for
//! timestamps on records, and **monotonic time** ([`Clock::monotonic`]),
//! which never goes backwards and ignores wall-clock steps, for measuring
//! intervals (rate limits, restart backoff).

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use tokio::sync::watch;

/// A boxed, sendable future (used where trait objects need async methods).
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Source of the current time and of timers.
///
/// Always use the context clock rather than [`Timestamp::now`] or
/// `tokio::time` directly, so tests can drive time.
pub trait Clock: Send + Sync + fmt::Debug + 'static {
    /// The current wall time (UTC). Use it for timestamps, never to measure
    /// intervals: it can step backwards or forwards.
    fn now(&self) -> Timestamp;

    /// Monotonic time since an arbitrary origin fixed when the clock was
    /// created. Never decreases and is unaffected by wall-clock steps.
    fn monotonic(&self) -> SignedDuration;

    /// Resolves once [`Clock::now`] is at or after `deadline`.
    fn sleep_until(&self, deadline: Timestamp) -> BoxFuture<()>;

    /// Resolves once [`Clock::monotonic`] has advanced by `duration`.
    fn sleep(&self, duration: SignedDuration) -> BoxFuture<()>;
}

/// A shared clock handle.
pub type SharedClock = Arc<dyn Clock>;

fn to_std(d: SignedDuration) -> Duration {
    Duration::try_from(d).unwrap_or(Duration::ZERO)
}

/// Wall-clock time ([`Timestamp::now`]), the process's monotonic clock and
/// tokio timers.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

fn process_origin() -> std::time::Instant {
    static ORIGIN: OnceLock<std::time::Instant> = OnceLock::new();
    *ORIGIN.get_or_init(std::time::Instant::now)
}

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }

    fn monotonic(&self) -> SignedDuration {
        SignedDuration::try_from(process_origin().elapsed()).unwrap_or(SignedDuration::MAX)
    }

    fn sleep_until(&self, deadline: Timestamp) -> BoxFuture<()> {
        // Relative sleep, re-checked on wake: wall-clock steps (NTP) can make
        // a single sleep end early.
        Box::pin(async move {
            loop {
                let left = deadline.duration_since(Timestamp::now());
                if left <= SignedDuration::ZERO {
                    return;
                }
                tokio::time::sleep(to_std(left)).await;
            }
        })
    }

    fn sleep(&self, duration: SignedDuration) -> BoxFuture<()> {
        Box::pin(tokio::time::sleep(to_std(duration)))
    }
}

/// Fake time that starts at a fixed timestamp and advances with tokio's
/// clock. Under `tokio::time::pause()` (e.g. `#[tokio::test(start_paused =
/// true)]`) it is fully deterministic: time jumps forward whenever every
/// task is idle. Wall and monotonic time advance together.
#[derive(Debug, Clone)]
pub struct TokioClock {
    origin: Timestamp,
    start: tokio::time::Instant,
}

impl TokioClock {
    /// A clock reading `origin` now.
    pub fn starting_at(origin: Timestamp) -> Self {
        Self {
            origin,
            start: tokio::time::Instant::now(),
        }
    }
}

impl Clock for TokioClock {
    fn now(&self) -> Timestamp {
        self.origin
            .saturating_add(self.monotonic())
            .unwrap_or(Timestamp::MAX)
    }

    fn monotonic(&self) -> SignedDuration {
        SignedDuration::try_from(self.start.elapsed()).unwrap_or(SignedDuration::MAX)
    }

    fn sleep_until(&self, deadline: Timestamp) -> BoxFuture<()> {
        let offset = to_std(deadline.duration_since(self.origin));
        match self.start.checked_add(offset) {
            Some(at) => Box::pin(tokio::time::sleep_until(at)),
            None => Box::pin(std::future::pending()),
        }
    }

    fn sleep(&self, duration: SignedDuration) -> BoxFuture<()> {
        Box::pin(tokio::time::sleep(to_std(duration)))
    }
}

#[derive(Debug, Clone, Copy)]
struct Manual {
    wall: Timestamp,
    mono: SignedDuration,
}

/// Fake time that only moves when told to. Clones share the same time.
///
/// [`ManualClock::advance`] moves wall and monotonic time together;
/// [`ManualClock::step_wall`] and [`ManualClock::set`] move only the wall
/// clock, like an NTP step, so tests can check that intervals are measured
/// on monotonic time.
#[derive(Debug, Clone)]
pub struct ManualClock {
    tx: Arc<watch::Sender<Manual>>,
}

impl ManualClock {
    /// A clock reading `start`, with monotonic time zero.
    pub fn new(start: Timestamp) -> Self {
        Self {
            tx: Arc::new(watch::Sender::new(Manual {
                wall: start,
                mono: SignedDuration::ZERO,
            })),
        }
    }

    /// Let time pass: both clocks move forward by `by`. A negative duration
    /// only steps the wall clock back (monotonic time cannot go backwards).
    pub fn advance(&self, by: SignedDuration) {
        self.tx.send_modify(|t| {
            t.wall = t.wall.saturating_add(by).unwrap_or(Timestamp::MAX);
            if by > SignedDuration::ZERO {
                t.mono = t.mono.saturating_add(by);
            }
        });
    }

    /// Step only the wall clock (forwards or backwards).
    pub fn step_wall(&self, by: SignedDuration) {
        self.tx
            .send_modify(|t| t.wall = t.wall.saturating_add(by).unwrap_or(Timestamp::MAX));
    }

    /// Set only the wall clock.
    pub fn set(&self, to: Timestamp) {
        self.tx.send_modify(|t| t.wall = to);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        self.tx.borrow().wall
    }

    fn monotonic(&self) -> SignedDuration {
        self.tx.borrow().mono
    }

    fn sleep_until(&self, deadline: Timestamp) -> BoxFuture<()> {
        let mut rx = self.tx.subscribe();
        Box::pin(async move {
            // The sender lives as long as any clone; be defensive anyway.
            let _ = rx.wait_for(|t| t.wall >= deadline).await;
        })
    }

    fn sleep(&self, duration: SignedDuration) -> BoxFuture<()> {
        let mut rx = self.tx.subscribe();
        let until = self.monotonic().saturating_add(duration);
        Box::pin(async move {
            let _ = rx.wait_for(|t| t.mono >= until).await;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::ToSpan;

    fn t0() -> Timestamp {
        "2030-01-01T00:00:00Z".parse().unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn tokio_clock_follows_paused_time() {
        let clock = TokioClock::starting_at(t0());
        assert_eq!(clock.now(), t0());
        let deadline = t0() + 90.seconds();
        clock.sleep_until(deadline).await;
        assert_eq!(clock.now(), deadline);
        clock.sleep(SignedDuration::from_secs(10)).await;
        assert_eq!(clock.monotonic(), SignedDuration::from_secs(100));
    }

    #[tokio::test]
    async fn manual_clock_wakes_sleepers() {
        let clock = ManualClock::new(t0());
        let c2 = clock.clone();
        let sleeper = tokio::spawn(async move { c2.sleep_until(t0() + 60.seconds()).await });
        clock.advance(SignedDuration::from_secs(30));
        tokio::task::yield_now().await;
        assert!(!sleeper.is_finished());
        clock.advance(SignedDuration::from_secs(30));
        sleeper.await.unwrap();
        assert_eq!(clock.now(), t0() + 60.seconds());
    }

    #[tokio::test]
    async fn wall_steps_do_not_move_monotonic_time() {
        let clock = ManualClock::new(t0());
        let c2 = clock.clone();
        let sleeper = tokio::spawn(async move { c2.sleep(SignedDuration::from_secs(60)).await });
        clock.step_wall(SignedDuration::from_hours(5));
        clock.set(t0() - SignedDuration::from_hours(5));
        clock.advance(SignedDuration::from_secs(-30));
        tokio::task::yield_now().await;
        assert!(!sleeper.is_finished());
        assert_eq!(clock.monotonic(), SignedDuration::ZERO);
        clock.advance(SignedDuration::from_secs(60));
        sleeper.await.unwrap();
        assert_eq!(clock.monotonic(), SignedDuration::from_secs(60));
    }
}
