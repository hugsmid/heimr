//! Client for the Octopus Energy (UK) APIs.
//!
//! - **REST** ([`Client`]): products and tariffs, Agile half-hourly unit
//!   rates, standing charges, smart-meter consumption (electricity and gas),
//!   account and meter-point lookups.
//! - **Kraken GraphQL** ([`KrakenClient`]): live smart-meter telemetry (needs
//!   a compatible consumer access device paired with the meter) and smart-meter
//!   discovery.
//! - **Slot maths** ([`slot`]): DST-safe half-hour alignment; [`prices`]
//!   expands rate rows to per-slot prices and finds cheap windows.
//!
//! All timestamps are [`jiff::Timestamp`] (UTC). Money is in pence, energy in
//! kWh (REST) or Wh (telemetry), power in W.
//!
//! ```no_run
//! use jiff::{Timestamp, ToSpan};
//! use octopus_energy::{Client, Period, TariffCode, prices};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let client = Client::public()?;
//! let tariff: TariffCode = "E-1R-AGILE-24-10-01-B".parse()?;
//! let now = Timestamp::now();
//! let rates = client.unit_rates(&tariff, Period::new(now, now + 24.hours())).await?;
//! let slots = prices::slot_prices(&rates, now, now + 24.hours())?;
//! for p in prices::cheapest(&slots, 3) {
//!     println!("{} {:.2}p", p.slot, p.inc_vat);
//! }
//! # Ok(()) }
//! ```
//!
//! Not affiliated with or endorsed by Octopus Energy.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod decimal;
pub mod error;
pub mod kraken;
pub mod model;
pub mod prices;
pub mod rest;
pub mod slot;
pub mod tariff;

pub use error::{Error, Result};
pub use kraken::{KrakenClient, Telemetry, TelemetryGrouping};
pub use rest::{Client, ClientBuilder, Grouping, MeterId, Period, ProductFilter};
pub use slot::HalfHour;
pub use tariff::{Fuel, Region, TariffCode};
