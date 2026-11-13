//! An example heimr adapter: polls a sensor's HTTP JSON endpoint and emits
//! temperature and humidity readings.
//!
//! Copy this crate to start a new adapter. Rename the crate, the type and
//! the kind, replace the device protocol, and keep the shape: validate in
//! `new` without I/O, do everything else in `run`, take all time from the
//! context clock, report health, and return `Ok(())` promptly on shutdown.
//! Safety, auditing, restarts, logging context and stats come from the
//! core.
//!
//! ```toml
//! [adapters.sensor]
//! type = "example-sensor"
//! url = "http://sensor.local/status"
//! interval = "30s"
//! ```
//!
//! The endpoint answers `{"temperature": 20.5, "humidity": 41.0}`.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::time::Duration;

use heimr_core::{
    Adapter, AdapterContext, AdapterDescriptor, AdapterError, Capabilities, DeviceId, Metric, Unit,
};
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;

/// Instance settings (the adapter's own keys). Unknown keys are refused by
/// the core. Never add a time-zone or currency key: use `ctx.site()`.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// The sensor's status URL.
    pub url: String,
    /// Time between polls (5 s ..= 1 h).
    #[serde(default = "default_interval")]
    pub interval: SignedDuration,
    /// Device id for the readings.
    #[serde(default = "default_device")]
    pub device: DeviceId,
}

fn default_interval() -> SignedDuration {
    SignedDuration::from_secs(30)
}

fn default_device() -> DeviceId {
    DeviceId::new("sensor-1").expect("a valid id")
}

/// What the endpoint returns.
#[derive(Debug, Deserialize)]
struct Status {
    temperature: f64,
    humidity: f64,
}

/// The adapter.
#[derive(Debug)]
pub struct ExampleSensor {
    url: reqwest::Url,
}

/// Why one poll failed.
enum Poll {
    /// Worth retrying at the next interval.
    Retry(String),
    /// The sensor refused us; a restart cannot help.
    Refused(String),
}

impl Adapter for ExampleSensor {
    type Config = Config;

    fn descriptor() -> AdapterDescriptor {
        AdapterDescriptor::new(
            "example-sensor",
            "temperature and humidity from a sensor's HTTP JSON endpoint",
            Capabilities::read_only(),
        )
    }

    fn new(config: &Config) -> Result<Self, AdapterError> {
        let url = reqwest::Url::parse(&config.url)
            .map_err(|e| AdapterError::config_key("url", e.to_string()))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(AdapterError::config_key("url", "must be http or https"));
        }
        if config.interval < SignedDuration::from_secs(5)
            || config.interval > SignedDuration::from_hours(1)
        {
            return Err(AdapterError::config_key(
                "interval",
                "must be between 5s and 1h",
            ));
        }
        Ok(Self { url })
    }

    async fn run(self, ctx: AdapterContext<Config>) -> Result<(), AdapterError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(AdapterError::fatal)?;
        let interval = ctx.config().interval;
        let mut next = ctx.now();
        loop {
            tokio::select! {
                () = ctx.shutdown().wait() => return Ok(()),
                () = ctx.sleep_until(next) => {}
            }
            // A poll is short, but never let it hold up shutdown.
            let polled = tokio::select! {
                () = ctx.shutdown().wait() => return Ok(()),
                p = self.poll(&client) => p,
            };
            match polled {
                Ok(s) => {
                    let device = ctx.config().device.clone();
                    let t = ctx.reading(
                        device.clone(),
                        Metric::TEMPERATURE,
                        s.temperature,
                        Unit::Celsius,
                    );
                    let h = ctx.reading(device, Metric::HUMIDITY, s.humidity, Unit::Percent);
                    ctx.emit(t).await?;
                    ctx.emit(h).await?;
                    ctx.health().ok().await?;
                }
                Err(Poll::Retry(why)) => ctx.health().degraded(why).await?,
                Err(Poll::Refused(why)) => return Err(AdapterError::fatal(why)),
            }
            let now = ctx.now();
            next = next.saturating_add(interval).unwrap_or(Timestamp::MAX);
            if next <= now {
                next = now.saturating_add(interval).unwrap_or(Timestamp::MAX);
            }
        }
    }
}

impl ExampleSensor {
    async fn poll(&self, client: &reqwest::Client) -> Result<Status, Poll> {
        let resp = client
            .get(self.url.clone())
            .send()
            .await
            .map_err(|e| Poll::Retry(format!("request failed: {e}")))?;
        let code = resp.status();
        if code == reqwest::StatusCode::UNAUTHORIZED || code == reqwest::StatusCode::FORBIDDEN {
            return Err(Poll::Refused(format!("the sensor refused access ({code})")));
        }
        if !code.is_success() {
            return Err(Poll::Retry(format!("the sensor answered {code}")));
        }
        let body = resp
            .bytes()
            .await
            .map_err(|e| Poll::Retry(format!("reading the answer: {e}")))?;
        let s: Status = serde_json::from_slice(&body)
            .map_err(|e| Poll::Retry(format!("unexpected answer: {e}")))?;
        if !s.temperature.is_finite() || !s.humidity.is_finite() {
            return Err(Poll::Retry("the sensor reported a non-finite value".into()));
        }
        Ok(s)
    }
}
