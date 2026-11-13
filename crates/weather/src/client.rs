//! HTTP client for the forecast, archive and historical-forecast APIs.

use std::time::Duration;

use jiff::civil::Date;
use jiff::{SignedDuration, Timestamp};
use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::{RequestBuilder, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use url::Url;

use crate::error::{Error, Result};
use crate::model::{ForecastPoint, HOURLY_VARIABLES, HourlyResponse, Observation};

/// Free forecast API host.
pub const FORECAST_BASE_URL: &str = "https://api.open-meteo.com";
/// Free historical-weather (archive) API host.
pub const ARCHIVE_BASE_URL: &str = "https://archive-api.open-meteo.com";
/// Free historical-forecast API host.
pub const HISTORICAL_FORECAST_BASE_URL: &str = "https://historical-forecast-api.open-meteo.com";
/// Commercial forecast host (used by default when an API key is set).
pub const CUSTOMER_FORECAST_BASE_URL: &str = "https://customer-api.open-meteo.com";
/// Commercial archive host.
pub const CUSTOMER_ARCHIVE_BASE_URL: &str = "https://customer-archive-api.open-meteo.com";
/// Commercial historical-forecast host.
pub const CUSTOMER_HISTORICAL_FORECAST_BASE_URL: &str =
    "https://customer-historical-forecast-api.open-meteo.com";

/// Longest forecast Open-Meteo serves, in days.
pub const MAX_FORECAST_DAYS: u8 = 16;

/// A point on the globe (WGS84 degrees).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Location {
    latitude: f64,
    longitude: f64,
}

impl Location {
    /// Validated location: latitude in [-90, 90], longitude in [-180, 180].
    pub fn new(latitude: f64, longitude: f64) -> Result<Self> {
        if !(-90.0..=90.0).contains(&latitude) || !(-180.0..=180.0).contains(&longitude) {
            return Err(Error::InvalidRequest(format!(
                "coordinates out of range: {latitude}, {longitude}"
            )));
        }
        Ok(Self {
            latitude,
            longitude,
        })
    }

    /// Latitude, degrees north.
    pub fn latitude(&self) -> f64 {
        self.latitude
    }

    /// Longitude, degrees east.
    pub fn longitude(&self) -> f64 {
        self.longitude
    }
}

/// What to record as a forecast's `issued_at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssuedAt {
    /// The instant just before the request was sent.
    FetchTime,
    /// A caller-supplied instant (e.g. a scheduler tick, or a fixed time in
    /// tests).
    At(Timestamp),
}

/// Options for [`Client::forecast`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForecastOptions {
    /// Days to forecast, 1–16, starting at 00:00 UTC today. Open-Meteo's
    /// default is 7.
    pub forecast_days: u8,
    /// Weather model(s), e.g. `ukmo_seamless`; `None` lets Open-Meteo pick
    /// (`best_match`).
    pub models: Option<String>,
}

impl ForecastOptions {
    /// `days` of forecast with the default model.
    pub fn days(days: u8) -> Self {
        Self {
            forecast_days: days,
            models: None,
        }
    }
}

impl Default for ForecastOptions {
    fn default() -> Self {
        Self::days(7)
    }
}

/// Builder for [`Client`].
#[derive(Debug, Default)]
pub struct ClientBuilder {
    forecast_base: Option<String>,
    archive_base: Option<String>,
    historical_forecast_base: Option<String>,
    api_key: Option<SecretString>,
    http: Option<reqwest::Client>,
    timeout: Option<Duration>,
    user_agent: Option<String>,
}

impl ClientBuilder {
    /// Forecast API host (default [`FORECAST_BASE_URL`]).
    pub fn forecast_base_url(mut self, url: impl Into<String>) -> Self {
        self.forecast_base = Some(url.into());
        self
    }

    /// Archive API host (default [`ARCHIVE_BASE_URL`]).
    pub fn archive_base_url(mut self, url: impl Into<String>) -> Self {
        self.archive_base = Some(url.into());
        self
    }

    /// Historical-forecast API host (default [`HISTORICAL_FORECAST_BASE_URL`]).
    pub fn historical_forecast_base_url(mut self, url: impl Into<String>) -> Self {
        self.historical_forecast_base = Some(url.into());
        self
    }

    /// Commercial API key, sent as the `apikey` query parameter. Hosts not
    /// set explicitly switch to the `customer-*` hosts.
    pub fn api_key(mut self, key: SecretString) -> Self {
        self.api_key = Some(key);
        self
    }

    /// Use a pre-configured `reqwest::Client` (ignores timeout/user agent).
    pub fn http_client(mut self, http: reqwest::Client) -> Self {
        self.http = Some(http);
        self
    }

    /// Per-request timeout (default 30 s).
    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = Some(t);
        self
    }

    /// User-Agent header (default `weather-rs/<version>`).
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = Some(ua.into());
        self
    }

    /// Build the client.
    pub fn build(self) -> Result<Client> {
        let paid = self.api_key.is_some();
        let pick = |given: Option<String>, free: &str, customer: &str, path: &str| {
            let mut u = Url::parse(
                given
                    .as_deref()
                    .unwrap_or(if paid { customer } else { free }),
            )?;
            u.set_path(path);
            Ok::<_, Error>(u)
        };
        let forecast = pick(
            self.forecast_base,
            FORECAST_BASE_URL,
            CUSTOMER_FORECAST_BASE_URL,
            "/v1/forecast",
        )?;
        let archive = pick(
            self.archive_base,
            ARCHIVE_BASE_URL,
            CUSTOMER_ARCHIVE_BASE_URL,
            "/v1/archive",
        )?;
        let historical_forecast = pick(
            self.historical_forecast_base,
            HISTORICAL_FORECAST_BASE_URL,
            CUSTOMER_HISTORICAL_FORECAST_BASE_URL,
            "/v1/forecast",
        )?;
        let http = match self.http {
            Some(h) => h,
            None => reqwest::Client::builder()
                .timeout(self.timeout.unwrap_or(Duration::from_secs(30)))
                .user_agent(self.user_agent.unwrap_or_else(|| {
                    concat!("weather-rs/", env!("CARGO_PKG_VERSION")).to_owned()
                }))
                .build()?,
        };
        Ok(Client {
            http,
            forecast,
            archive,
            historical_forecast,
            api_key: self.api_key,
        })
    }
}

/// Open-Meteo client. Cheap to clone.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    forecast: Url,
    archive: Url,
    historical_forecast: Url,
    api_key: Option<SecretString>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("forecast", &self.forecast.as_str())
            .field("archive", &self.archive.as_str())
            .field("historical_forecast", &self.historical_forecast.as_str())
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl Client {
    /// Builder.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// Client for the free, non-commercial API.
    pub fn new() -> Result<Self> {
        Self::builder().build()
    }

    /// Hourly forecast rows, oldest first, tagged with `issued_at`.
    ///
    /// Rows start at 00:00 UTC today, so the first few hours are already in
    /// the past.
    pub async fn forecast(
        &self,
        location: Location,
        options: &ForecastOptions,
        issued_at: IssuedAt,
    ) -> Result<Vec<ForecastPoint>> {
        let fetched_at = Timestamp::now();
        let resp = self.forecast_response(location, options).await?;
        let issued_at = match issued_at {
            IssuedAt::FetchTime => fetched_at,
            IssuedAt::At(t) => t,
        };
        resp.forecast_points(issued_at)
    }

    /// The unconverted forecast response (grid cell, elevation, units,
    /// columns). Use [`HourlyResponse::forecast_points`] to convert.
    pub async fn forecast_response(
        &self,
        location: Location,
        options: &ForecastOptions,
    ) -> Result<HourlyResponse> {
        if !(1..=MAX_FORECAST_DAYS).contains(&options.forecast_days) {
            return Err(Error::InvalidRequest(format!(
                "forecast_days must be 1–{MAX_FORECAST_DAYS}, got {}",
                options.forecast_days
            )));
        }
        let mut q = self.common_query(location);
        q.push(("forecast_days", options.forecast_days.to_string()));
        if let Some(m) = &options.models {
            q.push(("models", m.clone()));
        }
        self.get(&self.forecast, &q).await
    }

    /// Historical weather (reanalysis) for the UTC dates `start..=end`, as
    /// observation rows. The archive lags real time by several days; recent
    /// days come back as `null` values.
    pub async fn archive(
        &self,
        location: Location,
        start: Date,
        end: Date,
    ) -> Result<Vec<Observation>> {
        let q = self.dated_query(location, start, end)?;
        self.get(&self.archive, &q).await?.observations()
    }

    /// Archived short-range forecasts for the UTC dates `start..=end`,
    /// stitched into one continuous series by Open-Meteo. Returned as
    /// observation rows: the series carries no issue time, so it cannot
    /// evaluate forecast skill at a given lead time.
    pub async fn historical_forecast(
        &self,
        location: Location,
        start: Date,
        end: Date,
    ) -> Result<Vec<Observation>> {
        let q = self.dated_query(location, start, end)?;
        self.get(&self.historical_forecast, &q)
            .await?
            .observations()
    }

    // --- plumbing -------------------------------------------------------

    fn common_query(&self, location: Location) -> Vec<(&'static str, String)> {
        let mut q = vec![
            ("latitude", location.latitude.to_string()),
            ("longitude", location.longitude.to_string()),
            ("hourly", HOURLY_VARIABLES.join(",")),
            ("timeformat", "unixtime".to_owned()),
            ("timezone", "GMT".to_owned()),
            ("temperature_unit", "celsius".to_owned()),
            ("wind_speed_unit", "ms".to_owned()),
            ("precipitation_unit", "mm".to_owned()),
        ];
        if let Some(k) = &self.api_key {
            q.push(("apikey", k.expose_secret().to_owned()));
        }
        q
    }

    fn dated_query(
        &self,
        location: Location,
        start: Date,
        end: Date,
    ) -> Result<Vec<(&'static str, String)>> {
        if end < start {
            return Err(Error::InvalidRequest(format!(
                "end date {end} is before start date {start}"
            )));
        }
        let mut q = self.common_query(location);
        q.push(("start_date", start.to_string()));
        q.push(("end_date", end.to_string()));
        Ok(q)
    }

    async fn get(&self, url: &Url, q: &[(&str, String)]) -> Result<HourlyResponse> {
        tracing::debug!(%url, "open-meteo request");
        send(self.http.get(url.clone()).query(q)).await
    }
}

/// Open-Meteo's error body.
#[derive(Debug, Deserialize)]
struct ApiError {
    error: bool,
    #[serde(default)]
    reason: Option<String>,
}

async fn send(req: RequestBuilder) -> Result<HourlyResponse> {
    let resp = req.send().await?;
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.bytes().await?;
    let api_error = serde_json::from_slice::<ApiError>(&bytes)
        .ok()
        .filter(|e| e.error);

    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(Error::RateLimited {
            retry_after: retry_after(&headers),
            reason: api_error.and_then(|e| e.reason),
        });
    }
    if let Some(e) = api_error {
        return Err(Error::Api {
            status: status.as_u16(),
            reason: e.reason.unwrap_or_default(),
        });
    }
    if !status.is_success() {
        let mut body = String::from_utf8_lossy(&bytes).into_owned();
        if body.len() > 512 {
            let mut cut = 512;
            while !body.is_char_boundary(cut) {
                cut -= 1;
            }
            body.truncate(cut);
        }
        return Err(Error::Status {
            status: status.as_u16(),
            body,
        });
    }
    serde_json::from_slice(&bytes).map_err(|e| Error::Decode(e.to_string()))
}

fn retry_after(h: &HeaderMap) -> Option<SignedDuration> {
    let v = h.get(RETRY_AFTER)?.to_str().ok()?;
    v.trim().parse::<i64>().ok().map(SignedDuration::from_secs)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn location_is_validated() {
        assert!(Location::new(50.7275, -3.4747).is_ok());
        assert!(Location::new(90.5, 0.0).is_err());
        assert!(Location::new(0.0, -180.1).is_err());
        assert!(Location::new(f64::NAN, 0.0).is_err());
    }

    #[test]
    fn api_key_switches_unset_hosts_and_is_redacted() {
        let c = Client::builder()
            .api_key(SecretString::from("not-a-real-key"))
            .archive_base_url("http://localhost:1")
            .build()
            .unwrap();
        assert_eq!(
            c.forecast.as_str(),
            "https://customer-api.open-meteo.com/v1/forecast"
        );
        assert_eq!(c.archive.as_str(), "http://localhost:1/v1/archive");
        assert!(!format!("{c:?}").contains("not-a-real-key"));

        let free = Client::new().unwrap();
        assert_eq!(
            free.historical_forecast.as_str(),
            "https://historical-forecast-api.open-meteo.com/v1/forecast"
        );
    }
}
