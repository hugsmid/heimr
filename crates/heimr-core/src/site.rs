//! Site-wide locale: the time zone for calendar bucketing and presentation,
//! and the currency prices are quoted in.
//!
//! Both come from the site's configuration and have no default in heimr:
//! a home in one country must never silently inherit another's calendar or
//! currency. Timestamps stay UTC everywhere; the zone only decides where a
//! local day (or month) starts and ends, and how times are shown.
//!
//! Prices are numbers in the currency's **major unit** (e.g. EUR per kWh,
//! not cents). Minor units are a presentation concern.
//!
//! ```
//! use heimr_core::Site;
//! let site = Site::new("Europe/Berlin", "EUR").unwrap();
//! assert_eq!(site.time_zone_name(), "Europe/Berlin");
//! assert_eq!(site.currency().as_str(), "EUR");
//! // A local calendar day is 23, 24 or 25 hours long.
//! let date = jiff::civil::date(2030, 3, 31);
//! let (start, end) = site.day_bounds(date).unwrap();
//! assert_eq!(end.duration_since(start), jiff::SignedDuration::from_hours(23));
//! assert!(Site::new("Mars/Olympus_Mons", "EUR").is_err());
//! assert!(Site::new("Europe/Berlin", "euro").is_err());
//! ```

use std::fmt;
use std::str::FromStr;

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};
use serde::{Deserialize, Serialize};

/// A site setting is missing or invalid.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SiteError {
    /// The time zone is not a known IANA name.
    #[error(
        "unknown time zone {0:?}: use an IANA name such as \"Europe/Berlin\" or \"America/New_York\""
    )]
    UnknownTimeZone(String),
    /// The currency is not three upper-case letters.
    #[error("invalid currency {0:?}: use an ISO 4217 code such as \"EUR\" or \"USD\"")]
    InvalidCurrency(String),
}

/// An ISO 4217 currency code: three upper-case ASCII letters. The shape is
/// checked, not membership of the ISO list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Currency([u8; 3]);

impl Currency {
    /// Validate a code.
    pub fn new(code: &str) -> Result<Self, SiteError> {
        Self::parse(code.as_bytes()).ok_or_else(|| SiteError::InvalidCurrency(code.to_owned()))
    }

    /// A code known when the program is written, for an adapter's
    /// descriptor. Bind it to a `const` so a bad code fails to compile:
    ///
    /// ```
    /// use heimr_core::Currency;
    /// const USD: Currency = Currency::of("USD");
    /// assert_eq!(USD.as_str(), "USD");
    /// ```
    ///
    /// ```compile_fail
    /// const BAD: heimr_core::Currency = heimr_core::Currency::of("usd");
    /// ```
    ///
    /// # Panics
    /// If `code` is not three upper-case ASCII letters. Outside a `const`,
    /// use [`Currency::new`].
    pub const fn of(code: &str) -> Self {
        match Self::parse(code.as_bytes()) {
            Some(c) => c,
            None => panic!("a currency code is three upper-case ASCII letters"),
        }
    }

    const fn parse(code: &[u8]) -> Option<Self> {
        match code {
            &[a, b, c]
                if a.is_ascii_uppercase() && b.is_ascii_uppercase() && c.is_ascii_uppercase() =>
            {
                Some(Self([a, b, c]))
            }
            _ => None,
        }
    }

    /// The code, e.g. `"EUR"`.
    pub fn as_str(&self) -> &str {
        // Three ASCII upper-case letters, checked in `new`.
        std::str::from_utf8(&self.0).unwrap_or("XXX")
    }
}

impl FromStr for Currency {
    type Err = SiteError;
    fn from_str(s: &str) -> Result<Self, SiteError> {
        Self::new(s)
    }
}

impl TryFrom<String> for Currency {
    type Error = SiteError;
    fn try_from(s: String) -> Result<Self, SiteError> {
        Self::new(&s)
    }
}

impl From<Currency> for String {
    fn from(c: Currency) -> String {
        c.as_str().to_owned()
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The site's time zone and currency. Built from configuration by the app
/// and handed to adapters at prepare time
/// ([`Adapter::build`](crate::Adapter::build)) and at run time
/// ([`AdapterContext::site`](crate::AdapterContext::site)).
#[derive(Clone)]
pub struct Site {
    time_zone: TimeZone,
    name: String,
    currency: Currency,
}

impl Site {
    /// A site in IANA zone `time_zone` (looked up in the time zone
    /// database) quoting prices in `currency`.
    pub fn new(time_zone: &str, currency: &str) -> Result<Self, SiteError> {
        let tz = TimeZone::get(time_zone)
            .map_err(|_| SiteError::UnknownTimeZone(time_zone.to_owned()))?;
        Ok(Self {
            time_zone: tz,
            name: time_zone.to_owned(),
            currency: Currency::new(currency)?,
        })
    }

    /// The time zone, for calendar bucketing and presentation.
    pub fn time_zone(&self) -> &TimeZone {
        &self.time_zone
    }

    /// The zone's IANA name, as configured.
    pub fn time_zone_name(&self) -> &str {
        &self.name
    }

    /// The currency prices are quoted in (major units).
    pub fn currency(&self) -> Currency {
        self.currency
    }

    /// `t` in the site's zone.
    pub fn local(&self, t: Timestamp) -> Zoned {
        t.to_zoned(self.time_zone.clone())
    }

    /// The local calendar date `t` falls on.
    pub fn local_date(&self, t: Timestamp) -> Date {
        self.local(t).date()
    }

    /// Start (inclusive) and end (exclusive) of local calendar day `date`,
    /// as UTC instants: 23, 24 or 25 hours apart around DST changes.
    pub fn day_bounds(&self, date: Date) -> Result<(Timestamp, Timestamp), jiff::Error> {
        let start = date.to_zoned(self.time_zone.clone())?.start_of_day()?;
        let end = start.tomorrow()?.start_of_day()?;
        Ok((start.timestamp(), end.timestamp()))
    }
}

impl fmt::Debug for Site {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Site")
            .field("time_zone", &self.name)
            .field("currency", &self.currency.as_str())
            .finish()
    }
}

impl PartialEq for Site {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.currency == other.currency
    }
}

impl Eq for Site {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn currency_shape() {
        assert!(Currency::new("USD").is_ok());
        for bad in ["", "us", "usd", "USDX", "U5D", "ÜSD"] {
            assert!(Currency::new(bad).is_err(), "{bad}");
        }
        let c: Currency = serde_json::from_str("\"EUR\"").unwrap();
        assert_eq!(serde_json::to_string(&c).unwrap(), "\"EUR\"");
        assert!(serde_json::from_str::<Currency>("\"eur\"").is_err());
    }

    #[test]
    fn days_follow_the_zone() {
        let site = Site::new("America/New_York", "USD").unwrap();
        let (s, e) = site.day_bounds(jiff::civil::date(2030, 11, 3)).unwrap();
        assert_eq!(e.duration_since(s), jiff::SignedDuration::from_hours(25));
        assert_eq!(s.to_string(), "2030-11-03T04:00:00Z");
        let t: Timestamp = "2030-07-01T03:00:00Z".parse().unwrap();
        assert_eq!(site.local_date(t), jiff::civil::date(2030, 6, 30));
        assert!(Site::new("", "USD").is_err());
    }
}
