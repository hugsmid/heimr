//! Behaviour on the test kit's harness against replayed responses built
//! from the `weather` client's hand-built fixtures.

#![allow(clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use common::{HOUR, same_forecast, same_reading, value};
use heimr_core::{Basis, Forecast, HealthStatus, ManualClock, Quality, Reading, Unit};
use heimr_testkit::replay::{Cassette, ReplayServer};
use heimr_testkit::{Fault, Harness, t0};
use heimr_weather::OpenMeteo;
use jiff::{SignedDuration, Timestamp};
use serde_json::json;

const MIN: SignedDuration = SignedDuration::from_mins(1);

async fn start(cassette: Cassette, forecast: &str, observed: &str) -> (ReplayServer, Harness) {
    let server = ReplayServer::start(cassette).await;
    let h = Harness::start::<OpenMeteo>(common::settings(&server.uri(), forecast, observed))
        .await
        .unwrap();
    let mut h = h;
    settle(&mut h).await;
    (server, h)
}

/// Let real I/O against the local server finish.
async fn settle(h: &mut Harness) {
    for _ in 0..20 {
        h.settle().await;
    }
}

fn of<'a>(f: &'a [Forecast], metric: &str) -> Vec<&'a Forecast> {
    f.iter().filter(|x| x.metric.as_str() == metric).collect()
}

fn readings_of<'a>(r: &'a [Reading], metric: &str) -> Vec<&'a Reading> {
    r.iter().filter(|x| x.metric.as_str() == metric).collect()
}

#[tokio::test]
async fn first_poll_emits_forecasts_and_recent_hours() {
    let (server, mut h) = start(common::cassette(), "1h", "3h").await;
    let forecasts = h.forecasts();
    let fixture = common::forecast();
    // 48 hours from the clock's start; the fixture leaves cloud cover and
    // precipitation out at the longest lead.
    assert_eq!(of(&forecasts, "temperature").len(), 48);
    assert_eq!(of(&forecasts, "cloud_cover").len(), 47);
    assert_eq!(of(&forecasts, "precipitation").len(), 47);
    assert_eq!(of(&forecasts, "shortwave_radiation").len(), 47);
    assert_eq!(forecasts.len(), 48 * 5 + 47 * 3);
    assert!(forecasts.iter().all(|f| f.issued_at == t0()
        && f.basis == Basis::Modelled
        && f.changed_at == Some(heimr_core::revision_time(t0()))));
    let t = of(&forecasts, "temperature");
    assert_eq!(t[3].valid_from, t0() + 3 * HOUR);
    assert_eq!(t[3].valid_to, t[3].valid_from, "an instant");
    assert_eq!(t[3].value, value(&fixture, "temperature_2m", 3));
    assert_eq!(t[3].unit, Unit::Celsius);
    // Precipitation is the sum over the hour before the row's time.
    let p = of(&forecasts, "precipitation");
    assert_eq!(
        (p[5].valid_from, p[5].valid_to),
        (t0() + 4 * HOUR, t0() + 5 * HOUR)
    );
    assert_eq!(p[5].value, value(&fixture, "precipitation", 5));
    let sw = of(&forecasts, "shortwave_radiation");
    assert_eq!(
        (sw[5].valid_from, sw[5].valid_to, sw[5].unit),
        (t0() + 4 * HOUR, t0() + 5 * HOUR, Unit::WattPerSquareMetre)
    );
    let dir = of(&forecasts, "wind_direction");
    assert_eq!(
        (dir[2].valid_from, dir[2].unit),
        (t0() + 2 * HOUR, Unit::Degree)
    );
    assert_eq!(dir[2].value, value(&fixture, "wind_direction_10m", 2));

    let readings = h.readings();
    let past = common::historical();
    assert_eq!(readings.len(), 24 * 8, "24 recent hours, eight metrics");
    assert!(readings.iter().all(|r| r.quality == Quality::Estimated
        && r.changed_at == Some(heimr_core::revision_time(t0()))));
    let temp = readings_of(&readings, "temperature");
    assert_eq!(temp[0].time, t0() - 24 * HOUR);
    assert_eq!(temp[0].value, value(&past, "temperature_2m", 0));
    // An interval reading is stamped at the start of its interval.
    let rain = readings_of(&readings, "precipitation");
    assert_eq!(rain[0].time, t0() - 25 * HOUR);
    assert_eq!(rain[0].value, value(&past, "precipitation", 0));
    let sun = readings_of(&readings, "shortwave_radiation");
    assert_eq!(
        (sun[0].time, sun[0].unit),
        (t0() - 25 * HOUR, Unit::WattPerSquareMetre)
    );
    assert_eq!(sun[0].value, value(&past, "shortwave_radiation", 0));
    let dir = readings_of(&readings, "wind_direction");
    assert_eq!((dir[0].time, dir[0].unit), (t0() - 24 * HOUR, Unit::Degree));
    assert_eq!(
        readings_of(&readings, "pressure")[0].unit,
        Unit::Hectopascal
    );
    assert_eq!(h.stats().health, HealthStatus::Ok);
    assert_eq!(server.traffic().requests, 2);
    assert!(server.unmatched().is_empty());
    h.stop().await.unwrap();
}

/// Later fetches send only what is new or changed: an unchanged forecast
/// is not re-issued, and a recent hour whose value changed (reanalysis
/// replacing the stitched forecast) is sent again with a higher
/// `changed_at`.
#[tokio::test]
async fn later_fetches_send_only_new_or_changed_values() {
    // The second historical answer is three hours later: it adds the
    // three hours from the clock's start, and the 21 hours it shares with
    // the first carry other values (the fixture moved by three hours).
    let later = common::shifted(common::HISTORICAL, t0() - 21 * HOUR);
    let cassette = common::cassette_with(vec![common::historical(), later]);
    let (server, mut h) = start(cassette, "1h", "3h").await;
    let first = h.readings();
    assert_eq!(first.len(), 192);
    let forecasts = h.forecasts().len();

    h.advance(HOUR).await;
    settle(&mut h).await;
    assert_eq!(
        h.forecasts().len(),
        forecasts,
        "the same forecast: nothing new"
    );
    assert_eq!(
        h.readings().len(),
        192,
        "recent hours wait for their interval"
    );

    h.advance(2 * HOUR).await;
    settle(&mut h).await;
    let readings = h.readings();
    let second = &readings[192..];
    let fetched = t0() + 3 * HOUR;
    for r in second {
        let old = first
            .iter()
            .rev()
            .find(|o| o.metric == r.metric && o.time == r.time);
        match old {
            // A changed value: a new revision, never a repeat.
            Some(o) => {
                assert_ne!(o.value.to_bits(), r.value.to_bits(), "{r:?}");
                assert!(r.changed_at > o.changed_at);
            }
            None => assert!(r.time >= t0() - HOUR, "only the new hours are new: {r:?}"),
        }
        assert_eq!(r.changed_at, Some(heimr_core::revision_time(fetched)));
    }
    // Hours t0, t0+1h and t0+2h, where the second answer ends, are new.
    let temp = readings_of(second, "temperature");
    assert_eq!(temp.last().unwrap().time, t0() + 2 * HOUR);
    assert!(second.len() > 3 * 8, "corrections too: {}", second.len());
    assert!(second.len() <= 24 * 8);
    assert_eq!(server.traffic().requests, 5);
    assert_eq!(h.stats().emit_errors, 0);
    // Corrections outrank: the kit's repeat rule agrees.
    let series = <OpenMeteo as heimr_core::Adapter>::descriptor().series;
    assert!(heimr_testkit::checks::monotonic(&h.runs(), &series).is_empty());
    h.stop().await.unwrap();
}

/// A restart re-sends both windows: every forecast hour and recent hour
/// comes again bit for bit, issue time and `changed_at` included.
#[tokio::test]
async fn a_restart_resends_both_windows_bit_identically() {
    let (_server, mut h) = start(common::cassette(), "1h", "3h").await;
    let (readings, forecasts) = (h.readings(), h.forecasts());
    assert_eq!(readings.len(), 192);
    // A cursor left by an earlier version goes once the map is saved.
    let legacy = h.state_dir().join("cursor.json");
    std::fs::write(&legacy, "{}").unwrap();
    h.advance(10 * MIN).await;
    h.restart().await.unwrap();
    settle(&mut h).await;
    assert!(!legacy.exists(), "the old cursor is removed");
    let again = h.readings();
    assert_eq!(again.len(), 384, "the window again");
    for (a, b) in readings.iter().zip(&again[192..]) {
        assert!(same_reading(a, b), "{a:?}\n{b:?}");
    }
    let f = h.forecasts();
    // Hours already past ten minutes later are not re-sent.
    let resent = &f[forecasts.len()..];
    assert!(!resent.is_empty());
    for b in resent {
        let a = forecasts
            .iter()
            .find(|a| a.metric == b.metric && a.valid_from == b.valid_from)
            .unwrap();
        assert!(same_forecast(a, b), "{a:?}\n{b:?}");
        assert_eq!(b.issued_at, t0(), "first seen before the restart");
    }
    assert_eq!(h.stats().emit_errors, 0);
    let series = <OpenMeteo as heimr_core::Adapter>::descriptor().series;
    assert!(heimr_testkit::checks::monotonic(&h.runs(), &series).is_empty());
    h.stop().await.unwrap();
}

#[tokio::test]
async fn an_outage_backs_off_recovers_and_resends() {
    let (server, mut h) = start(common::cassette(), "1h", "3h").await;
    let forecasts = h.forecasts().len();
    let before = server.traffic().requests;
    server.set_fault(Some(Fault::Unavailable));
    h.advance_by_steps(HOUR + 30 * MIN, 10 * SignedDuration::from_secs(1))
        .await;
    let during = server.traffic().requests - before;
    // The hourly poll at +1h, then retries after 30 s, 1, 2, 4, 8 and 16
    // minutes: a handful, not a loop.
    assert!((3..=8).contains(&during), "{during} requests");
    assert_eq!(h.stats().health, HealthStatus::Degraded);
    assert!(!h.is_finished(), "an outage is not fatal");
    server.set_fault(None);
    // Past the next fetch of recent hours (due three hours in).
    h.advance_by_steps(2 * HOUR, 10 * SignedDuration::from_secs(1))
        .await;
    assert_eq!(h.stats().health, HealthStatus::Ok);
    // After the error, both windows are re-sent, unchanged.
    let readings = h.readings();
    assert_eq!(readings.len(), 384, "the recent hours again");
    for (a, b) in readings[..192].iter().zip(&readings[192..]) {
        assert!(same_reading(a, b));
    }
    let f = h.forecasts();
    assert!(f.len() > forecasts, "the forecast window again");
    assert!(f[forecasts..].iter().all(|x| x.issued_at == t0()));
    assert_eq!(h.stats().emit_errors, 0);
    h.stop().await.unwrap();
}

/// A failed save of the revisions map fails the fetch: nothing minted is
/// sent until it is saved; once writable again, the next fetch saves and
/// re-sends.
#[tokio::test]
async fn a_failed_save_sends_nothing_until_saved() {
    // The second historical answer moves the values: every hour changes.
    let later = common::shifted(common::HISTORICAL, t0() - 21 * HOUR);
    let (_server, mut h) = start(
        common::cassette_with(vec![common::historical(), later]),
        "1h",
        "3h",
    )
    .await;
    assert_eq!(h.readings().len(), 192);
    let blocker = h.state_dir().join("revisions.json.tmp");
    std::fs::create_dir(&blocker).unwrap();
    h.advance(3 * HOUR).await;
    settle(&mut h).await;
    assert_eq!(h.readings().len(), 192, "nothing sent while unsaved");
    let s = h.stats();
    assert_eq!(s.health, HealthStatus::Degraded);
    assert!(
        s.health_reason
            .as_deref()
            .unwrap()
            .contains("cannot save the revisions map"),
        "{s:?}"
    );
    std::fs::remove_dir(&blocker).unwrap();
    h.advance_by_steps(2 * MIN, 10 * SignedDuration::from_secs(1))
        .await;
    settle(&mut h).await;
    let readings = h.readings();
    assert!(readings.len() > 192, "saved, then sent");
    assert_eq!(h.stats().health, HealthStatus::Ok);
    let series = <OpenMeteo as heimr_core::Adapter>::descriptor().series;
    assert!(heimr_testkit::checks::monotonic(&h.runs(), &series).is_empty());
    h.stop().await.unwrap();
}

#[tokio::test]
async fn a_rate_limit_waits_as_long_as_asked() {
    let rate_limited: serde_json::Value = serde_json::from_str(common::RATE_LIMITED).unwrap();
    let cassette: Cassette = serde_json::from_value(json!({ "interactions": [
        { "request": { "method": "GET", "path": "/v1/forecast",
                       "query": { "forecast_days": "2" } },
          "responses": [
            { "status": 429, "headers": { "retry-after": "600" }, "json": rate_limited },
            { "status": 200, "json": common::forecast() }
          ] },
        { "request": { "method": "GET", "path": "/v1/forecast" },
          "responses": [ { "status": 200, "json": common::historical() } ] }
    ]}))
    .unwrap();
    let (server, mut h) = start(cassette, "1h", "3h").await;
    assert!(h.forecasts().is_empty());
    assert_eq!(h.stats().health, HealthStatus::Degraded);
    let reason = h.stats().health_reason.unwrap();
    assert!(reason.contains("rate limited"), "{reason}");
    h.advance_by_steps(9 * MIN, 10 * SignedDuration::from_secs(1))
        .await;
    assert_eq!(server.traffic().requests, 1, "nothing before Retry-After");
    h.advance_by_steps(2 * MIN, 10 * SignedDuration::from_secs(1))
        .await;
    assert_eq!(h.stats().health, HealthStatus::Ok);
    assert!(!h.forecasts().is_empty());
    assert_eq!(h.readings().len(), 192);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn refused_and_invalid_requests_are_fatal() {
    let (server, mut h) = start(common::cassette(), "1h", "3h").await;
    server.set_fault(Some(Fault::Rejected));
    h.advance(HOUR).await;
    settle(&mut h).await;
    assert!(h.is_finished());
    let err = h.stop().await.unwrap_err();
    assert!(
        err.is_fatal() && err.to_string().contains("HTTP 401"),
        "{err}"
    );

    let invalid: serde_json::Value = serde_json::from_str(common::INVALID_VARIABLE).unwrap();
    let mut c = Cassette::default();
    c.push(heimr_testkit::replay::Interaction::get("/v1/forecast").json(400, invalid));
    let (_server, mut h) = start(c, "1h", "3h").await;
    settle(&mut h).await;
    assert!(h.is_finished());
    assert_eq!(h.stats().health, HealthStatus::Failed);
    assert!(h.stop().await.unwrap_err().is_fatal());
}

#[tokio::test]
async fn garbage_is_survived() {
    let (server, mut h) = start(common::cassette(), "5m", "15m").await;
    server.set_fault(Some(Fault::Malformed));
    h.advance_by_steps(HOUR, 10 * SignedDuration::from_secs(1))
        .await;
    assert!(!h.is_finished());
    assert_eq!(h.stats().health, HealthStatus::Degraded);
    assert_eq!(h.stats().emit_errors, 0);
    h.stop().await.unwrap();
}

/// Forecasts beyond the app's horizon are dropped, not refused.
#[tokio::test]
async fn the_app_horizon_trims_forecasts() {
    let server = ReplayServer::start(common::cassette()).await;
    let dir = std::env::temp_dir().join(format!("heimr-weather-app-{}", std::process::id()));
    let uri = server.uri();
    let toml = format!(
        "[site]\ntime_zone = \"UTC\"\ncurrency = \"EUR\"\n\
         [heimr]\nstate_dir = \"{}\"\nforecast_horizon = \"24h\"\n\
         [sinks]\nlogging = false\n\
         [adapters.weather]\ntype = \"open-meteo\"\nlatitude = 0.0\nlongitude = 0.0\n\
         forecast_days = 2\nforecast_url = \"{uri}\"\nhistorical_url = \"{uri}\"\n",
        dir.display()
    );
    let config = heimr::Config::from_toml_str(&toml).unwrap();
    let sink = heimr::MemorySink::new(10_000);
    let app = heimr::App::builder(config)
        .public_adapters()
        .sink(sink.clone())
        .clock(Arc::new(ManualClock::new(t0())))
        .build()
        .await
        .unwrap();
    let telemetry = app.telemetry();
    let stop = app.shutdown_trigger();
    let running = tokio::spawn(app.run_until(std::future::pending()));
    // Records reach the sink in order, so once the instance's "ok" health
    // report (sent after both fetches) is there, everything before it is.
    // The app's clock is manual and never moves; only the local HTTP
    // round trips take real time.
    let reported_ok = || {
        sink.events().iter().any(|e| {
            e.kind == heimr_core::Event::ADAPTER_HEALTH
                && e.actor == "weather"
                && e.payload["status"] == "ok"
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while !reported_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a first round within 30 s");
    let stats = telemetry.get("weather").unwrap();
    stop.trigger();
    running.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let f = sink.forecasts();
    assert_eq!(
        f.iter()
            .filter(|x| x.metric.as_str() == "temperature")
            .count(),
        25,
        "t0 ..= t0+24h"
    );
    assert!(f.iter().all(|x| x.valid_to <= t0() + 24 * HOUR));
    assert_eq!(stats.emit_errors, 0);
}

/// An empty or garbage revisions map is reported as degraded health
/// naming the file, and rebuilt: the recent hours are sent again with new,
/// higher `changed_at` values (the same values, re-issued); the next good
/// round reports ok.
#[tokio::test]
async fn an_unreadable_map_is_reported_and_rebuilt() {
    for bad in ["", "yesterday-ish"] {
        let (_server, mut h) = start(common::cassette(), "1h", "3h").await;
        assert_eq!(h.readings().len(), 192);
        std::fs::write(h.state_dir().join("revisions.json"), bad).unwrap();
        h.advance(10 * MIN).await;
        h.restart().await.unwrap();
        settle(&mut h).await;
        let s = h.stats();
        assert_eq!(s.health, HealthStatus::Degraded, "{bad:?}");
        let reason = s.health_reason.unwrap();
        assert!(
            reason.contains("revisions.json")
                && reason.contains("unreadable")
                && reason.contains("rebuilt"),
            "{reason}"
        );
        let readings = h.readings();
        assert_eq!(readings.len(), 384, "the same 192 values again");
        for (a, b) in readings[..192].iter().zip(&readings[192..]) {
            assert_eq!((a.time, a.value.to_bits()), (b.time, b.value.to_bits()));
            assert!(b.changed_at > a.changed_at, "rebuilt markers look newer");
        }
        h.advance(HOUR).await;
        settle(&mut h).await;
        assert_eq!(h.stats().health, HealthStatus::Ok);
        assert_eq!(h.readings().len(), 384);
        h.stop().await.unwrap();
    }
}

/// `historical()` with `column` set to null at `rows`.
fn with_nulls(column: &str, rows: &[usize]) -> serde_json::Value {
    let mut v = common::historical();
    for &r in rows {
        v["hourly"][column][r] = serde_json::Value::Null;
    }
    v
}

/// A value missing from a recent hour is not sent; a later fetch that
/// has it sends it then, and the hours around it are not repeated.
#[tokio::test]
async fn a_missing_recent_value_is_sent_when_filled() {
    // Row 14 is never filled; row 22 is filled by the second answer.
    let first = with_nulls("temperature_2m", &[14, 22]);
    let later = with_nulls("temperature_2m", &[14]);
    let (_server, mut h) = start(common::cassette_with(vec![first, later]), "1h", "3h").await;
    let temp = |h: &mut Harness| -> Vec<Timestamp> {
        readings_of(&h.readings(), "temperature")
            .iter()
            .map(|r| r.time)
            .collect()
    };
    let at = |row: i32| t0() - 24 * HOUR + row * HOUR;
    let before = temp(&mut h);
    assert_eq!(before.len(), 22, "all but rows 14 and 22");
    assert!(!before.contains(&at(14)) && !before.contains(&at(22)));
    h.advance(3 * HOUR).await;
    settle(&mut h).await;
    let after = temp(&mut h);
    assert_eq!(after.len(), 23, "row 22 once filled, nothing else again");
    assert_eq!(after[22], at(22));
    h.stop().await.unwrap();
}

/// After the clock steps back, forecast hours first seen "later" are held
/// back rather than refused, the recent-hours request still has start <=
/// end, and the adapter keeps running; once the clock is past the first
/// sighting again, the held hours go out unchanged.
#[tokio::test]
async fn a_clock_stepped_back_holds_back_and_is_not_fatal() {
    let (server, mut h) = start(common::cassette(), "1h", "3h").await;
    let forecasts = h.forecasts();
    assert_eq!(h.readings().len(), 192);
    // Move the saved first sightings two hours ahead of the clock, as if
    // the clock had stepped back two hours since.
    let path = h.state_dir().join("revisions.json");
    let mut rows: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let ahead = t0() + 2 * HOUR;
    for r in &mut rows {
        r["first_seen"] = json!(ahead.to_string());
    }
    std::fs::write(&path, serde_json::to_string(&rows).unwrap()).unwrap();
    h.restart().await.unwrap();
    settle(&mut h).await;
    assert!(!h.is_finished());
    assert_eq!(h.stats().health, HealthStatus::Ok);
    assert_eq!(h.stats().emit_errors, 0, "nothing refused");
    assert_eq!(h.forecasts().len(), forecasts.len(), "forecasts held back");
    assert_eq!(
        h.readings().len(),
        384,
        "readings carry no issue time: re-sent"
    );
    let last = server.received().pop().unwrap();
    let date = |key: &str| {
        last.query
            .split('&')
            .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
            .unwrap()
            .to_owned()
    };
    assert!(date("start_date") <= date("end_date"), "{last}");
    // Past the first sighting, the next fetch sends them.
    h.advance(2 * HOUR).await;
    settle(&mut h).await;
    let held: Vec<_> = h.forecasts()[forecasts.len()..].to_vec();
    assert!(!held.is_empty());
    assert!(held.iter().all(|f| f.issued_at == ahead));
    assert_eq!(h.stats().emit_errors, 0);
    h.stop().await.unwrap();
}

/// Rows at the edge of the representable range (far outside the
/// lookback) are not sent, and nothing panics.
#[tokio::test]
async fn an_hour_at_the_edge_of_time_is_not_a_panic() {
    let edge = common::shifted(common::HISTORICAL, Timestamp::MIN);
    let (_server, mut h) = start(common::cassette_with(vec![edge]), "1h", "3h").await;
    assert!(!h.is_finished());
    assert!(h.readings().is_empty(), "outside the lookback");
    assert_eq!(h.stats().health, HealthStatus::Ok);
    assert_eq!(h.stats().emit_errors, 0);
    h.stop().await.unwrap();
}
