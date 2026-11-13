//! Kraken GraphQL contract tests against a mock server serving fixtures.

#![allow(clippy::unwrap_used)]

use jiff::Timestamp;
use octopus_energy::{Error, KrakenClient, TelemetryGrouping};
use secrecy::SecretString;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> serde_json::Value {
    let p = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn json(name: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(fixture(name))
}

const TOKEN: &str = "eyJ0eXAiOiJKV1QifQ.FAKE.FIXTURE";

async fn setup() -> (MockServer, KrakenClient) {
    let server = MockServer::start().await;
    let c = KrakenClient::with_base_url(
        SecretString::from("sk_test_not_a_real_key"),
        &server.uri(),
        reqwest::Client::new(),
    )
    .unwrap();
    (server, c)
}

async fn mount_token(server: &MockServer, times: u64) {
    Mock::given(method("POST"))
        .and(path("/v1/graphql/"))
        .and(body_string_contains("obtainKrakenToken"))
        .and(body_string_contains("sk_test_not_a_real_key"))
        .respond_with(json("kraken_token.json"))
        .expect(times)
        .mount(server)
        .await;
}

#[tokio::test]
async fn token_is_cached_and_sent_bare() {
    let (server, c) = setup().await;
    mount_token(&server, 1).await;
    Mock::given(path("/v1/graphql/"))
        .and(header("authorization", TOKEN))
        .and(body_string_contains("smartImportElectricityMeter"))
        .respond_with(json("kraken_meters.json"))
        .expect(2)
        .mount(&server)
        .await;
    let inv = c.meters("A-00000000").await.unwrap();
    let again = c.meters("A-00000000").await.unwrap();
    assert_eq!(inv, again);
    insta::assert_json_snapshot!(inv);
    assert_eq!(inv.electricity_device_id(), Some("00-00-00-00-00-00-00-01"));
    assert_eq!(
        inv.gas[0].meters[0].consumption_units.as_deref(),
        Some("m³")
    );
}

#[tokio::test]
async fn telemetry_decimals_parse_from_strings_numbers_and_nulls() {
    let (server, c) = setup().await;
    mount_token(&server, 1).await;
    Mock::given(path("/v1/graphql/"))
        .and(body_string_contains("smartMeterTelemetry"))
        .and(body_string_contains("FIVE_MINUTES"))
        .and(body_string_contains("00-00-00-00-00-00-00-01"))
        .respond_with(json("kraken_telemetry.json"))
        .mount(&server)
        .await;
    let start: Timestamp = "2026-11-12T09:00:00Z".parse().unwrap();
    let end: Timestamp = "2026-11-12T09:30:00Z".parse().unwrap();
    let t = c
        .telemetry(
            "00-00-00-00-00-00-00-01",
            start,
            end,
            TelemetryGrouping::FiveMinutes,
        )
        .await
        .unwrap();
    insta::assert_json_snapshot!(t);
    assert_eq!(t.len(), 6);
    assert_eq!(t[4].demand_w, None);
    // Cumulative register grows by the deltas.
    let deltas: f64 = t[1..].iter().filter_map(|x| x.import_delta_wh).sum();
    let growth = t[5].import_wh.unwrap() - t[0].import_wh.unwrap();
    assert!((deltas - growth).abs() < 1e-6);
    assert!((t[0].import_kwh().unwrap() - 4321.139).abs() < 1e-9);
}

#[tokio::test]
async fn latest_telemetry_picks_newest_reading() {
    let (server, c) = setup().await;
    mount_token(&server, 1).await;
    Mock::given(path("/v1/graphql/"))
        .and(body_string_contains("smartMeterTelemetry"))
        .respond_with(json("kraken_telemetry.json"))
        .mount(&server)
        .await;
    let latest = c
        .latest_telemetry("00-00-00-00-00-00-00-01")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        latest.read_at,
        "2026-11-12T09:25:00Z".parse::<Timestamp>().unwrap()
    );
}

#[tokio::test]
async fn rate_limit_error_code_maps_to_rate_limited() {
    let (server, c) = setup().await;
    mount_token(&server, 1).await;
    Mock::given(path("/v1/graphql/"))
        .and(body_string_contains("smartMeterTelemetry"))
        .respond_with(json("kraken_error_rate_limited.json"))
        .mount(&server)
        .await;
    let err = c.latest_telemetry("x").await.unwrap_err();
    assert!(matches!(err, Error::RateLimited { .. }), "{err:?}");
}

#[tokio::test]
async fn expired_token_is_refreshed_once() {
    let (server, c) = setup().await;
    mount_token(&server, 2).await;
    // First data call: token expired. Second: success.
    Mock::given(path("/v1/graphql/"))
        .and(body_string_contains("smartMeterTelemetry"))
        .respond_with(json("kraken_error_token_expired.json"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/v1/graphql/"))
        .and(body_string_contains("smartMeterTelemetry"))
        .respond_with(json("kraken_telemetry.json"))
        .expect(1)
        .mount(&server)
        .await;
    assert!(c.latest_telemetry("x").await.unwrap().is_some());
}
