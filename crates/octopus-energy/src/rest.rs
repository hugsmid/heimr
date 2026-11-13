//! REST client (`/v1/…`).

use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use reqwest::header::{CONTENT_TYPE, HeaderMap, RETRY_AFTER};
use reqwest::{RequestBuilder, Response, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::de::DeserializeOwned;
use url::Url;

use crate::error::{Error, Result};
use crate::model::{
    Account, Consumption, GridSupplyPoint, MeterPointInfo, Page, Product, ProductDetail, Rate,
};
use crate::tariff::{Fuel, Region, TariffCode};

/// Default API host, as documented by Octopus Energy. Use
/// [`ClientBuilder::base_url`] to point the client elsewhere (a test server, or
/// a different host); requests carry your API key, so configure only hosts you
/// trust.
pub const DEFAULT_BASE_URL: &str = "https://api.octopus.energy";

/// Maximum page size for rate endpoints.
const RATE_PAGE_SIZE: u32 = 1500;
/// Maximum page size for consumption endpoints.
const CONSUMPTION_PAGE_SIZE: u32 = 25_000;
/// Safety valve against pagination loops.
const MAX_PAGES: usize = 1000;

/// A time window `[from, to)`. `to = None` means "up to whatever is published".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Period {
    /// Start (inclusive).
    pub from: Timestamp,
    /// End (exclusive).
    pub to: Option<Timestamp>,
}

impl Period {
    /// `[from, to)`.
    pub fn new(from: Timestamp, to: Timestamp) -> Self {
        Self { from, to: Some(to) }
    }

    /// From `from` onwards.
    pub fn since(from: Timestamp) -> Self {
        Self { from, to: None }
    }
}

/// Server-side aggregation for consumption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grouping {
    /// Hourly.
    Hour,
    /// UK calendar days.
    Day,
    /// Weeks.
    Week,
    /// Calendar months.
    Month,
    /// Quarters.
    Quarter,
}

impl Grouping {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Quarter => "quarter",
        }
    }
}

/// Identifies a smart meter for consumption queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeterId {
    /// Electricity: MPAN + meter serial.
    Electricity {
        /// MPAN.
        mpan: String,
        /// Meter serial number.
        serial: String,
    },
    /// Gas: MPRN + meter serial.
    Gas {
        /// MPRN.
        mprn: String,
        /// Meter serial number.
        serial: String,
    },
}

/// Filters for `GET /products/`.
#[derive(Debug, Clone, Default)]
pub struct ProductFilter {
    /// Only variable products.
    pub is_variable: Option<bool>,
    /// Only green products.
    pub is_green: Option<bool>,
    /// Only tracker products.
    pub is_tracker: Option<bool>,
    /// Only business products.
    pub is_business: Option<bool>,
    /// Products available at this instant (server default: now).
    pub available_at: Option<Timestamp>,
    /// Brand, e.g. `OCTOPUS_ENERGY`.
    pub brand: Option<String>,
}

/// Builder for [`Client`].
#[derive(Debug, Default)]
pub struct ClientBuilder {
    base_url: Option<String>,
    api_key: Option<SecretString>,
    http: Option<reqwest::Client>,
    timeout: Option<Duration>,
    user_agent: Option<String>,
}

impl ClientBuilder {
    /// Override the API host (default [`DEFAULT_BASE_URL`]).
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// API key (from the Octopus dashboard). Needed for consumption and
    /// account endpoints; prices are public.
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

    /// User-Agent header (default `octopus-energy-rs/<version>`).
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = Some(ua.into());
        self
    }

    /// Build the client.
    pub fn build(self) -> Result<Client> {
        let mut base = Url::parse(self.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL))?;
        base.set_path("/v1/");
        let http = match self.http {
            Some(h) => h,
            None => reqwest::Client::builder()
                .timeout(self.timeout.unwrap_or(Duration::from_secs(30)))
                .user_agent(self.user_agent.unwrap_or_else(|| {
                    concat!("octopus-energy-rs/", env!("CARGO_PKG_VERSION")).to_owned()
                }))
                .build()?,
        };
        Ok(Client {
            http,
            base,
            api_key: self.api_key,
        })
    }
}

/// Octopus Energy REST client. Cheap to clone.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: Url,
    api_key: Option<SecretString>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("base", &self.base.as_str())
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl Client {
    /// Builder.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// Unauthenticated client on the default host (prices and products only).
    pub fn public() -> Result<Self> {
        Self::builder().build()
    }

    /// The base URL in use (always ends in `/v1/`).
    pub fn base_url(&self) -> &Url {
        &self.base
    }

    /// `GET /products/` (all pages).
    pub async fn products(&self, filter: &ProductFilter) -> Result<Vec<Product>> {
        let mut q: Vec<(&str, String)> = Vec::new();
        let flag = |b: bool| if b { "true" } else { "false" }.to_owned();
        if let Some(v) = filter.is_variable {
            q.push(("is_variable", flag(v)));
        }
        if let Some(v) = filter.is_green {
            q.push(("is_green", flag(v)));
        }
        if let Some(v) = filter.is_tracker {
            q.push(("is_tracker", flag(v)));
        }
        if let Some(v) = filter.is_business {
            q.push(("is_business", flag(v)));
        }
        if let Some(t) = filter.available_at {
            q.push(("available_at", t.to_string()));
        }
        if let Some(b) = &filter.brand {
            q.push(("brand", b.clone()));
        }
        self.get_all("products/", &q, false).await
    }

    /// `GET /products/{code}/`.
    pub async fn product(
        &self,
        code: &str,
        tariffs_active_at: Option<Timestamp>,
    ) -> Result<ProductDetail> {
        let mut q = Vec::new();
        if let Some(t) = tariffs_active_at {
            q.push(("tariffs_active_at", t.to_string()));
        }
        self.get_one(&format!("products/{}/", seg(code)), &q, false)
            .await
    }

    /// Standard unit rates (p/kWh) for an electricity or gas tariff, oldest
    /// first. For Agile this is one row per half-hour.
    pub async fn unit_rates(&self, tariff: &TariffCode, period: Period) -> Result<Vec<Rate>> {
        self.rates(tariff, "standard-unit-rates", period).await
    }

    /// Day unit rates (dual-register electricity tariffs).
    pub async fn day_unit_rates(&self, tariff: &TariffCode, period: Period) -> Result<Vec<Rate>> {
        self.rates(tariff, "day-unit-rates", period).await
    }

    /// Night unit rates (dual-register electricity tariffs).
    pub async fn night_unit_rates(&self, tariff: &TariffCode, period: Period) -> Result<Vec<Rate>> {
        self.rates(tariff, "night-unit-rates", period).await
    }

    /// Standing charges (p/day), oldest first.
    pub async fn standing_charges(&self, tariff: &TariffCode, period: Period) -> Result<Vec<Rate>> {
        self.rates(tariff, "standing-charges", period).await
    }

    async fn rates(&self, tariff: &TariffCode, kind: &str, period: Period) -> Result<Vec<Rate>> {
        let fuel = match tariff.fuel() {
            Fuel::Electricity => "electricity-tariffs",
            Fuel::Gas => "gas-tariffs",
        };
        let path = format!(
            "products/{}/{fuel}/{}/{kind}/",
            seg(tariff.product()),
            seg(&tariff.to_string())
        );
        let mut q = vec![
            ("period_from", period.from.to_string()),
            ("page_size", RATE_PAGE_SIZE.to_string()),
        ];
        if let Some(to) = period.to {
            q.push(("period_to", to.to_string()));
        }
        // Rate endpoints are newest-first and ignore order_by; sort here.
        let mut rates: Vec<Rate> = self.get_all(&path, &q, false).await?;
        rates.sort_by_key(|r| r.valid_from);
        Ok(rates)
    }

    /// Smart-meter consumption, oldest first. Half-hourly unless `grouping`
    /// is given. Needs an API key. Usually lags by a day or more.
    pub async fn consumption(
        &self,
        meter: &MeterId,
        period: Period,
        grouping: Option<Grouping>,
    ) -> Result<Vec<Consumption>> {
        let path = match meter {
            MeterId::Electricity { mpan, serial } => {
                format!(
                    "electricity-meter-points/{}/meters/{}/consumption/",
                    seg(mpan),
                    seg(serial)
                )
            }
            MeterId::Gas { mprn, serial } => {
                format!(
                    "gas-meter-points/{}/meters/{}/consumption/",
                    seg(mprn),
                    seg(serial)
                )
            }
        };
        let mut q = vec![
            ("period_from", period.from.to_string()),
            ("page_size", CONSUMPTION_PAGE_SIZE.to_string()),
            ("order_by", "period".to_owned()),
        ];
        if let Some(to) = period.to {
            q.push(("period_to", to.to_string()));
        }
        if let Some(g) = grouping {
            q.push(("group_by", g.as_str().to_owned()));
        }
        let mut rows: Vec<Consumption> = self.get_all(&path, &q, true).await?;
        rows.sort_by_key(|c| c.interval_start);
        rows.dedup_by_key(|c| c.interval_start);
        Ok(rows)
    }

    /// `GET /accounts/{number}/`. Needs an API key.
    pub async fn account(&self, number: &str) -> Result<Account> {
        self.get_one(&format!("accounts/{}/", seg(number)), &[], true)
            .await
    }

    /// `GET /electricity-meter-points/{mpan}/` (public): the MPAN's region.
    pub async fn meter_point(&self, mpan: &str) -> Result<MeterPointInfo> {
        self.get_one(
            &format!("electricity-meter-points/{}/", seg(mpan)),
            &[],
            false,
        )
        .await
    }

    /// Region for a postcode, if it maps to exactly one GSP group.
    pub async fn region_for_postcode(&self, postcode: &str) -> Result<Option<Region>> {
        let q = [("postcode", postcode.replace(' ', ""))];
        let gsps: Vec<GridSupplyPoint> = self
            .get_all("industry/grid-supply-points/", &q, false)
            .await?;
        Ok(match gsps.as_slice() {
            [one] => Some(one.group_id),
            _ => None,
        })
    }

    // --- plumbing -------------------------------------------------------

    fn url(&self, path: &str) -> Result<Url> {
        Ok(self.base.join(path)?)
    }

    fn authed(&self, req: RequestBuilder, required: bool) -> Result<RequestBuilder> {
        match (&self.api_key, required) {
            (Some(k), _) => Ok(req.basic_auth(k.expose_secret(), Some(""))),
            (None, true) => Err(Error::MissingApiKey),
            (None, false) => Ok(req),
        }
    }

    async fn get_one<T: DeserializeOwned>(
        &self,
        path: &str,
        q: &[(&str, String)],
        auth: bool,
    ) -> Result<T> {
        let req = self.authed(self.http.get(self.url(path)?).query(q), auth)?;
        decode(send(req).await?).await
    }

    async fn get_all<T: DeserializeOwned>(
        &self,
        path: &str,
        q: &[(&str, String)],
        auth: bool,
    ) -> Result<Vec<T>> {
        let mut out = Vec::new();
        let mut req = self.authed(self.http.get(self.url(path)?).query(q), auth)?;
        for _ in 0..MAX_PAGES {
            let page: Page<T> = decode(send(req).await?).await?;
            out.extend(page.results);
            match page.next {
                None => return Ok(out),
                Some(next) => {
                    let url = self.rebase(&next)?;
                    tracing::debug!(%url, "following pagination");
                    req = self.authed(self.http.get(url), auth)?;
                }
            }
        }
        Err(Error::Decode(format!("more than {MAX_PAGES} pages")))
    }

    /// Follow `next` links on the configured host only: keep the path/query
    /// but never send credentials to whatever host the link names (some
    /// hosts reportedly return links to a different one).
    fn rebase(&self, next: &str) -> Result<Url> {
        let mut u = Url::parse(next)?;
        if !u.path().starts_with("/v1/") {
            return Err(Error::Decode(format!("unexpected pagination link {next}")));
        }
        u.set_scheme(self.base.scheme())
            .map_err(|()| Error::Decode("bad scheme".into()))?;
        u.set_host(self.base.host_str())?;
        u.set_port(self.base.port())
            .map_err(|()| Error::Decode("bad port".into()))?;
        Ok(u)
    }
}

/// Percent-encode one path segment.
fn seg(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

pub(crate) async fn send(req: RequestBuilder) -> Result<Response> {
    let resp = req.send().await?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let headers = resp.headers().clone();
    let is_json = is_json(&headers);
    let mut body = resp.text().await.unwrap_or_default();
    body.truncate(512);
    Err(match status {
        StatusCode::TOO_MANY_REQUESTS => Error::RateLimited {
            retry_after: retry_after(&headers),
        },
        StatusCode::FORBIDDEN if !is_json => Error::Throttled,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Error::Unauthorised(body),
        s => Error::Status {
            status: s.as_u16(),
            body,
        },
    })
}

pub(crate) async fn decode<T: DeserializeOwned>(resp: Response) -> Result<T> {
    let bytes = resp.bytes().await?;
    serde_json::from_slice(&bytes).map_err(|e| Error::Decode(e.to_string()))
}

fn is_json(h: &HeaderMap) -> bool {
    h.get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("json"))
}

fn retry_after(h: &HeaderMap) -> Option<SignedDuration> {
    let v = h.get(RETRY_AFTER)?.to_str().ok()?;
    v.trim().parse::<i64>().ok().map(SignedDuration::from_secs)
}
