//! The series allow-list and ceiling, run on the test kit's harness
//! against the fixture host.

#![allow(clippy::unwrap_used)]

mod common;

use std::collections::BTreeSet;

use heimr_core::HealthStatus;
use heimr_system::System;
use heimr_testkit::Harness;
use jiff::SignedDuration;

async fn run(patch: serde_json::Value) -> (common::Host, Harness) {
    let host = common::Host::new();
    let mut settings = host.settings();
    for (k, v) in patch.as_object().unwrap() {
        if v.is_null() {
            settings.as_object_mut().unwrap().remove(k);
        } else {
            settings[k] = v.clone();
        }
    }
    let mut h = Harness::start::<System>(settings).await.unwrap();
    // Two samples: rates need a second one.
    h.advance(SignedDuration::from_secs(10)).await;
    for _ in 0..10 {
        h.settle().await;
    }
    (host, h)
}

fn series(h: &mut Harness) -> BTreeSet<(String, String)> {
    h.readings()
        .iter()
        .map(|r| (r.device.to_string(), r.metric.to_string()))
        .collect()
}

#[tokio::test]
async fn only_listed_series_are_sent() {
    let (_host, mut h) = run(serde_json::json!({
        "series": ["cpu.usage", "memory.*", "host/load.1"],
    }))
    .await;
    let got = series(&mut h);
    assert!(!got.is_empty());
    for (device, metric) in &got {
        let ok = metric == "cpu.usage"
            || metric.starts_with("memory.")
            || (device == "host" && metric == "load.1");
        assert!(ok, "{device}/{metric}");
    }
    assert!(got.contains(&("host".into(), "load.1".into())));
    assert!(!got.iter().any(|(_, m)| m == "load.5"));
    assert!(got.iter().any(|(_, m)| m == "memory.total"));
    assert_eq!(h.stats().health, HealthStatus::Ok);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_ceiling_caps_the_series_and_degrades_health() {
    let (_host, mut h) = run(serde_json::json!({ "max_series": 5 })).await;
    assert_eq!(series(&mut h).len(), 5, "no more than the ceiling");
    let s = h.stats();
    assert_eq!(s.health, HealthStatus::Degraded);
    let reason = s.health_reason.unwrap();
    assert!(
        reason.contains("more series would exceed max_series (5); not sent"),
        "{reason}"
    );
    assert_eq!(s.gauges.get("series"), Some(&5.0), "{:?}", s.gauges);
    // Without a ceiling in the way, the same host is healthy.
    let (_host, mut free) = run(serde_json::json!({})).await;
    assert!(series(&mut free).len() > 5);
    assert_eq!(free.stats().health, HealthStatus::Ok);
    free.stop().await.unwrap();
    h.stop().await.unwrap();
}

/// By default only named processes are reported: no top-N `proc:`
/// series to churn and fill the ceiling.
#[tokio::test]
async fn top_processes_are_opt_in() {
    let (_host, mut h) = run(serde_json::json!({ "top": null })).await;
    let procs: BTreeSet<String> = series(&mut h)
        .into_iter()
        .filter(|(d, _)| d.starts_with("proc:"))
        .map(|(d, _)| d)
        .collect();
    assert_eq!(procs, BTreeSet::from(["proc:exampled".to_owned()]));
    h.stop().await.unwrap();
    let (_host, mut h) = run(serde_json::json!({ "top": 2 })).await;
    let procs = series(&mut h)
        .into_iter()
        .filter(|(d, _)| d.starts_with("proc:"))
        .map(|(d, _)| d)
        .collect::<BTreeSet<_>>();
    assert!(procs.len() > 1, "{procs:?}");
    h.stop().await.unwrap();
}
