//! The heimr conformance suite against replayed Open-Meteo responses.

mod common;

use heimr_testkit::{ClockMode, Suite};
use heimr_weather::OpenMeteo;
use jiff::SignedDuration;
use serde_json::json;

const POLLED: &str = "polled: no record-on-change series";
const READ_ONLY: &str = "read-only adapter: no commands";

#[test]
fn conformance() {
    Suite::replay::<OpenMeteo>(common::cassette(), |uri| common::settings(uri, "5m", "15m"))
        // Polls every 5 minutes at the most: an outage must span a poll.
        .outage(SignedDuration::from_mins(10))
        .clock(ClockMode::Manual {
            step: SignedDuration::from_secs(5),
            settle: std::time::Duration::from_millis(3),
        })
        .invalid("latitude", json!({ "latitude": 91.0 }))
        .invalid("latitude", json!({ "latitude": null }))
        .invalid("longitude", json!({ "longitude": -180.5 }))
        .invalid("forecast_interval", json!({ "forecast_interval": "1m" }))
        .invalid("observed_interval", json!({ "observed_interval": "25h" }))
        .invalid("longitude", json!({ "longitude": null }))
        .invalid("forecast_days", json!({ "forecast_days": 17 }))
        .invalid("forecast_url", json!({ "forecast_url": "not a url" }))
        .invalid("historical_url", json!({ "historical_url": "file:///etc" }))
        .expect_skip("commands.shadow", READ_ONLY)
        .expect_skip("commands.live", READ_ONLY)
        .expect_note("records.keep_alive", POLLED)
        .run()
        .assert_passed();
}

/// The cassettes hold nothing the recorder's scan would refuse.
#[test]
fn cassettes_pass_the_redaction_scan() {
    let found = heimr_testkit::replay::Redactor::new().scan(&common::cassette());
    assert!(found.is_empty(), "{found:?}");
}
