//! Unit-level behaviour with the direct-drive harness and a replay server.

use example_adapter::ExampleSensor;
use heimr_testkit::Harness;
use heimr_testkit::render::Render;
use heimr_testkit::replay::{Cassette, ReplayServer};
use jiff::SignedDuration;
use serde_json::json;

#[tokio::test]
async fn emits_temperature_and_humidity_each_poll() {
    let cassette = Cassette::parse(include_str!("fixtures/sensor.json")).expect("a cassette");
    let server = ReplayServer::start(cassette).await;
    let settings = json!({ "url": format!("{}/status", server.uri()), "interval": "10s" });
    let mut h = Harness::start::<ExampleSensor>(settings)
        .await
        .expect("starts");
    for _ in 0..2 {
        h.advance(SignedDuration::from_secs(10)).await;
    }
    let records = h.stop().await.expect("stops cleanly");
    let text = Render::new()
        .relative_to(heimr_testkit::t0())
        .skip_health()
        .render(&records);
    assert_eq!(
        text,
        "+0s reading under-test sensor-1 temperature 19.5 °C\n\
         +0s reading under-test sensor-1 humidity 44 %\n\
         +10s reading under-test sensor-1 temperature 19.75 °C\n\
         +10s reading under-test sensor-1 humidity 43.5 %\n\
         +20s reading under-test sensor-1 temperature 20 °C\n\
         +20s reading under-test sensor-1 humidity 43 %\n"
    );
    assert!(server.unmatched().is_empty());
    assert_eq!(server.traffic().writes, 0);
}
