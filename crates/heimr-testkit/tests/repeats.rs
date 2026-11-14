//! `records.monotonic` judges repeats: across a restart any record
//! repeated for the same series and time must be identical, and a changed
//! value must outrank with a higher `changed_at` on a versioned series.
//! A re-sending adapter breaks one rule at a time on its re-sends.

#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use heimr_core::{
    Adapter, AdapterContext, AdapterDescriptor, AdapterError, Basis, Capabilities, DeviceId,
    Envelope, Metric, Quality, SeriesTemplate, TimeSource, Unit, revision_time,
};
use heimr_testkit::{Fault, Status, Suite, Traffic, Upstream, checks, t0};
use jiff::SignedDuration;
use serde::Deserialize;
use serde_json::json;

/// A source shared by the upstream and the adapter by id.
#[derive(Debug, Default)]
struct Source {
    fault: Mutex<Option<Fault>>,
    requests: AtomicU64,
    /// Runs of the adapter so far.
    runs: AtomicU64,
}

fn sources() -> &'static Mutex<HashMap<u64, Arc<Source>>> {
    static S: OnceLock<Mutex<HashMap<u64, Arc<Source>>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

fn source(id: u64) -> Arc<Source> {
    sources().lock().unwrap().entry(id).or_default().clone()
}

struct Up {
    id: u64,
    flags: Vec<&'static str>,
}

impl Upstream for Up {
    fn settings(&self) -> serde_json::Value {
        json!({ "source": self.id, "flags": self.flags })
    }
    fn inject(&self, fault: Option<Fault>) -> bool {
        *source(self.id).fault.lock().unwrap() = fault;
        true
    }
    fn traffic(&self) -> Option<Traffic> {
        Some(Traffic::new(
            source(self.id).requests.load(Ordering::Relaxed),
            0,
        ))
    }
}

#[derive(Debug, Deserialize)]
struct Config {
    source: u64,
    #[serde(default)]
    flags: Vec<String>,
}

/// Fetches a fixed window on every start and sends it: five readings of a
/// versioned `level`, five of a plain `count`, five forecasts of `wind`.
/// Every first send is right; the flags spoil the re-sends of later runs.
/// An outage makes it fail, so the supervisor restarts it.
struct Resender {
    flags: Vec<String>,
}

const TICK: SignedDuration = SignedDuration::from_secs(10);

impl Adapter for Resender {
    type Config = Config;

    fn descriptor() -> AdapterDescriptor {
        let m = |s: &str| Metric::new(s).unwrap();
        AdapterDescriptor::new("resender", "re-sends its window", Capabilities::read_only())
            .with_series(
                SeriesTemplate::gauge(m("level"), Unit::Percent, TICK)
                    .with_t_source(TimeSource::Device)
                    .versioned(),
            )
            .with_series(
                SeriesTemplate::gauge(m("count"), Unit::Dimensionless, TICK)
                    .with_t_source(TimeSource::Device),
            )
            .with_series(SeriesTemplate::forecast(
                m("wind"),
                Unit::MetrePerSecond,
                TICK,
            ))
    }

    fn new(c: &Config) -> Result<Self, AdapterError> {
        Ok(Self {
            flags: c.flags.clone(),
        })
    }

    async fn run(self, ctx: AdapterContext<Config>) -> Result<(), AdapterError> {
        let src = source(ctx.config().source);
        let has = |f: &str| self.flags.iter().any(|x| x == f);
        // A failed fetch at start-up is a failed run.
        src.requests.fetch_add(1, Ordering::Relaxed);
        if src.fault.lock().unwrap().is_some() {
            return Err(AdapterError::transient("source unavailable"));
        }
        let resend = src.runs.fetch_add(1, Ordering::Relaxed) > 0;
        ctx.health().ok().await?;
        let dev = DeviceId::new("probe").unwrap();
        let first_seen = revision_time(t0());
        for k in 0..5i32 {
            let t = t0() - SignedDuration::from_mins(i64::from(5 - k));
            let mut value = f64::from(k);
            let mut changed_at = first_seen;
            let mut quality = Quality::Good;
            if resend && has("new_changed_at") {
                changed_at = revision_time(ctx.now());
            }
            if resend && has("lower_changed_at") {
                value += 0.5;
                changed_at = first_seen - 1;
            }
            if resend && has("correction") {
                value += 0.5;
                changed_at = revision_time(ctx.now());
            }
            if resend && has("requality") {
                quality = Quality::Estimated;
            }
            let r = ctx
                .reading(
                    dev.clone(),
                    Metric::new("level").unwrap(),
                    value,
                    Unit::Percent,
                )
                .with_time(t)
                .with_quality(quality)
                .with_changed_at(changed_at);
            ctx.emit(r).await?;
            let plain = if resend && has("plain_changed") {
                f64::from(k) + 1.0
            } else {
                f64::from(k)
            };
            let c = ctx
                .reading(
                    dev.clone(),
                    Metric::new("count").unwrap(),
                    plain,
                    Unit::Dimensionless,
                )
                .with_time(t);
            ctx.emit(c).await?;
            let issued = if resend && has("restamp") {
                ctx.now()
            } else {
                t0()
            };
            let valid =
                t0() + SignedDuration::from_hours(1) + SignedDuration::from_mins(i64::from(k));
            let f = ctx
                .forecast(
                    dev.clone(),
                    Metric::new("wind").unwrap(),
                    3.0,
                    Unit::MetrePerSecond,
                    Basis::Modelled,
                )
                .with_issued_at(issued)
                .valid_at(valid)
                .with_changed_at(first_seen);
            ctx.emit(f).await?;
        }
        loop {
            tokio::select! {
                () = ctx.shutdown().wait() => return Ok(()),
                () = ctx.sleep(TICK) => {}
            }
            src.requests.fetch_add(1, Ordering::Relaxed);
            match *src.fault.lock().unwrap() {
                Some(Fault::Unavailable) => {
                    return Err(AdapterError::transient("source unavailable"));
                }
                Some(Fault::Rejected) => return Err(AdapterError::fatal("rejected")),
                _ => {}
            }
            ctx.health().ok().await?;
        }
    }
}

fn suite(flags: &[&'static str]) -> heimr_testkit::Report {
    let flags = flags.to_vec();
    Suite::with_upstream::<Resender, _, _, _>(move || {
        static N: AtomicU64 = AtomicU64::new(1 << 40);
        let flags = flags.clone();
        async move {
            Up {
                id: N.fetch_add(1, Ordering::Relaxed),
                flags,
            }
        }
    })
    .expect_skip("commands.shadow", "read-only")
    .expect_skip("commands.live", "read-only")
    .expect_note("records.keep_alive", "polled: no record-on-change series")
    .expect_skip("fault.malformed_no_panic", "no garbage to send")
    .run()
}

fn failures(r: &heimr_testkit::Report) -> BTreeMap<String, String> {
    r.results
        .iter()
        .filter(|x| x.status == Status::Failed)
        .map(|x| (x.id.clone(), x.reason.clone().unwrap_or_default()))
        .collect()
}

#[test]
fn identical_resends_and_outranking_corrections_pass() {
    for flags in [&[][..], &["correction"]] {
        let r = suite(flags);
        assert!(failures(&r).is_empty(), "{flags:?}\n{r}");
        assert_eq!(
            r.get("records.monotonic").unwrap().status,
            Status::Passed,
            "{r}"
        );
    }
}

/// Each spoiled re-send fails `records.monotonic` and `records.restart`,
/// and nothing else, with its reason.
#[test]
fn each_spoiled_resend_fails_with_its_reason() {
    let cases: &[(&[&str], &str)] = &[
        (
            &["restamp"],
            "forecast probe/wind at 2030-01-01T01:00:00Z: re-sent with its time re-stamped (issued at 2030-01-01T00",
        ),
        (
            &["requality"],
            "reading probe/level at 2029-12-31T23:55:00Z: quality changed on re-send (Estimated, was Good) without a new changed_at",
        ),
        (
            &["new_changed_at"],
            "reading probe/level at 2029-12-31T23:55:00Z: the same value re-sent with a different changed_at",
        ),
        (
            &["lower_changed_at"],
            "reading probe/level at 2029-12-31T23:55:00Z: a new value with changed_at 1893455999999999999, not above the 1893456000000000000 it replaces",
        ),
        (
            &["plain_changed"],
            "reading probe/count at 2029-12-31T23:55:00Z: the value changed on re-send, on a series that is not versioned",
        ),
    ];
    let mut missed = Vec::new();
    for (flags, why) in cases {
        let r = suite(flags);
        let f = failures(&r);
        // Caught across the supervisor restart and the app restart alike.
        for id in ["records.monotonic", "records.restart"] {
            match f.get(id) {
                Some(reason) if reason.contains(why) => {}
                other => missed.push(format!("{flags:?} {id}: {other:?}")),
            }
        }
        if f.len() != 2 {
            missed.push(format!("{flags:?}: also failed {f:?}"));
        }
    }
    assert!(missed.is_empty(), "{}", missed.join("\n"));
}

/// The check is a plain function over runs, here the harness's: an
/// identical re-send passes whether it follows a restart or comes within
/// one run (as after an error).
#[tokio::test]
async fn the_check_splits_by_run() {
    let mut h = heimr_testkit::Harness::start::<Resender>(json!({ "source": 7 }))
        .await
        .unwrap();
    h.restart().await.unwrap();
    let runs = h.runs();
    assert_eq!(runs.len(), 2);
    let facts = |run: &[Envelope]| {
        run.iter()
            .filter(|e| !matches!(e.record, heimr_core::Record::Event(_)))
            .count()
    };
    assert_eq!(facts(&runs[0]), 15);
    assert_eq!(facts(&runs[1]), 15, "the window again");
    let series = Resender::descriptor().series;
    assert!(checks::monotonic(&runs, &series).is_empty());
    let joined: Vec<Envelope> = runs.concat();
    assert!(checks::monotonic(&[joined], &series).is_empty());
    h.stop().await.unwrap();
}
