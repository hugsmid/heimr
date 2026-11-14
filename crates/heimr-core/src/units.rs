//! Units, metric names, and heating setpoints with their allowed band.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::id::{self, IdError};

/// A heating setpoint in °C.
///
/// A `Setpoint` is always a number inside the absolute envelope
/// [`SetpointBand::ENVELOPE`] (5.0..=30.0 °C). Ordinary construction also
/// checks a [`SetpointBand`]:
///
/// - [`SetpointBand::setpoint`] (usually `policy.setpoint.setpoint(v)`)
///   checks the band of the [`SafetyPolicy`](crate::SafetyPolicy) in force;
/// - [`Setpoint::new`] checks the conservative [`SetpointBand::DEFAULT`]
///   (15.0..=24.0 °C);
/// - [`SetpointBand::clamp`] clamps into a band, explicitly.
///
/// The one way to skip the band check is the named escape
/// [`Setpoint::envelope_only`], used where no policy is at hand (e.g.
/// deserialising a command from another process). Whatever the
/// construction, the [`Actuator`](crate::Actuator) re-checks every setpoint
/// against the policy in force before it can be delivered.
///
/// There is no `Default`. Observed temperatures (readings, a device's
/// reported target) are plain `f64`, because devices legitimately report
/// values outside any command band (e.g. frost protection).
///
/// ```
/// use heimr_core::{Setpoint, SetpointBand};
/// assert_eq!(Setpoint::new(20.5).unwrap().get(), 20.5);
/// assert!(Setpoint::new(25.0).is_err()); // outside the default band
/// let band = SetpointBand::new(16.0, 21.0).unwrap();
/// assert!(band.setpoint(21.5).is_err());
/// assert_eq!(band.clamp(30.0).unwrap().get(), 21.0);
/// // The escape skips the band, never the envelope; the actuator re-checks.
/// assert!(Setpoint::envelope_only(26.0).is_ok());
/// assert!(Setpoint::envelope_only(40.0).is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct Setpoint(f64);

/// A value could not become a [`Setpoint`] or a [`SetpointBand`].
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum SetpointError {
    /// The value was NaN.
    #[error("setpoint is not a number")]
    NotANumber,
    /// The value was outside the absolute envelope.
    #[error("setpoint {0} °C is outside the absolute envelope 5.0..=30.0 °C")]
    OutsideEnvelope(f64),
    /// The value was outside the band it was checked against.
    #[error("setpoint {value} °C is outside the allowed band {min}..={max} °C")]
    OutsideBand {
        /// Requested value.
        value: f64,
        /// Band lower bound.
        min: f64,
        /// Band upper bound.
        max: f64,
    },
    /// A band's `min` exceeded its `max`.
    #[error("setpoint band is inverted: min {min} > max {max}")]
    InvertedBand {
        /// Lower bound.
        min: f64,
        /// Upper bound.
        max: f64,
    },
}

impl Setpoint {
    /// A setpoint inside the conservative [`SetpointBand::DEFAULT`].
    pub fn new(value: f64) -> Result<Self, SetpointError> {
        SetpointBand::DEFAULT.setpoint(value)
    }

    /// The named escape: a setpoint checked against the absolute envelope
    /// only, not against any band. Use it only where no policy is at hand;
    /// the actuator re-checks the band before delivery.
    pub fn envelope_only(value: f64) -> Result<Self, SetpointError> {
        if value.is_nan() {
            Err(SetpointError::NotANumber)
        } else if SetpointBand::ENVELOPE.contains_value(value) {
            Ok(Self(value))
        } else {
            Err(SetpointError::OutsideEnvelope(value))
        }
    }

    /// The value in °C.
    pub fn get(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for Setpoint {
    type Error = SetpointError;
    /// Deserialisation path: checks the envelope only (no policy is at hand);
    /// the actuator re-checks the band.
    fn try_from(v: f64) -> Result<Self, SetpointError> {
        Self::envelope_only(v)
    }
}

impl From<Setpoint> for f64 {
    fn from(c: Setpoint) -> f64 {
        c.0
    }
}

impl fmt::Display for Setpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} °C", self.0)
    }
}

/// An allowed setpoint range, inside [`SetpointBand::ENVELOPE`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawBand", into = "RawBand")]
pub struct SetpointBand {
    min: f64,
    max: f64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBand {
    min: f64,
    max: f64,
}

impl TryFrom<RawBand> for SetpointBand {
    type Error = SetpointError;
    fn try_from(r: RawBand) -> Result<Self, SetpointError> {
        Self::new(r.min, r.max)
    }
}

impl From<SetpointBand> for RawBand {
    fn from(b: SetpointBand) -> Self {
        Self {
            min: b.min,
            max: b.max,
        }
    }
}

impl SetpointBand {
    /// The absolute envelope, 5.0..=30.0 °C: no band, and no setpoint, can
    /// leave it.
    pub const ENVELOPE: Self = Self {
        min: 5.0,
        max: 30.0,
    };

    /// The conservative default band, 15.0..=24.0 °C.
    pub const DEFAULT: Self = Self {
        min: 15.0,
        max: 24.0,
    };

    /// A band; both bounds must be numbers inside the envelope and
    /// `min <= max`.
    pub fn new(min: f64, max: f64) -> Result<Self, SetpointError> {
        let min = Setpoint::envelope_only(min)?.get();
        let max = Setpoint::envelope_only(max)?.get();
        if min > max {
            return Err(SetpointError::InvertedBand { min, max });
        }
        Ok(Self { min, max })
    }

    /// Lower bound, °C.
    pub fn min(&self) -> f64 {
        self.min
    }

    /// Upper bound, °C.
    pub fn max(&self) -> f64 {
        self.max
    }

    fn contains_value(&self, v: f64) -> bool {
        self.min <= v && v <= self.max
    }

    /// Whether `s` lies inside the band.
    pub fn contains(&self, s: Setpoint) -> bool {
        self.contains_value(s.get())
    }

    /// Whether `other` lies entirely inside this band.
    pub fn includes(&self, other: &Self) -> bool {
        self.min <= other.min && other.max <= self.max
    }

    /// A setpoint, or an error if `value` is NaN or outside the band.
    pub fn setpoint(&self, value: f64) -> Result<Setpoint, SetpointError> {
        let s = Setpoint::envelope_only(value)?;
        if self.contains(s) {
            Ok(s)
        } else {
            Err(SetpointError::OutsideBand {
                value,
                min: self.min,
                max: self.max,
            })
        }
    }

    /// A setpoint, explicitly clamped into the band. NaN is still an error.
    pub fn clamp(&self, value: f64) -> Result<Setpoint, SetpointError> {
        if value.is_nan() {
            return Err(SetpointError::NotANumber);
        }
        Ok(Setpoint(value.clamp(self.min, self.max)))
    }
}

impl Default for SetpointBand {
    /// [`SetpointBand::DEFAULT`].
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Unit of a [`Reading`](crate::Reading) or [`Forecast`](crate::Forecast)
/// value. Serialised as its symbol (`"°C"`, `"%"`, `"µg/m³"`, …), which is
/// also how the store will keep it.
///
/// Money units ([`Unit::is_money`]) are in the **major unit of the site's
/// currency** (0.184, not 18.4 in a minor unit such as cents); the symbol `¤` stands for
/// that currency. Only an adapter kind that declares the currency its
/// source quotes
/// ([`AdapterDescriptor::source_currency`](crate::AdapterDescriptor::source_currency))
/// may emit them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Unit {
    /// Degrees Celsius.
    #[serde(rename = "°C")]
    Celsius,
    /// Percent (relative humidity, battery, filter life, valve position).
    #[serde(rename = "%")]
    Percent,
    /// Micrograms per cubic metre (particulates).
    #[serde(rename = "µg/m³")]
    MicrogramsPerCubicMetre,
    /// Parts per million (CO₂).
    #[serde(rename = "ppm")]
    PartsPerMillion,
    /// Parts per billion (VOC).
    #[serde(rename = "ppb")]
    PartsPerBillion,
    /// Watts.
    #[serde(rename = "W")]
    Watt,
    /// Watt-hours.
    #[serde(rename = "Wh")]
    WattHour,
    /// Kilowatt-hours.
    #[serde(rename = "kWh")]
    KilowattHour,
    /// Cubic metres (gas volume).
    #[serde(rename = "m³")]
    CubicMetre,
    /// Cubic metres per hour (air flow).
    #[serde(rename = "m³/h")]
    CubicMetrePerHour,
    /// Hectopascals (air pressure).
    #[serde(rename = "hPa")]
    Hectopascal,
    /// Lux (illuminance).
    #[serde(rename = "lx")]
    Lux,
    /// Metres per second (wind speed).
    #[serde(rename = "m/s")]
    MetrePerSecond,
    /// Millimetres (precipitation).
    #[serde(rename = "mm")]
    Millimetre,
    /// Hours (e.g. filter run time).
    #[serde(rename = "h")]
    Hour,
    /// Seconds (durations, CPU time).
    #[serde(rename = "s")]
    Second,
    /// Bytes (memory, storage, transferred data).
    #[serde(rename = "B")]
    Byte,
    /// Bytes per second (throughput).
    #[serde(rename = "B/s")]
    BytePerSecond,
    /// Dimensionless count, level or ratio.
    #[serde(rename = "1")]
    Dimensionless,
    /// Watts per square metre (irradiance, e.g. shortwave radiation).
    #[serde(rename = "W/m²")]
    WattPerSquareMetre,
    /// Degrees of angle (a direction, e.g. where the wind blows from,
    /// clockwise from north).
    #[serde(rename = "°")]
    Degree,
    /// An amount of money in the site currency's major unit (e.g. a daily
    /// standing charge).
    #[serde(rename = "¤")]
    Currency,
    /// A price per kilowatt-hour in the site currency's major unit.
    #[serde(rename = "¤/kWh")]
    CurrencyPerKilowattHour,
}

impl Unit {
    /// Every unit, in declaration order.
    pub const ALL: &'static [Self] = &[
        Self::Celsius,
        Self::Percent,
        Self::MicrogramsPerCubicMetre,
        Self::PartsPerMillion,
        Self::PartsPerBillion,
        Self::Watt,
        Self::WattHour,
        Self::KilowattHour,
        Self::CubicMetre,
        Self::CubicMetrePerHour,
        Self::Hectopascal,
        Self::Lux,
        Self::MetrePerSecond,
        Self::Millimetre,
        Self::Hour,
        Self::Second,
        Self::Byte,
        Self::BytePerSecond,
        Self::Dimensionless,
        Self::WattPerSquareMetre,
        Self::Degree,
        Self::Currency,
        Self::CurrencyPerKilowattHour,
    ];

    /// The unit's symbol, as stored.
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Celsius => "°C",
            Self::Percent => "%",
            Self::MicrogramsPerCubicMetre => "µg/m³",
            Self::PartsPerMillion => "ppm",
            Self::PartsPerBillion => "ppb",
            Self::Watt => "W",
            Self::WattHour => "Wh",
            Self::KilowattHour => "kWh",
            Self::CubicMetre => "m³",
            Self::CubicMetrePerHour => "m³/h",
            Self::Hectopascal => "hPa",
            Self::Lux => "lx",
            Self::MetrePerSecond => "m/s",
            Self::Millimetre => "mm",
            Self::Hour => "h",
            Self::Second => "s",
            Self::Byte => "B",
            Self::BytePerSecond => "B/s",
            Self::Dimensionless => "1",
            Self::WattPerSquareMetre => "W/m²",
            Self::Degree => "°",
            Self::Currency => "¤",
            Self::CurrencyPerKilowattHour => "¤/kWh",
        }
    }

    /// Whether values in this unit are money (in the site currency).
    ///
    /// ```
    /// use heimr_core::Unit;
    /// assert!(Unit::CurrencyPerKilowattHour.is_money());
    /// assert!(!Unit::KilowattHour.is_money());
    /// ```
    pub fn is_money(self) -> bool {
        matches!(self, Self::Currency | Self::CurrencyPerKilowattHour)
    }
}

impl fmt::Display for Unit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.symbol())
    }
}

/// A metric name such as `temperature` (same character rules as ids).
/// Common, vendor-neutral metrics are associated constants; adapters may
/// define their own with [`Metric::new`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Metric(Cow<'static, str>);

impl Metric {
    /// Air temperature (°C).
    pub const TEMPERATURE: Self = Self(Cow::Borrowed("temperature"));
    /// Relative humidity (%).
    pub const HUMIDITY: Self = Self(Cow::Borrowed("humidity"));
    /// PM2.5 particulates (µg/m³).
    pub const PM25: Self = Self(Cow::Borrowed("pm25"));
    /// PM10 particulates (µg/m³).
    pub const PM10: Self = Self(Cow::Borrowed("pm10"));
    /// CO₂ concentration (ppm).
    pub const CO2: Self = Self(Cow::Borrowed("co2"));
    /// Volatile organic compounds (ppb, or a dimensionless index).
    pub const VOC: Self = Self(Cow::Borrowed("voc"));
    /// Remaining filter life (%).
    pub const FILTER_LIFE: Self = Self(Cow::Borrowed("filter_life"));
    /// Fan level actually running (dimensionless, in the adapter's declared
    /// level range).
    pub const FAN_LEVEL: Self = Self(Cow::Borrowed("fan_level"));
    /// Air flow (m³/h).
    pub const AIR_FLOW: Self = Self(Cow::Borrowed("air_flow"));
    /// Heating demand or valve opening (%).
    pub const HEATING_DEMAND: Self = Self(Cow::Borrowed("heating_demand"));
    /// Instantaneous power (W).
    pub const POWER: Self = Self(Cow::Borrowed("power"));
    /// Energy (Wh or kWh).
    pub const ENERGY: Self = Self(Cow::Borrowed("energy"));
    /// Battery level (%).
    pub const BATTERY: Self = Self(Cow::Borrowed("battery"));

    /// Validate and wrap a metric name.
    pub fn new(name: impl Into<String>) -> Result<Self, IdError> {
        let name = name.into();
        id::validate(&name)?;
        Ok(Self(Cow::Owned(name)))
    }

    /// The name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Metric {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, IdError> {
        Self::new(s)
    }
}

impl TryFrom<String> for Metric {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, IdError> {
        Self::new(s)
    }
}

impl From<Metric> for String {
    fn from(m: Metric) -> String {
        m.0.into_owned()
    }
}

impl fmt::Display for Metric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn setpoint_edges() {
        assert_eq!(Setpoint::new(15.0).unwrap().get(), 15.0);
        assert_eq!(Setpoint::new(24.0).unwrap().get(), 24.0);
        assert!(matches!(
            Setpoint::new(14.99),
            Err(SetpointError::OutsideBand { .. })
        ));
        assert_eq!(Setpoint::new(f64::NAN), Err(SetpointError::NotANumber));
        assert_eq!(
            SetpointBand::DEFAULT.clamp(f64::NAN),
            Err(SetpointError::NotANumber)
        );
        assert_eq!(
            SetpointBand::DEFAULT
                .clamp(f64::NEG_INFINITY)
                .unwrap()
                .get(),
            15.0
        );
        assert!(serde_json::from_str::<Setpoint>("31.0").is_err());
        assert_eq!(
            serde_json::from_str::<Setpoint>("19.5").unwrap().get(),
            19.5
        );
        assert!(SetpointBand::new(22.0, 18.0).is_err());
        assert!(SetpointBand::new(4.0, 18.0).is_err());
        assert!(SetpointBand::new(f64::NAN, 18.0).is_err());
        assert!(serde_json::from_str::<SetpointBand>(r#"{"min": 20, "max": 19}"#).is_err());
        assert!(SetpointBand::ENVELOPE.includes(&SetpointBand::DEFAULT));
        assert!(!SetpointBand::DEFAULT.includes(&SetpointBand::ENVELOPE));
    }

    #[test]
    fn irradiance_and_direction_units() {
        assert_eq!(
            serde_json::to_string(&Unit::WattPerSquareMetre).unwrap(),
            "\"W/m²\""
        );
        assert_eq!(serde_json::to_string(&Unit::Degree).unwrap(), "\"°\"");
        assert_eq!(serde_json::from_str::<Unit>("\"°\"").unwrap(), Unit::Degree);
        assert!(!Unit::Degree.is_money() && !Unit::WattPerSquareMetre.is_money());
    }

    #[test]
    fn unit_symbols_round_trip() {
        for u in Unit::ALL {
            let json = serde_json::to_string(u).unwrap();
            assert_eq!(json, format!("\"{}\"", u.symbol()));
            assert_eq!(serde_json::from_str::<Unit>(&json).unwrap(), *u);
        }
    }

    proptest! {
        /// A setpoint built through a band lies inside that band and the
        /// envelope; the escape never leaves the envelope.
        #[test]
        fn setpoint_never_escapes(v in proptest::num::f64::ANY, lo in 5.0f64..30.0, w in 0.0f64..25.0) {
            let band = SetpointBand::new(lo, (lo + w).min(30.0)).unwrap();
            for s in [band.setpoint(v), band.clamp(v)].into_iter().flatten() {
                prop_assert!(band.contains(s));
                prop_assert!(SetpointBand::ENVELOPE.contains(s));
            }
            if let Ok(s) = Setpoint::new(v) {
                prop_assert!(SetpointBand::DEFAULT.contains(s));
            }
            if let Ok(s) = Setpoint::envelope_only(v) {
                prop_assert!(SetpointBand::ENVELOPE.contains(s));
            }
        }

        #[test]
        fn strict_never_moves_value(v in -100.0f64..100.0) {
            if let Ok(s) = Setpoint::new(v) {
                prop_assert_eq!(s.get(), v);
            }
        }
    }
}
