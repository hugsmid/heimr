//! The whole conformance suite against a recorded (here: hand-built)
//! sensor, served by a local replay server.

use example_adapter::ExampleSensor;
use heimr_testkit::Suite;
use heimr_testkit::replay::Cassette;
use serde_json::json;

// ANCHOR: conformance
#[test]
fn conformance() {
    let cassette = Cassette::parse(include_str!("fixtures/sensor.json")).expect("a cassette");
    Suite::replay::<ExampleSensor>(
        cassette,
        |uri| json!({ "url": format!("{uri}/status"), "interval": "10s" }),
    )
    .invalid("url", json!({ "url": "not a url" }))
    .invalid("interval", json!({ "interval": "1s" }))
    .expect_skip("commands.shadow", "read-only sensor: no commands")
    .expect_skip("commands.live", "read-only sensor: no commands")
    .run()
    .assert_passed();
}
// ANCHOR_END: conformance
