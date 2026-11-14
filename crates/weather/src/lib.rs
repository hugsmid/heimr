//! Client for the [Open-Meteo](https://open-meteo.com) weather APIs.
//!
//! - **Forecast** ([`Client::forecast`]): hourly forecast rows
//!   ([`ForecastPoint`]) tagged with an explicit `issued_at`.
//! - **Historical weather / archive** ([`Client::archive`]): reanalysis
//!   (ERA5 and friends) as [`Observation`] rows, a few days behind real time.
//! - **Historical forecast** ([`Client::historical_forecast`]): archived
//!   short-range forecasts stitched into one continuous series, also as
//!   [`Observation`] rows; it covers the days the archive has not reached yet.
//! - **Degree-days** ([`degree_days`]): heating degree-days from hourly rows.
//!
//! Every request asks for `timeformat=unixtime`, `timezone=GMT`, °C, m/s and
//! mm, and the response's `hourly_units` are checked, so all times are
//! [`jiff::Timestamp`] (UTC) and units never drift silently. Missing values
//! are `None`.
//!
//! ```no_run
//! use weather::{Client, ForecastOptions, IssuedAt, Location};
//!
//! # async fn run() -> Result<(), weather::Error> {
//! let client = Client::new()?;
//! let reykjavik = Location::new(64.15, -21.94)?; // an example location
//! let points = client
//!     .forecast(reykjavik, &ForecastOptions::days(2), IssuedAt::FetchTime)
//!     .await?;
//! for p in points.iter().take(24) {
//!     println!("{} {:?} °C", p.valid_at, p.temperature_c);
//! }
//! # Ok(()) }
//! ```
//!
//! Open-Meteo data is licensed under CC BY 4.0: credit "Weather data by
//! Open-Meteo.com" with a link when you display or redistribute it. The free
//! API is for non-commercial use only. Not affiliated with Open-Meteo.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod client;
pub mod degree_days;
pub mod error;
pub mod model;

pub use client::{Client, ClientBuilder, ForecastOptions, IssuedAt, Location};
pub use degree_days::{
    BASE_15_5_C, BASE_18_C, BASE_65_F, DegreeDays, daily_heating_degree_days, heating_degree_days,
};
pub use error::{Error, Result};
pub use model::{ForecastPoint, HourlyResponse, Observation};
