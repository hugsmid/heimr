//! Kraken GraphQL client: authentication, smart-meter discovery and live
//! smart-meter telemetry (`smartMeterTelemetry`), available when a consumer
//! access device (CAD) is paired with the meter.
//!
//! Notes on the API's behaviour (from the public docs and maintained
//! open-source clients):
//! - Tokens come from `obtainKrakenToken(input: {APIKey})`, last ~60 minutes,
//!   and are sent as a bare `Authorization: <token>` header (no `Bearer`).
//! - Errors arrive as HTTP 200 with `errors[].extensions.errorCode`.
//! - `smartMeterTelemetry` has a per-account limit (~100–125 requests/hour,
//!   shared with the Octopus app). Poll no faster than once a minute.

use std::sync::Mutex;

use jiff::{SignedDuration, Timestamp};
use reqwest::header::AUTHORIZATION;
use secrecy::{ExposeSecret, SecretString};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use crate::decimal::opt_f64;
use crate::error::{Error, Result};
use crate::rest::{DEFAULT_BASE_URL, decode, send};

/// Refresh the token this long before it expires.
const REFRESH_MARGIN: SignedDuration = SignedDuration::from_mins(5);

/// Telemetry resolution. With no grouping (see [`KrakenClient::latest_telemetry`])
/// the API returns the single latest reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TelemetryGrouping {
    /// 10 seconds (the finest resolution the API offers).
    TenSeconds,
    /// 1 minute.
    OneMinute,
    /// 5 minutes.
    FiveMinutes,
    /// 30 minutes.
    HalfHourly,
    /// 1 hour.
    Hourly,
}

/// One telemetry point. Energy in **Wh** and power in **W**, as returned;
/// fields are optional because meters differ in what they report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Telemetry {
    /// Reading time.
    pub read_at: Timestamp,
    /// Cumulative import register, Wh.
    #[serde(default, deserialize_with = "opt_f64", rename = "consumption")]
    pub import_wh: Option<f64>,
    /// Import during this interval, Wh.
    #[serde(default, deserialize_with = "opt_f64", rename = "consumptionDelta")]
    pub import_delta_wh: Option<f64>,
    /// Instantaneous demand, W (negative when exporting).
    #[serde(default, deserialize_with = "opt_f64", rename = "demand")]
    pub demand_w: Option<f64>,
    /// Cumulative export register, Wh (often absent).
    #[serde(default, deserialize_with = "opt_f64", rename = "export")]
    pub export_wh: Option<f64>,
    /// Cost of the interval, exc. tax (unit as reported; believed pence).
    #[serde(default, deserialize_with = "opt_f64")]
    pub cost_delta: Option<f64>,
    /// Cost of the interval inc. tax.
    #[serde(default, deserialize_with = "opt_f64")]
    pub cost_delta_with_tax: Option<f64>,
}

impl Telemetry {
    /// Cumulative import in kWh.
    pub fn import_kwh(&self) -> Option<f64> {
        self.import_wh.map(|wh| wh / 1000.0)
    }
}

/// Smart meters on an account, with telemetry device ids where present.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MeterInventory {
    /// Active electricity meter points.
    pub electricity: Vec<ElectricityPoint>,
    /// Active gas meter points.
    pub gas: Vec<GasPoint>,
}

impl MeterInventory {
    /// Device id of the first smart import electricity meter — the id to
    /// use for live telemetry.
    pub fn electricity_device_id(&self) -> Option<&str> {
        self.electricity
            .iter()
            .flat_map(|p| &p.meters)
            .find_map(|m| m.import_device_id.as_deref())
    }
}

/// An electricity meter point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElectricityPoint {
    /// MPAN.
    pub mpan: String,
    /// `IMPORT` / `EXPORT`.
    pub direction: Option<String>,
    /// Active meters.
    pub meters: Vec<Meter>,
}

/// A gas meter point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GasPoint {
    /// MPRN.
    pub mprn: String,
    /// Active meters.
    pub meters: Vec<Meter>,
}

/// A meter as seen by Kraken.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meter {
    /// Serial number.
    pub serial_number: String,
    /// Smart import device id (EUI-64, `00-11-…`), electricity only.
    pub import_device_id: Option<String>,
    /// Smart gas meter device id.
    pub gas_device_id: Option<String>,
    /// Gas only: `"kWh"` (SMETS1) or `"m³"` (SMETS2). Tells you how to read
    /// REST gas consumption.
    pub consumption_units: Option<String>,
}

#[derive(Debug, Clone)]
struct Token {
    jwt: SecretString,
    expires_at: Timestamp,
}

/// Kraken GraphQL client. Holds a cached token; share it behind an `Arc`.
pub struct KrakenClient {
    http: reqwest::Client,
    endpoint: Url,
    api_key: SecretString,
    token: Mutex<Option<Token>>,
}

impl std::fmt::Debug for KrakenClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KrakenClient")
            .field("endpoint", &self.endpoint.as_str())
            .finish_non_exhaustive()
    }
}

impl KrakenClient {
    /// Client on the default host.
    pub fn new(api_key: SecretString) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent(concat!("octopus-energy-rs/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Self::with_base_url(api_key, DEFAULT_BASE_URL, http)
    }

    /// Client on a specific host, such as a test server.
    pub fn with_base_url(
        api_key: SecretString,
        base_url: &str,
        http: reqwest::Client,
    ) -> Result<Self> {
        let mut endpoint = Url::parse(base_url)?;
        endpoint.set_path("/v1/graphql/");
        Ok(Self {
            http,
            endpoint,
            api_key,
            token: Mutex::new(None),
        })
    }

    /// Account numbers visible to this API key.
    pub async fn account_numbers(&self) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct D {
            viewer: Viewer,
        }
        #[derive(Deserialize)]
        struct Viewer {
            accounts: Vec<Acc>,
        }
        #[derive(Deserialize)]
        struct Acc {
            number: String,
        }
        let d: D = self
            .query("query { viewer { accounts { number } } }", json!({}))
            .await?;
        Ok(d.viewer.accounts.into_iter().map(|a| a.number).collect())
    }

    /// Active smart meters and their device ids.
    pub async fn meters(&self, account_number: &str) -> Result<MeterInventory> {
        let d: wire::AccountData = self
            .query(wire::METERS_QUERY, json!({ "account": account_number }))
            .await?;
        Ok(d.into())
    }

    /// Telemetry between `start` and `end` at `grouping` resolution. Keep
    /// windows short (hours, not weeks): long ranges are unreliable.
    pub async fn telemetry(
        &self,
        device_id: &str,
        start: Timestamp,
        end: Timestamp,
        grouping: TelemetryGrouping,
    ) -> Result<Vec<Telemetry>> {
        let d: wire::TelemetryData = self
            .query(
                wire::TELEMETRY_RANGE_QUERY,
                json!({
                    "deviceId": device_id,
                    "start": start.to_string(),
                    "end": end.to_string(),
                    "grouping": grouping,
                }),
            )
            .await?;
        Ok(d.smart_meter_telemetry.unwrap_or_default())
    }

    /// The latest single reading (live power).
    pub async fn latest_telemetry(&self, device_id: &str) -> Result<Option<Telemetry>> {
        let d: wire::TelemetryData = self
            .query(
                wire::TELEMETRY_LATEST_QUERY,
                json!({ "deviceId": device_id }),
            )
            .await?;
        Ok(d.smart_meter_telemetry
            .unwrap_or_default()
            .into_iter()
            .max_by_key(|t| t.read_at))
    }

    /// Run an arbitrary authenticated query (escape hatch).
    pub async fn query<T: DeserializeOwned>(&self, query: &str, variables: Value) -> Result<T> {
        match self.query_once(query, &variables).await {
            Err(Error::GraphQl {
                code: Some(code), ..
            }) if code == "KT-CT-1112" || code == "KT-CT-1124" => {
                // Token rejected: drop it and retry once with a fresh one.
                self.clear_token();
                self.query_once(query, &variables).await
            }
            r => r,
        }
    }

    async fn query_once<T: DeserializeOwned>(&self, query: &str, variables: &Value) -> Result<T> {
        let token = self.token().await?;
        let req = self
            .http
            .post(self.endpoint.clone())
            .header(AUTHORIZATION, token.expose_secret())
            .json(&json!({ "query": query, "variables": variables }));
        let body: wire::Response<T> = decode(send(req).await?).await?;
        body.into_result()
    }

    fn clear_token(&self) {
        if let Ok(mut t) = self.token.lock() {
            *t = None;
        }
    }

    async fn token(&self) -> Result<SecretString> {
        if let Some(t) = self.token.lock().ok().and_then(|g| g.clone())
            && Timestamp::now() + REFRESH_MARGIN < t.expires_at
        {
            return Ok(t.jwt);
        }
        let req = self.http.post(self.endpoint.clone()).json(&json!({
            "query": wire::TOKEN_MUTATION,
            "variables": { "input": { "APIKey": self.api_key.expose_secret() } },
        }));
        let body: wire::Response<wire::TokenData> = decode(send(req).await?).await?;
        let t = body.into_result()?.obtain_kraken_token;
        let expires_at = t
            .payload
            .as_ref()
            .and_then(|p| p.get("exp"))
            .and_then(Value::as_i64)
            .and_then(|s| Timestamp::from_second(s).ok())
            // No exp claim: assume the documented 60 minutes.
            .unwrap_or_else(|| Timestamp::now() + SignedDuration::from_mins(60));
        let token = Token {
            jwt: SecretString::from(t.token),
            expires_at,
        };
        if let Ok(mut g) = self.token.lock() {
            *g = Some(token.clone());
        }
        Ok(token.jwt)
    }
}

/// Wire types and query documents.
mod wire {
    use super::*;

    pub(super) const TOKEN_MUTATION: &str = "mutation ObtainKrakenToken($input: ObtainJSONWebTokenInput!) { obtainKrakenToken(input: $input) { token refreshExpiresIn payload } }";

    pub(super) const METERS_QUERY: &str = r#"query Meters($account: String!) {
  account(accountNumber: $account) {
    electricityAgreements(active: true) {
      meterPoint {
        mpan
        direction
        meters(includeInactive: false) {
          serialNumber
          smartImportElectricityMeter { deviceId }
        }
      }
    }
    gasAgreements(active: true) {
      meterPoint {
        mprn
        meters(includeInactive: false) {
          serialNumber
          consumptionUnits
          smartGasMeter { deviceId }
        }
      }
    }
  }
}"#;

    pub(super) const TELEMETRY_RANGE_QUERY: &str = r#"query Telemetry($deviceId: String!, $start: DateTime, $end: DateTime, $grouping: TelemetryGrouping) {
  smartMeterTelemetry(deviceId: $deviceId, start: $start, end: $end, grouping: $grouping) {
    readAt consumption consumptionDelta demand export costDelta costDeltaWithTax
  }
}"#;

    pub(super) const TELEMETRY_LATEST_QUERY: &str = r#"query Telemetry($deviceId: String!) {
  smartMeterTelemetry(deviceId: $deviceId) {
    readAt consumption consumptionDelta demand export costDelta costDeltaWithTax
  }
}"#;

    #[derive(Deserialize)]
    pub(super) struct Response<T> {
        data: Option<T>,
        #[serde(default)]
        errors: Vec<GqlError>,
    }

    #[derive(Deserialize)]
    struct GqlError {
        message: String,
        extensions: Option<Extensions>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Extensions {
        error_code: Option<String>,
    }

    impl<T> Response<T> {
        pub(super) fn into_result(self) -> Result<T> {
            if let Some(e) = self.errors.into_iter().next() {
                let code = e.extensions.and_then(|x| x.error_code);
                return Err(match code.as_deref() {
                    Some("KT-CT-1199") => Error::RateLimited { retry_after: None },
                    Some("KT-CT-1111" | "KT-CT-4177" | "KT-CT-11334") => {
                        Error::Unauthorised(format!("{}: {}", code.unwrap_or_default(), e.message))
                    }
                    _ => Error::GraphQl {
                        code,
                        message: e.message,
                    },
                });
            }
            self.data
                .ok_or_else(|| Error::Decode("GraphQL response had neither data nor errors".into()))
        }
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct TokenData {
        pub obtain_kraken_token: TokenPayload,
    }

    #[derive(Deserialize)]
    pub(super) struct TokenPayload {
        pub token: String,
        pub payload: Option<Value>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct TelemetryData {
        pub smart_meter_telemetry: Option<Vec<Telemetry>>,
    }

    #[derive(Deserialize)]
    pub(super) struct AccountData {
        account: Account,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Account {
        #[serde(default)]
        electricity_agreements: Vec<Agreement<EPoint>>,
        #[serde(default)]
        gas_agreements: Vec<Agreement<GPoint>>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Agreement<P> {
        meter_point: P,
    }

    #[derive(Deserialize)]
    struct EPoint {
        mpan: String,
        direction: Option<String>,
        #[serde(default)]
        meters: Vec<WMeter>,
    }

    #[derive(Deserialize)]
    struct GPoint {
        mprn: String,
        #[serde(default)]
        meters: Vec<WMeter>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct WMeter {
        serial_number: String,
        consumption_units: Option<String>,
        smart_import_electricity_meter: Option<Device>,
        smart_gas_meter: Option<Device>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Device {
        device_id: Option<String>,
    }

    impl From<WMeter> for Meter {
        fn from(m: WMeter) -> Self {
            Meter {
                serial_number: m.serial_number,
                import_device_id: m.smart_import_electricity_meter.and_then(|d| d.device_id),
                gas_device_id: m.smart_gas_meter.and_then(|d| d.device_id),
                consumption_units: m.consumption_units,
            }
        }
    }

    impl From<AccountData> for MeterInventory {
        fn from(d: AccountData) -> Self {
            MeterInventory {
                electricity: d
                    .account
                    .electricity_agreements
                    .into_iter()
                    .map(|a| ElectricityPoint {
                        mpan: a.meter_point.mpan,
                        direction: a.meter_point.direction,
                        meters: a.meter_point.meters.into_iter().map(Into::into).collect(),
                    })
                    .collect(),
                gas: d
                    .account
                    .gas_agreements
                    .into_iter()
                    .map(|a| GasPoint {
                        mprn: a.meter_point.mprn,
                        meters: a.meter_point.meters.into_iter().map(Into::into).collect(),
                    })
                    .collect(),
            }
        }
    }
}
