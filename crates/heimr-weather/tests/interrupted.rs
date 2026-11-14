//! A fetch cut short (an emit refused because the app closed, or shutdown
//! while an emit waits) loses nothing: the next run re-sends the whole
//! window, and what the first run did send comes again bit for bit.

#![allow(clippy::unwrap_used)]

mod common;

use std::collections::BTreeSet;

use common::{StateDir, run_direct, same_reading};
use heimr_core::{Envelope, ManualClock, Reading, Record};
use heimr_testkit::replay::ReplayServer;
use heimr_testkit::t0;

/// Receive until the first round ends (the adapter reports health ok),
/// allowing at most 10 s of real time for the local server's I/O.
async fn until_round_done(rx: &mut tokio::sync::mpsc::Receiver<Envelope>) -> Vec<Envelope> {
    let mut out = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(e) = rx.recv().await {
            let done = matches!(&e.record, heimr_core::Record::Event(ev)
                if ev.kind == heimr_core::Event::ADAPTER_HEALTH && ev.payload["status"] == "ok");
            out.push(e);
            if done {
                break;
            }
        }
    })
    .await
    .expect("a round within 10 s");
    out
}

/// Wait (at most 10 s of real time) until the record channel is full: the
/// adapter is then waiting to emit its next record.
async fn until_full(rx: &tokio::sync::mpsc::Receiver<Envelope>) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while rx.len() < rx.max_capacity() {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the adapter blocks on a full channel");
}

/// Every reading.
fn readings(envelopes: &[Envelope]) -> Vec<Reading> {
    envelopes
        .iter()
        .filter_map(|e| match &e.record {
            Record::Reading(r) => Some(r.clone()),
            _ => None,
        })
        .collect()
}

async fn second_run(uri: &str, dir: &StateDir) -> Vec<Envelope> {
    let clock = ManualClock::new(t0() + jiff::SignedDuration::from_mins(5));
    let (mut rx, trigger, task) =
        run_direct(common::settings(uri, "1h", "3h"), &dir.0, clock, 8192);
    let mut out = until_round_done(&mut rx).await;
    trigger.trigger();
    task.await.unwrap().unwrap();
    while let Ok(e) = rx.try_recv() {
        out.push(e);
    }
    out
}

/// (metric, time) of every reading one uninterrupted run emits.
async fn complete() -> BTreeSet<(String, jiff::Timestamp)> {
    let server = ReplayServer::start(common::cassette()).await;
    let dir = StateDir::new("complete");
    readings(&second_run(&server.uri(), &dir).await)
        .into_iter()
        .map(|r| (r.metric.to_string(), r.time))
        .collect()
}

/// The second run sends every reading of the window; whatever the first
/// sent comes again identically.
fn assert_resent_identically(
    first: &[Envelope],
    second: &[Envelope],
    all: &BTreeSet<(String, jiff::Timestamp)>,
) {
    let (a, b) = (readings(first), readings(second));
    assert!(
        !a.is_empty() && a.len() < b.len(),
        "{} then {}",
        a.len(),
        b.len()
    );
    let keys: BTreeSet<_> = b.iter().map(|r| (r.metric.to_string(), r.time)).collect();
    assert_eq!(keys.len(), b.len(), "once each in the second run");
    assert_eq!(&keys, all, "nothing lost");
    for r in &a {
        let again = b
            .iter()
            .find(|x| x.metric == r.metric && x.time == r.time)
            .unwrap();
        assert!(same_reading(r, again), "{r:?}\n{again:?}");
    }
}

/// Receive until `n` readings have arrived; return everything received.
async fn take_readings(rx: &mut tokio::sync::mpsc::Receiver<Envelope>, n: usize) -> Vec<Envelope> {
    let mut out = Vec::new();
    while readings(&out).len() < n {
        out.push(rx.recv().await.unwrap());
    }
    out
}

#[tokio::test]
async fn an_emit_refused_mid_fetch_loses_nothing() {
    let all = complete().await;
    let server = ReplayServer::start(common::cassette()).await;
    let dir = StateDir::new("refused");
    let (mut rx, _trigger, task) = run_direct(
        common::settings(&server.uri(), "1h", "3h"),
        &dir.0,
        ManualClock::new(t0()),
        1,
    );
    let mut first = take_readings(&mut rx, 30).await;
    rx.close();
    while let Some(e) = rx.recv().await {
        first.push(e);
    }
    let err = task.await.unwrap().unwrap_err();
    assert!(err.to_string().contains("no longer receiving"), "{err}");
    let second = second_run(&server.uri(), &dir).await;
    assert_resent_identically(&first, &second, &all);
}

#[tokio::test]
async fn shutdown_mid_fetch_loses_nothing() {
    let all = complete().await;
    let server = ReplayServer::start(common::cassette()).await;
    let dir = StateDir::new("shutdown");
    let (mut rx, trigger, task) = run_direct(
        common::settings(&server.uri(), "1h", "3h"),
        &dir.0,
        ManualClock::new(t0()),
        1,
    );
    let mut first = take_readings(&mut rx, 30).await;
    until_full(&rx).await;
    trigger.trigger();
    task.await.unwrap().unwrap();
    while let Ok(e) = rx.try_recv() {
        first.push(e);
    }
    let second = second_run(&server.uri(), &dir).await;
    assert_resent_identically(&first, &second, &all);
}
