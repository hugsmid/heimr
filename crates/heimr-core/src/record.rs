//! What adapters emit: readings, forecasts, device state and events.
//!
//! Provenance is the core's job: every record travels in an [`Envelope`]
//! whose `source` the core stamps, and an adapter's emitter rejects
//! readings and events that name anyone but the adapter instance, and events
//! of a kind reserved for the core (see [`Event::is_reserved_kind`]).

use std::fmt;

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use crate::domain::{ClimateState, FanState, SwitchState};
use crate::id::DeviceId;
use crate::units::{Metric, Unit};

/// Data quality flag stored with every reading.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Quality {
    /// Measured and plausible.
    #[default]
    Good,
    /// Derived or interpolated.
    Estimated,
    /// Measured but outside expected bounds.
    Suspect,
    /// Known bad; kept for audit, excluded from aggregates.
    Bad,
}

/// One sample: time, source, device, metric, value, unit and quality.
///
/// Adapters build readings with
/// [`AdapterContext::reading`](crate::AdapterContext::reading), which stamps
/// the instance name as `source`; the emitter rejects any other source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Reading {
    /// Sample time (UTC).
    pub time: Timestamp,
    /// Adapter instance that produced it.
    pub source: String,
    /// Device within the source.
    pub device: DeviceId,
    /// What was measured.
    pub metric: Metric,
    /// Value in `unit`. NaN and ±∞ are allowed and passed through
    /// unchanged: the store will record them as bad quality, so adapters must
    /// not filter them out. On the wire a non-finite value is a string
    /// ([`value_serde`]), so it survives JSON.
    #[serde(with = "value_serde")]
    pub value: f64,
    /// Unit of `value`.
    pub unit: Unit,
    /// Quality flag.
    pub quality: Quality,
    /// The source's revision time or version of this value, for a
    /// versioned series ([`SeriesTemplate::versioned`](crate::SeriesTemplate#structfield.versioned));
    /// `None` otherwise. See [`Reading::with_changed_at`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_at: Option<u64>,
}

impl Reading {
    /// A reading with [`Quality::Good`].
    pub fn new(
        time: Timestamp,
        source: impl Into<String>,
        device: DeviceId,
        metric: Metric,
        value: f64,
        unit: Unit,
    ) -> Self {
        Self {
            time,
            source: source.into(),
            device,
            metric,
            value,
            unit,
            quality: Quality::Good,
            changed_at: None,
        }
    }

    /// Replace the quality flag.
    #[must_use]
    pub fn with_quality(mut self, quality: Quality) -> Self {
        self.quality = quality;
        self
    }

    /// Set the revision marker of a reading of a versioned series: the
    /// source's revision time or version of this value, higher for each
    /// revision. The emitter requires it on versioned series and refuses
    /// it on others.
    ///
    /// It must be the same every time one revision is re-sent, so it is
    /// **never the current fetch time**: use the source's own revision
    /// marker, or else the time the adapter first saw this revision of the
    /// value, kept in its state directory (see [`revision_time`]).
    #[must_use]
    pub fn with_changed_at(mut self, changed_at: u64) -> Self {
        self.changed_at = Some(changed_at);
        self
    }

    /// Replace the sample time.
    #[must_use]
    pub fn with_time(mut self, time: Timestamp) -> Self {
        self.time = time;
        self
    }
}

/// Serde for record values that keeps NaN and ±∞: a finite value is a
/// plain number; `inf` and `-inf` are strings; NaN is `"NaN"`, or
/// `"NaN:0x<bits>"` when its bits are not the canonical NaN's, so every
/// value reads back bit for bit (JSON has no NaN or infinity, and would
/// otherwise write `null`).
///
/// ```
/// # use heimr_core::{DeviceId, Metric, Reading, Unit};
/// let r = Reading::new("2030-01-01T00:00:00Z".parse().unwrap(), "s",
///     DeviceId::new("d").unwrap(), Metric::TEMPERATURE, f64::NAN, Unit::Celsius);
/// let json = serde_json::to_value(&r).unwrap();
/// assert_eq!(json["value"], "NaN");
/// let back: Reading = serde_json::from_value(json).unwrap();
/// assert!(back.value.is_nan());
/// ```
pub mod value_serde {
    use serde::de::{self, Visitor};
    use serde::{Deserializer, Serializer};

    /// Write `v`: a number if finite, else a string.
    pub fn serialize<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
        let v = *v;
        if v.is_finite() {
            s.serialize_f64(v)
        } else if v == f64::INFINITY {
            s.serialize_str("inf")
        } else if v == f64::NEG_INFINITY {
            s.serialize_str("-inf")
        } else if v.to_bits() == f64::NAN.to_bits() {
            s.serialize_str("NaN")
        } else {
            s.serialize_str(&format!("NaN:0x{:016x}", v.to_bits()))
        }
    }

    /// Read a number, or one of the strings [`serialize`] writes.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        d.deserialize_any(Value)
    }

    struct Value;

    impl Visitor<'_> for Value {
        type Value = f64;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a number, \"NaN\", \"NaN:0x<bits>\", \"inf\" or \"-inf\"")
        }

        fn visit_f64<E: de::Error>(self, v: f64) -> Result<f64, E> {
            Ok(v)
        }

        #[allow(clippy::cast_precision_loss)]
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<f64, E> {
            Ok(v as f64)
        }

        #[allow(clippy::cast_precision_loss)]
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<f64, E> {
            Ok(v as f64)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<f64, E> {
            match v {
                "NaN" => Ok(f64::NAN),
                "inf" => Ok(f64::INFINITY),
                "-inf" => Ok(f64::NEG_INFINITY),
                _ => v
                    .strip_prefix("NaN:0x")
                    .and_then(|h| u64::from_str_radix(h, 16).ok())
                    .map(f64::from_bits)
                    .filter(|x| x.is_nan())
                    .ok_or_else(|| E::invalid_value(de::Unexpected::Str(v), &self)),
            }
        }
    }
}

/// A time as a revision marker (`changed_at`): nanoseconds since the Unix
/// epoch, 0 for earlier times. Use it for a revision *time* (the source's,
/// or when the adapter first saw a revision), never for the time of the
/// current fetch.
///
/// ```
/// use heimr_core::revision_time;
/// assert_eq!(revision_time("1970-01-01T00:00:01Z".parse().unwrap()), 1_000_000_000);
/// assert_eq!(revision_time("1969-12-31T23:59:59Z".parse().unwrap()), 0);
/// ```
pub fn revision_time(t: Timestamp) -> u64 {
    u64::try_from(t.as_nanosecond()).unwrap_or(if t < Timestamp::UNIX_EPOCH {
        0
    } else {
        u64::MAX
    })
}

/// How far ahead of its issue time a forecast may reach by default:
/// `valid_to - issued_at` at most 16 days. The app's `[heimr]
/// forecast_horizon` setting replaces it; it is not an adapter key.
pub const DEFAULT_FORECAST_HORIZON: SignedDuration = SignedDuration::from_hours(16 * 24);

/// How far ahead of the clock a record may be stamped: a forecast's
/// `issued_at` (the emitter refuses one later than now plus this) and, in
/// the test kit, any record's time.
pub const MAX_FUTURE_SKEW: SignedDuration = SignedDuration::from_secs(1);

/// Whether a [`Forecast`]'s value is committed or predicted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Basis {
    /// A value the source has committed to, such as a published tariff
    /// rate.
    Published,
    /// A prediction, such as a weather forecast.
    Modelled,
}

impl Basis {
    /// `published` or `modelled`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::Modelled => "modelled",
        }
    }
}

/// A value for a future (or past) period, issued at a known time: a
/// published price, a weather forecast.
///
/// The period is `valid_from..valid_to`, end exclusive; an instant has
/// `valid_from == valid_to`. [`Record::time`] is `issued_at`, so the rule
/// for how far records may be stamped from the clock applies to when it was
/// issued, not to the period. The emitter also requires
/// `valid_from <= valid_to`, `valid_to - issued_at` within the forecast
/// horizon ([`DEFAULT_FORECAST_HORIZON`] unless the app sets another)
/// ([`Forecast::validate`]). A NaN or infinite value is passed through
/// unchanged, as for a [`Reading`].
///
/// Adapters build forecasts with
/// [`AdapterContext::forecast`](crate::AdapterContext::forecast). The core
/// stamps the source on the [`Envelope`]; a forecast carries none of its
/// own, so it cannot name another instance.
///
/// ```
/// use heimr_core::{Basis, DeviceId, Forecast, Metric, Unit};
/// use jiff::{SignedDuration, Timestamp};
///
/// let issued: Timestamp = "2030-01-01T16:00:00Z".parse().unwrap();
/// let slot: Timestamp = "2030-01-02T00:00:00Z".parse().unwrap();
/// let rate = Forecast::new(issued, DeviceId::new("tariff").unwrap(), Metric::new("unit_rate").unwrap(),
///         0.184, Unit::CurrencyPerKilowattHour, Basis::Published)
///     .valid_between(slot, slot + SignedDuration::from_mins(30));
/// assert!(rate.validate(SignedDuration::from_hours(24)).is_ok());
/// assert!(rate.validate(SignedDuration::from_hours(1)).is_err()); // beyond the horizon
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Forecast {
    /// When the source issued it, or when the adapter first saw it (UTC).
    pub issued_at: Timestamp,
    /// Start of the period it is for (UTC, inclusive).
    pub valid_from: Timestamp,
    /// End of the period (UTC, exclusive; equal to `valid_from` for an
    /// instant).
    pub valid_to: Timestamp,
    /// Device (or tariff, location, …) within the source.
    pub device: DeviceId,
    /// What it is for.
    pub metric: Metric,
    /// Value in `unit`. NaN and ±∞ are passed through unchanged (on the
    /// wire as strings, [`value_serde`]).
    #[serde(with = "value_serde")]
    pub value: f64,
    /// Unit of `value`.
    pub unit: Unit,
    /// Quality flag.
    pub quality: Quality,
    /// Committed or predicted.
    pub basis: Basis,
    /// The revision marker: forecast series are versioned, so the emitter
    /// requires it. See [`Forecast::with_changed_at`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_at: Option<u64>,
}

/// A forecast failed validation at the adapter boundary.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ForecastError {
    /// The period ends before it starts.
    #[error("forecast {metric} on {device}: valid_from {valid_from} is after valid_to {valid_to}")]
    Inverted {
        /// Device.
        device: DeviceId,
        /// Metric.
        metric: Metric,
        /// Start.
        valid_from: Timestamp,
        /// End.
        valid_to: Timestamp,
    },
    /// The forecast claims to be issued later than now (plus
    /// [`MAX_FUTURE_SKEW`]).
    #[error(
        "forecast {metric} on {device}: issued_at {issued_at} is after the clock's {now} (allowed skew {MAX_FUTURE_SKEW:#})"
    )]
    IssuedInFuture {
        /// Device.
        device: DeviceId,
        /// Metric.
        metric: Metric,
        /// Issue time.
        issued_at: Timestamp,
        /// The clock's time.
        now: Timestamp,
    },
    /// The period ends further ahead of the issue time than the horizon.
    #[error(
        "forecast {metric} on {device}: valid_to {valid_to} is more than the {horizon:#} horizon after issued_at {issued_at}"
    )]
    BeyondHorizon {
        /// Device.
        device: DeviceId,
        /// Metric.
        metric: Metric,
        /// Issue time.
        issued_at: Timestamp,
        /// End of the period.
        valid_to: Timestamp,
        /// The horizon in force.
        horizon: SignedDuration,
    },
}

impl Forecast {
    /// A forecast with [`Quality::Good`], issued at `issued_at` and valid at
    /// that instant until set with [`Forecast::valid_at`] or
    /// [`Forecast::valid_between`].
    pub fn new(
        issued_at: Timestamp,
        device: DeviceId,
        metric: Metric,
        value: f64,
        unit: Unit,
        basis: Basis,
    ) -> Self {
        Self {
            issued_at,
            valid_from: issued_at,
            valid_to: issued_at,
            device,
            metric,
            value,
            unit,
            quality: Quality::Good,
            basis,
            changed_at: None,
        }
    }

    /// Set the revision marker: the issue time of this revision of the
    /// value ([`revision_time`]), or the source's version. Forecast series
    /// are versioned, so the emitter requires one; the highest is current.
    ///
    /// It must be the same every time one revision is re-sent, so it is
    /// **never the current fetch time**: an unchanged value keeps the
    /// marker it was first sent with.
    #[must_use]
    pub fn with_changed_at(mut self, changed_at: u64) -> Self {
        self.changed_at = Some(changed_at);
        self
    }

    /// Valid at the instant `t`.
    #[must_use]
    pub fn valid_at(self, t: Timestamp) -> Self {
        self.valid_between(t, t)
    }

    /// Valid over `from..to` (end exclusive).
    #[must_use]
    pub fn valid_between(mut self, from: Timestamp, to: Timestamp) -> Self {
        self.valid_from = from;
        self.valid_to = to;
        self
    }

    /// Replace the issue time.
    #[must_use]
    pub fn with_issued_at(mut self, t: Timestamp) -> Self {
        self.issued_at = t;
        self
    }

    /// Replace the quality flag.
    #[must_use]
    pub fn with_quality(mut self, quality: Quality) -> Self {
        self.quality = quality;
        self
    }

    /// Check that it was not issued later than `now` plus
    /// [`MAX_FUTURE_SKEW`] (the emitter checks this against its clock).
    pub fn validate_issued(&self, now: Timestamp) -> Result<(), ForecastError> {
        if self.issued_at.duration_since(now) > MAX_FUTURE_SKEW {
            Err(ForecastError::IssuedInFuture {
                device: self.device.clone(),
                metric: self.metric.clone(),
                issued_at: self.issued_at,
                now,
            })
        } else {
            Ok(())
        }
    }

    /// Check the period rules the emitter enforces:
    /// `valid_from <= valid_to`, and `valid_to - issued_at <= horizon`. The
    /// value is not checked: NaN and ±∞ pass through.
    pub fn validate(&self, horizon: SignedDuration) -> Result<(), ForecastError> {
        if self.valid_from > self.valid_to {
            return Err(ForecastError::Inverted {
                device: self.device.clone(),
                metric: self.metric.clone(),
                valid_from: self.valid_from,
                valid_to: self.valid_to,
            });
        }
        if self.valid_to.duration_since(self.issued_at) > horizon {
            return Err(ForecastError::BeyondHorizon {
                device: self.device.clone(),
                metric: self.metric.clone(),
                issued_at: self.issued_at,
                valid_to: self.valid_to,
                horizon,
            });
        }
        Ok(())
    }
}

/// An event: a command decision with its
/// inputs and reason, an adapter starting or failing, something an adapter
/// observed, …
///
/// Adapters build events with
/// [`AdapterContext::event`](crate::AdapterContext::event), which stamps the
/// instance name as `actor`; the emitter rejects any other actor and any
/// kind reserved for the core.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Event {
    /// When it happened (UTC).
    pub time: Timestamp,
    /// What happened, e.g. [`Event::COMMAND`].
    pub kind: String,
    /// Who did it: `planner`, `assistant`, `user`, an adapter instance, …
    pub actor: String,
    /// Structured details.
    pub payload: serde_json::Value,
}

impl Event {
    /// A command passed through the actuator (delivered, shadowed or
    /// rejected). Core only.
    pub const COMMAND: &'static str = "command";
    /// An accepted command did not reach its adapter instance (it stopped,
    /// or the audit write timed out). Core only.
    pub const COMMAND_DROPPED: &'static str = "command_dropped";
    /// An adapter instance started. Core only.
    pub const ADAPTER_STARTED: &'static str = "adapter_started";
    /// An adapter instance stopped at shutdown. Core only.
    pub const ADAPTER_STOPPED: &'static str = "adapter_stopped";
    /// An adapter instance failed (and is restarting, or stays down if the
    /// error was fatal). Core only.
    pub const ADAPTER_FAILED: &'static str = "adapter_failed";
    /// An adapter's health changed (`ctx.health()`). Core only.
    pub const ADAPTER_HEALTH: &'static str = "adapter_health";

    /// Whether `kind` is reserved for core emitters. Reserved are
    /// `command`, anything starting with `command_`, `adapter_` or `heimr`,
    /// so future core kinds cannot collide with adapter events.
    ///
    /// ```
    /// use heimr_core::Event;
    /// assert!(Event::is_reserved_kind("adapter_health"));
    /// assert!(Event::is_reserved_kind("command"));
    /// assert!(!Event::is_reserved_kind("door_opened"));
    /// ```
    pub fn is_reserved_kind(kind: &str) -> bool {
        kind == "command"
            || kind.starts_with("command_")
            || kind.starts_with("adapter_")
            || kind.starts_with("heimr")
    }

    /// Build an event.
    pub fn new(
        time: Timestamp,
        kind: impl Into<String>,
        actor: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            time,
            kind: kind.into(),
            actor: actor.into(),
            payload,
        }
    }
}

/// Anything an adapter can emit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Record {
    /// A sensor sample.
    Reading(Reading),
    /// A value for a period, issued ahead of (or after) it.
    Forecast(Forecast),
    /// A heating zone's state.
    Climate(ClimateState),
    /// An air-treatment device's state.
    Fan(FanState),
    /// A switch or plug's state.
    Switch(SwitchState),
    /// An event.
    Event(Event),
}

impl Record {
    /// The record's own timestamp (for a forecast, when it was issued).
    pub fn time(&self) -> Timestamp {
        match self {
            Self::Reading(r) => r.time,
            Self::Forecast(f) => f.issued_at,
            Self::Climate(s) => s.time,
            Self::Fan(s) => s.time,
            Self::Switch(s) => s.time,
            Self::Event(e) => e.time,
        }
    }
}

impl From<Reading> for Record {
    fn from(r: Reading) -> Self {
        Self::Reading(r)
    }
}
impl From<Forecast> for Record {
    fn from(f: Forecast) -> Self {
        Self::Forecast(f)
    }
}
impl From<ClimateState> for Record {
    fn from(s: ClimateState) -> Self {
        Self::Climate(s)
    }
}
impl From<FanState> for Record {
    fn from(s: FanState) -> Self {
        Self::Fan(s)
    }
}
impl From<SwitchState> for Record {
    fn from(s: SwitchState) -> Self {
        Self::Switch(s)
    }
}
impl From<Event> for Record {
    fn from(e: Event) -> Self {
        Self::Event(e)
    }
}

/// A [`Record`] stamped with the source it came from. Only the core builds
/// envelopes ([`Emitter`](crate::Emitter)), so the source can be trusted
/// downstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Envelope {
    /// Adapter instance name, or `actuator` / `heimr` for core records.
    pub source: String,
    /// The record.
    pub record: Record,
}

impl Envelope {
    pub(crate) fn new(source: impl Into<String>, record: Record) -> Self {
        Self {
            source: source.into(),
            record,
        }
    }

    /// For state records, the key that identifies the zone or device's
    /// state: the emitting instance plus the zone or device id. Two
    /// instances reporting the same zone id never overwrite each other.
    pub fn state_key(&self) -> Option<StateKey> {
        let (class, id) = match &self.record {
            Record::Climate(s) => (StateClass::Climate, s.zone.as_str()),
            Record::Fan(s) => (StateClass::Fan, s.device.as_str()),
            Record::Switch(s) => (StateClass::Switch, s.device.as_str()),
            Record::Reading(_) | Record::Forecast(_) | Record::Event(_) => return None,
        };
        Some(StateKey {
            source: self.source.clone(),
            class,
            id: id.to_owned(),
        })
    }
}

/// The device class of a state record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StateClass {
    /// [`ClimateState`].
    Climate,
    /// [`FanState`].
    Fan,
    /// [`SwitchState`].
    Switch,
}

/// Identifies one zone's or device's state: (source instance, class, id).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StateKey {
    /// The emitting adapter instance (core-stamped).
    pub source: String,
    /// Device class.
    pub class: StateClass,
    /// Zone or device id within the source.
    pub id: String,
}

impl fmt::Display for StateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.source, self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn rate(issued: &str, from: &str, to: &str, value: f64) -> Forecast {
        Forecast::new(
            t(issued),
            DeviceId::new("tariff").unwrap(),
            Metric::new("unit_rate").unwrap(),
            value,
            Unit::CurrencyPerKilowattHour,
            Basis::Published,
        )
        .valid_between(t(from), t(to))
    }

    #[test]
    fn forecast_rules() {
        let h = DEFAULT_FORECAST_HORIZON;
        let ok = rate(
            "2030-01-01T16:00:00Z",
            "2030-01-02T00:00:00Z",
            "2030-01-02T00:30:00Z",
            0.2,
        );
        assert_eq!(ok.validate(h), Ok(()));
        // An instant, and a period wholly in the past, are fine.
        let instant = ok.clone().valid_at(t("2030-01-01T17:00:00Z"));
        assert_eq!(instant.validate(h), Ok(()));
        let past = rate(
            "2030-01-01T16:00:00Z",
            "2029-12-31T00:00:00Z",
            "2029-12-31T00:30:00Z",
            -0.01,
        );
        assert_eq!(past.validate(h), Ok(()));
        assert!(matches!(
            rate(
                "2030-01-01T16:00:00Z",
                "2030-01-02T00:30:00Z",
                "2030-01-02T00:00:00Z",
                0.2
            )
            .validate(h),
            Err(ForecastError::Inverted { .. })
        ));
        // Non-finite values pass: the store will record them as bad quality.
        for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let f = rate(
                "2030-01-01T16:00:00Z",
                "2030-01-02T00:00:00Z",
                "2030-01-02T00:30:00Z",
                v,
            );
            assert_eq!(f.validate(h), Ok(()), "{v}");
        }
        // Exactly the horizon is allowed; one second more is not.
        let edge = rate(
            "2030-01-01T00:00:00Z",
            "2030-01-16T00:00:00Z",
            "2030-01-17T00:00:00Z",
            1.0,
        );
        assert_eq!(edge.validate(h), Ok(()));
        let e = edge
            .with_issued_at(t("2029-12-31T23:59:59Z"))
            .validate(h)
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "forecast unit_rate on tariff: valid_to 2030-01-17T00:00:00Z is more than the 384h horizon after issued_at 2029-12-31T23:59:59Z"
        );
    }

    proptest::proptest! {
        /// Every value, NaN payloads, ±∞ and -0.0 included, survives a JSON
        /// round trip bit for bit, in readings and forecasts.
        #[test]
        fn values_round_trip_through_json(bits in proptest::prelude::any::<u64>()) {
            let v = f64::from_bits(bits);
            let r = Reading::new(t("2030-01-01T00:00:00Z"), "s", DeviceId::new("d").unwrap(),
                Metric::ENERGY, v, Unit::KilowattHour);
            let back: Reading = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
            proptest::prop_assert_eq!(back.value.to_bits(), bits);
            let f = rate("2030-01-01T16:00:00Z", "2030-01-02T00:00:00Z", "2030-01-02T00:30:00Z", v);
            let back: Forecast = serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
            proptest::prop_assert_eq!(back.value.to_bits(), bits);
        }
    }

    #[test]
    fn non_finite_values_on_the_wire() {
        let json = |v: f64| {
            serde_json::to_value(Reading::new(
                t("2030-01-01T00:00:00Z"),
                "s",
                DeviceId::new("d").unwrap(),
                Metric::ENERGY,
                v,
                Unit::KilowattHour,
            ))
            .unwrap()["value"]
                .clone()
        };
        assert_eq!(json(1.5), serde_json::json!(1.5));
        assert_eq!(json(f64::INFINITY), "inf");
        assert_eq!(json(f64::NEG_INFINITY), "-inf");
        assert_eq!(json(f64::NAN), "NaN");
        assert_eq!(
            json(f64::from_bits(0x7ff8_0000_0000_beef)),
            "NaN:0x7ff800000000beef"
        );
        // Integers read as numbers; anything else is refused.
        let mut r = serde_json::to_value(Reading::new(
            t("2030-01-01T00:00:00Z"),
            "s",
            DeviceId::new("d").unwrap(),
            Metric::ENERGY,
            1.0,
            Unit::KilowattHour,
        ))
        .unwrap();
        r["value"] = serde_json::json!(3);
        assert_eq!(
            serde_json::from_value::<Reading>(r.clone()).unwrap().value,
            3.0
        );
        for bad in ["nan", "NaN:0x1", "Infinity", ""] {
            r["value"] = serde_json::json!(bad);
            assert!(
                serde_json::from_value::<Reading>(r.clone()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn changed_at_is_optional_on_the_wire() {
        let r = Reading::new(
            t("2030-01-01T00:00:00Z"),
            "s",
            DeviceId::new("d").unwrap(),
            Metric::ENERGY,
            1.0,
            Unit::KilowattHour,
        );
        let json = serde_json::to_value(&r).unwrap();
        assert!(json.get("changed_at").is_none());
        let r = r.with_changed_at(7);
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["changed_at"], 7);
        assert_eq!(serde_json::from_value::<Reading>(json).unwrap(), r);
        let f = rate(
            "2030-01-01T16:00:00Z",
            "2030-01-02T00:00:00Z",
            "2030-01-02T00:30:00Z",
            0.2,
        )
        .with_changed_at(u64::MAX);
        let back: Forecast = serde_json::from_value(serde_json::to_value(&f).unwrap()).unwrap();
        assert_eq!(back.changed_at, Some(u64::MAX));
        assert_eq!(
            revision_time(t("2030-01-01T00:00:00Z")),
            1_893_456_000_000_000_000
        );
    }

    #[test]
    fn forecast_serde_names_and_time() {
        let f = rate(
            "2030-01-01T16:00:00Z",
            "2030-01-02T00:00:00Z",
            "2030-01-02T00:30:00Z",
            0.25,
        );
        let record = Record::from(f.clone());
        assert_eq!(record.time(), f.issued_at);
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["type"], "forecast");
        assert_eq!(json["data"]["basis"], "published");
        assert_eq!(json["data"]["unit"], "¤/kWh");
        assert_eq!(json["data"]["valid_to"], "2030-01-02T00:30:00Z");
        let back: Record = serde_json::from_value(json).unwrap();
        assert_eq!(back, record);
        assert_eq!(
            serde_json::to_string(&Basis::Modelled).unwrap(),
            "\"modelled\""
        );
        let env = Envelope::new("prices", record);
        assert_eq!(env.state_key(), None);
    }
}
