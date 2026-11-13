//! REST client contract tests against a mock server serving fixtures.

#![allow(clippy::unwrap_used)]

use jiff::Timestamp;
use octopus_energy::model::active_agreement;
use octopus_energy::prices::{cheapest, cheapest_window, slot_prices};
use octopus_energy::{Client, Error, Grouping, MeterId, Period, ProductFilter, Region, TariffCode};
use secrecy::SecretString;
use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> serde_json::Value {
    let p = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn json(name: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(fixture(name))
}

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

async fn client(server: &MockServer, key: bool) -> Client {
    let mut b = Client::builder().base_url(server.uri());
    if key {
        b = b.api_key(SecretString::from("sk_test_not_a_real_key"));
    }
    b.build().unwrap()
}

/// `Authorization: Basic base64("sk_test_not_a_real_key:")`.
const BASIC: &str = "Basic c2tfdGVzdF9ub3RfYV9yZWFsX2tleTo=";

/// The default host is the documented one.
#[test]
fn default_host_is_the_documented_one() {
    let c = Client::public().unwrap();
    assert_eq!(c.base_url().as_str(), "https://api.octopus.energy/v1/");
    assert_eq!(
        octopus_energy::rest::DEFAULT_BASE_URL,
        "https://api.octopus.energy"
    );
}

/// The fixture's `next` link names `api.octopus.energy`; page 2 must still be
/// fetched from the configured (mock) host.
#[tokio::test]
async fn pagination_links_are_rebased_onto_configured_host() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/products/"))
        .and(query_param("is_variable", "true"))
        .and(query_param_is_missing("page"))
        .respond_with(json("products_page1.json"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/products/"))
        .and(query_param("page", "2"))
        .respond_with(json("products_page2.json"))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server, false).await;
    let products = c
        .products(&ProductFilter {
            is_variable: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();
    insta::assert_json_snapshot!(products);
    let agile: Vec<_> = products
        .iter()
        .filter(|p| p.is_agile_import())
        .map(|p| &p.code)
        .collect();
    assert_eq!(agile, ["AGILE-24-10-01"]);
}

#[tokio::test]
async fn product_detail_exposes_regional_tariffs() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/products/AGILE-24-10-01/"))
        .respond_with(json("product_agile.json"))
        .mount(&server)
        .await;
    let p = client(&server, false)
        .await
        .product("AGILE-24-10-01", None)
        .await
        .unwrap();
    insta::assert_json_snapshot!(p);
    let c = p.electricity_tariff(Region::new('B').unwrap()).unwrap();
    assert_eq!(c.code.to_string(), "E-1R-AGILE-24-10-01-B");
    assert!(p.electricity_tariff(Region::new('P').unwrap()).is_none());
}

#[tokio::test]
async fn agile_rates_on_a_clocks_back_day_yield_50_slots() {
    let server = MockServer::start().await;
    Mock::given(path(
        "/v1/products/AGILE-24-10-01/electricity-tariffs/E-1R-AGILE-24-10-01-N/standard-unit-rates/",
    ))
    .and(query_param("period_from", "2026-10-24T23:00:00Z"))
    .and(query_param("period_to", "2026-10-26T00:00:00Z"))
    .and(query_param("page_size", "1500"))
    .respond_with(json("agile_unit_rates.json"))
    .mount(&server)
    .await;

    let tariff: TariffCode = "E-1R-AGILE-24-10-01-N".parse().unwrap();
    let (from, to) = (ts("2026-10-24T23:00:00Z"), ts("2026-10-26T00:00:00Z"));
    let rates = client(&server, false)
        .await
        .unit_rates(&tariff, Period::new(from, to))
        .await
        .unwrap();
    assert!(
        rates.windows(2).all(|w| w[0].valid_from < w[1].valid_from),
        "sorted oldest first"
    );

    let slots = slot_prices(&rates, from, to).unwrap();
    assert_eq!(slots.len(), 50);
    assert!(
        slots.iter().any(|s| s.inc_vat < 0.0),
        "fixture includes plunge pricing"
    );
    insta::assert_json_snapshot!("agile_cheapest_3", cheapest(&slots, 3));
    let (window, mean) = cheapest_window(&slots, 4).unwrap();
    insta::assert_json_snapshot!("agile_cheapest_2h", (window.first().map(|s| s.slot), mean));
}

#[tokio::test]
async fn standing_charges_and_gas_rates() {
    let server = MockServer::start().await;
    Mock::given(path(
        "/v1/products/AGILE-24-10-01/electricity-tariffs/E-1R-AGILE-24-10-01-N/standing-charges/",
    ))
    .respond_with(json("agile_standing_charges.json"))
    .mount(&server)
    .await;
    Mock::given(path(
        "/v1/products/VAR-22-11-01/gas-tariffs/G-1R-VAR-22-11-01-N/standard-unit-rates/",
    ))
    .respond_with(json("gas_unit_rates.json"))
    .mount(&server)
    .await;
    let c = client(&server, false).await;
    let since = Period::since(ts("2026-01-01T00:00:00Z"));
    let sc = c
        .standing_charges(&"E-1R-AGILE-24-10-01-N".parse().unwrap(), since)
        .await
        .unwrap();
    let gas = c
        .unit_rates(&"G-1R-VAR-22-11-01-N".parse().unwrap(), since)
        .await
        .unwrap();
    insta::assert_json_snapshot!((sc, &gas));
    // Flat gas rate expands to a full winter day of slots at the current price.
    let day = slot_prices(&gas, ts("2026-11-10T00:00:00Z"), ts("2026-11-11T00:00:00Z")).unwrap();
    assert_eq!(day.len(), 48);
    assert!(day.iter().all(|s| s.inc_vat == 6.1845));
}

#[tokio::test]
async fn electricity_consumption_is_authenticated_paginated_and_utc() {
    let server = MockServer::start().await;
    let p = "/v1/electricity-meter-points/0000000000000/meters/00A0000000/consumption/";
    Mock::given(path(p))
        .and(header("authorization", BASIC))
        .and(query_param("order_by", "period"))
        .and(query_param("page_size", "25000"))
        .and(query_param_is_missing("page"))
        .respond_with(json("consumption_electricity_page1.json"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path(p))
        .and(header("authorization", BASIC))
        .and(query_param("page", "2"))
        .respond_with(json("consumption_electricity_page2.json"))
        .expect(1)
        .mount(&server)
        .await;

    let meter = MeterId::Electricity {
        mpan: "0000000000000".into(),
        serial: "00A0000000".into(),
    };
    let rows = client(&server, true)
        .await
        .consumption(
            &meter,
            Period::new(ts("2026-06-30T23:00:00Z"), ts("2026-07-01T23:00:00Z")),
            None,
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 48);
    // "+01:00" offsets normalise to UTC instants.
    assert_eq!(rows[0].interval_start, ts("2026-06-30T23:00:00Z"));
    let total: f64 = rows.iter().map(|r| r.consumption).sum();
    insta::assert_json_snapshot!(
        "electricity_day_total_kwh",
        (total * 1000.0).round() / 1000.0
    );
    insta::assert_json_snapshot!("electricity_first_rows", &rows[..3]);
}

#[tokio::test]
async fn consumption_requires_an_api_key() {
    let server = MockServer::start().await;
    let meter = MeterId::Gas {
        mprn: "0000000000".into(),
        serial: "G4A00000000000".into(),
    };
    let err = client(&server, false)
        .await
        .consumption(
            &meter,
            Period::since(ts("2026-11-10T00:00:00Z")),
            Some(Grouping::Day),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::MissingApiKey), "{err:?}");
}

#[tokio::test]
async fn gas_consumption_with_grouping() {
    let server = MockServer::start().await;
    Mock::given(path(
        "/v1/gas-meter-points/0000000000/meters/G4A00000000000/consumption/",
    ))
    .and(query_param("group_by", "day"))
    .respond_with(json("consumption_gas.json"))
    .mount(&server)
    .await;
    let meter = MeterId::Gas {
        mprn: "0000000000".into(),
        serial: "G4A00000000000".into(),
    };
    let rows = client(&server, true)
        .await
        .consumption(
            &meter,
            Period::new(ts("2026-11-03T00:00:00Z"), ts("2026-11-10T00:00:00Z")),
            Some(Grouping::Day),
        )
        .await
        .unwrap();
    // One row per day.
    assert_eq!(rows.len(), 7);
    assert_eq!(rows[0].interval_start, ts("2026-11-03T00:00:00Z"));
    assert_eq!(rows[6].interval_end, ts("2026-11-10T00:00:00Z"));
}

#[tokio::test]
async fn account_agreements_and_lookups() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/accounts/A-00000000/"))
        .and(header("authorization", BASIC))
        .respond_with(json("account.json"))
        .mount(&server)
        .await;
    Mock::given(path("/v1/electricity-meter-points/0000000000000/"))
        .respond_with(json("meter_point.json"))
        .mount(&server)
        .await;
    Mock::given(path("/v1/industry/grid-supply-points/"))
        .and(query_param("postcode", "AA11AA"))
        .respond_with(json("grid_supply_points.json"))
        .mount(&server)
        .await;
    let c = client(&server, true).await;
    let acc = c.account("A-00000000").await.unwrap();
    insta::assert_json_snapshot!(acc);
    let emp = &acc.properties[0].electricity_meter_points[0];
    let now = active_agreement(&emp.agreements, ts("2026-11-12T00:00:00Z")).unwrap();
    assert!(now.tariff_code.is_agile());
    let then = active_agreement(&emp.agreements, ts("2024-01-01T00:00:00Z")).unwrap();
    assert!(!then.tariff_code.is_agile());

    assert_eq!(
        c.meter_point("0000000000000").await.unwrap().gsp.letter(),
        'K'
    );
    assert_eq!(
        c.region_for_postcode("AA1 1AA")
            .await
            .unwrap()
            .map(|r| r.letter()),
        Some('K')
    );
}

#[tokio::test]
async fn error_mapping() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/products/RATE/"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "120"))
        .mount(&server)
        .await;
    Mock::given(path("/v1/products/WAF/"))
        .respond_with(
            ResponseTemplate::new(403).set_body_raw("<html>Access denied</html>", "text/html"),
        )
        .mount(&server)
        .await;
    Mock::given(path("/v1/products/AUTH/"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(serde_json::json!({"detail": "Invalid API key"})),
        )
        .mount(&server)
        .await;
    Mock::given(path("/v1/products/GONE/"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(serde_json::json!({"detail": "Not found."})),
        )
        .mount(&server)
        .await;
    let c = client(&server, false).await;
    let e = |code: &'static str| {
        let c = c.clone();
        async move { c.product(code, None).await.unwrap_err() }
    };
    match e("RATE").await {
        Error::RateLimited { retry_after } => assert_eq!(retry_after.unwrap().as_secs(), 120),
        other => panic!("{other:?}"),
    }
    assert!(matches!(e("WAF").await, Error::Throttled));
    assert!(matches!(e("AUTH").await, Error::Unauthorised(_)));
    assert!(matches!(e("GONE").await, Error::Status { status: 404, .. }));
}

#[test]
fn debug_output_never_contains_the_key() {
    let c = Client::builder()
        .api_key(SecretString::from("sk_live_SHOULD_NOT_APPEAR"))
        .build()
        .unwrap();
    assert!(!format!("{c:?}").contains("SHOULD_NOT_APPEAR"));
}
