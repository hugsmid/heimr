//! Contract tests against a mock server serving hand-built fixtures.

#![allow(clippy::unwrap_used)]

use jiff::Timestamp;
use secrecy::SecretString;
use weather::{
    BASE_15_5_C, Client, Error, ForecastOptions, IssuedAt, Location, heating_degree_days,
};
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> serde_json::Value {
    let p = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn json(name: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(fixture(name))
}

/// Reykjavík, as an example location.
fn reykjavik() -> Location {
    Location::new(64.15, -21.94).unwrap()
}

fn client(server: &MockServer) -> Client {
    Client::builder()
        .forecast_base_url(server.uri())
        .archive_base_url(server.uri())
        .historical_forecast_base_url(server.uri())
        .build()
        .unwrap()
}

/// The query every request must carry so units and times are unambiguous.
fn common(m: wiremock::MockBuilder) -> wiremock::MockBuilder {
    m.and(query_param("latitude", "64.15"))
        .and(query_param("longitude", "-21.94"))
        .and(query_param(
            "hourly",
            "temperature_2m,relative_humidity_2m,wind_speed_10m,wind_direction_10m,\
             precipitation,cloud_cover,shortwave_radiation,surface_pressure",
        ))
        .and(query_param("timeformat", "unixtime"))
        .and(query_param("timezone", "GMT"))
        .and(query_param("wind_speed_unit", "ms"))
        .and(query_param("temperature_unit", "celsius"))
        .and(query_param("precipitation_unit", "mm"))
        .and(query_param_is_missing("apikey"))
}

#[tokio::test]
async fn forecast_rows_carry_the_callers_issued_at() {
    let server = MockServer::start().await;
    common(Mock::given(method("GET")).and(path("/v1/forecast")))
        .and(query_param("forecast_days", "2"))
        .and(query_param_is_missing("models"))
        .respond_with(json("forecast.json"))
        .expect(1)
        .mount(&server)
        .await;

    let issued: Timestamp = "2026-11-12T05:00:00Z".parse().unwrap();
    let points = client(&server)
        .forecast(reykjavik(), &ForecastOptions::days(2), IssuedAt::At(issued))
        .await
        .unwrap();
    assert_eq!(points.len(), 48);
    assert!(points.iter().all(|p| p.issued_at == issued));
    assert_eq!(points[0].valid_at, "2026-11-12T00:00:00Z".parse().unwrap());
    assert_eq!(points[47].valid_at, "2026-11-13T23:00:00Z".parse().unwrap());
    // Trailing nulls at the longest lead time survive as None.
    assert_eq!(points[47].shortwave_wm2, None);
    assert_eq!(points[47].cloud_cover_pct, None);
    assert!(points[47].temperature_c.is_some());
    insta::assert_json_snapshot!(
        "forecast_first_and_last",
        [points[0], points[13], points[47]]
    );
}

#[tokio::test]
async fn fetch_time_issued_at_is_taken_around_the_request() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/forecast"))
        .and(query_param("models", "ukmo_seamless"))
        .respond_with(json("forecast.json"))
        .mount(&server)
        .await;
    let before = Timestamp::now();
    let opts = ForecastOptions {
        forecast_days: 2,
        models: Some("ukmo_seamless".into()),
    };
    let points = client(&server)
        .forecast(reykjavik(), &opts, IssuedAt::FetchTime)
        .await
        .unwrap();
    let after = Timestamp::now();
    let issued = points[0].issued_at;
    assert!(before <= issued && issued <= after);
    assert!(points.iter().all(|p| p.issued_at == issued));
}

#[tokio::test]
async fn forecast_days_out_of_range_never_hits_the_network() {
    let server = MockServer::start().await;
    let c = client(&server);
    for days in [0, 17] {
        let err = c
            .forecast(
                reykjavik(),
                &ForecastOptions::days(days),
                IssuedAt::FetchTime,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn archive_observations_and_degree_days() {
    let server = MockServer::start().await;
    common(Mock::given(path("/v1/archive")))
        .and(query_param("start_date", "2026-01-10"))
        .and(query_param("end_date", "2026-01-11"))
        .respond_with(json("archive.json"))
        .expect(1)
        .mount(&server)
        .await;

    let obs = client(&server)
        .archive(
            reykjavik(),
            "2026-01-10".parse().unwrap(),
            "2026-01-11".parse().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(obs.len(), 48);
    assert_eq!(obs[30].temperature_c, None);
    assert_eq!(obs[30].pressure_hpa, None);
    assert!(obs[30].humidity_pct.is_some());
    insta::assert_json_snapshot!("archive_first_rows", &obs[..3]);

    let dd = heating_degree_days(&obs, BASE_15_5_C);
    assert_eq!((dd.hours, dd.missing_hours), (47, 1));
    insta::assert_json_snapshot!("archive_degree_days", dd, {
        ".value" => insta::rounded_redaction(6),
    });
}

#[tokio::test]
async fn historical_forecast_uses_its_own_host() {
    let archive = MockServer::start().await;
    let hist = MockServer::start().await;
    common(Mock::given(path("/v1/forecast")))
        .and(query_param("start_date", "2026-11-08"))
        .and(query_param("end_date", "2026-11-08"))
        .respond_with(json("historical_forecast.json"))
        .expect(1)
        .mount(&hist)
        .await;
    let c = Client::builder()
        .archive_base_url(archive.uri())
        .historical_forecast_base_url(hist.uri())
        .build()
        .unwrap();
    let day = "2026-11-08".parse().unwrap();
    let obs = c.historical_forecast(reykjavik(), day, day).await.unwrap();
    assert_eq!(obs.len(), 24);
    assert_eq!(obs[0].time, "2026-11-08T00:00:00Z".parse().unwrap());
    assert!(archive.received_requests().await.unwrap().is_empty());

    let err = c
        .archive(reykjavik(), day, "2026-11-07".parse().unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidRequest(_)));
}

#[tokio::test]
async fn api_error_400_maps_reason() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/forecast"))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(fixture("error_invalid_variable.json")),
        )
        .mount(&server)
        .await;
    let err = client(&server)
        .forecast(reykjavik(), &ForecastOptions::days(1), IssuedAt::FetchTime)
        .await
        .unwrap_err();
    match err {
        Error::Api { status, reason } => {
            assert_eq!(status, 400);
            assert!(reason.starts_with("Cannot initialize"), "{reason}");
        }
        other => panic!("expected Api error, got {other:?}"),
    }
}

#[tokio::test]
async fn rate_limit_429_maps_retry_after_and_reason() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/archive"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "60")
                .set_body_json(fixture("error_rate_limited.json")),
        )
        .mount(&server)
        .await;
    let day = "2026-01-10".parse().unwrap();
    let err = client(&server)
        .archive(reykjavik(), day, day)
        .await
        .unwrap_err();
    match err {
        Error::RateLimited {
            retry_after,
            reason,
        } => {
            assert_eq!(retry_after, Some(jiff::SignedDuration::from_secs(60)));
            assert!(reason.unwrap().contains("limit exceeded"));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn other_statuses_and_bad_bodies() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/forecast"))
        .respond_with(ResponseTemplate::new(502).set_body_string("<html>bad gateway</html>"))
        .mount(&server)
        .await;
    Mock::given(path("/v1/archive"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;
    let c = client(&server);
    let err = c
        .forecast(reykjavik(), &ForecastOptions::days(1), IssuedAt::FetchTime)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Status { status: 502, .. }), "{err}");
    let day = "2026-01-10".parse().unwrap();
    let err = c.archive(reykjavik(), day, day).await.unwrap_err();
    assert!(matches!(err, Error::Decode(_)), "{err}");
}

#[tokio::test]
async fn unit_and_length_drift_is_rejected() {
    let server = MockServer::start().await;
    let mut kmh = fixture("archive.json");
    kmh["hourly_units"]["wind_speed_10m"] = "km/h".into();
    let mut short = fixture("archive.json");
    short["hourly"]["cloud_cover"].as_array_mut().unwrap().pop();
    Mock::given(path("/v1/archive"))
        .respond_with(ResponseTemplate::new(200).set_body_json(kmh))
        .mount(&server)
        .await;
    Mock::given(path("/v1/forecast"))
        .respond_with(ResponseTemplate::new(200).set_body_json(short))
        .mount(&server)
        .await;
    let c = client(&server);
    let day = "2026-01-10".parse().unwrap();
    let err = c.archive(reykjavik(), day, day).await.unwrap_err();
    assert!(
        matches!(&err, Error::UnitMismatch { column, .. } if column == "wind_speed_10m"),
        "{err}"
    );
    let err = c
        .historical_forecast(reykjavik(), day, day)
        .await
        .unwrap_err();
    assert!(
        matches!(
            &err,
            Error::LengthMismatch { column, expected: 48, actual: 47 } if column == "cloud_cover"
        ),
        "{err}"
    );
}

#[tokio::test]
async fn api_key_is_sent_as_query_parameter() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/forecast"))
        .and(query_param("apikey", "test-key-not-real"))
        .respond_with(json("forecast.json"))
        .expect(1)
        .mount(&server)
        .await;
    let c = Client::builder()
        .forecast_base_url(server.uri())
        .api_key(SecretString::from("test-key-not-real"))
        .build()
        .unwrap();
    c.forecast(reykjavik(), &ForecastOptions::days(2), IssuedAt::FetchTime)
        .await
        .unwrap();
}
