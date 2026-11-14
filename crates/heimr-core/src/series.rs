//! Series templates: how each metric an adapter kind emits will become a
//! stored series.
//!
//! The store, when it is added, will keep one series per
//! `<instance>/<device>/<metric>`, created from the template the first
//! time a device emits the metric. Today an adapter kind declares, on its
//! [descriptor](crate::AdapterDescriptor), one [`SeriesTemplate`] per
//! metric it emits as readings, and one per metric it emits as forecasts
//! (a [`SeriesRole`] other than [`Measured`](SeriesRole::Measured)), and
//! the emitter refuses a reading or forecast whose metric has no template,
//! or whose unit differs from it.
//!
//! Parts marked *provisional* may change when the store is added.
//!
//! **A template is fixed for the life of its series.** A change of kind,
//! value type, unit or `versioned` is a new series (and so a new metric
//! name), never an edit of the old one. The intervals
//! (`expected_interval`, `staleness_limit`, `lateness_bound`) are
//! configuration and may change.
//!
//! ```
//! use heimr_core::{Metric, SeriesTemplate, TimeSource, Unit};
//! use jiff::SignedDuration;
//!
//! // A sensor polled every 30 s, stamped with the host clock.
//! let t = SeriesTemplate::gauge(Metric::TEMPERATURE, Unit::Celsius, SignedDuration::from_secs(30));
//! assert_eq!(t.t_source, TimeSource::Host);
//! assert_eq!(t.staleness_limit(), SignedDuration::from_secs(90)); // 3 × interval by default
//! t.validate().unwrap();
//!
//! // A door contact that reports on change, with a keep-alive every 15 min.
//! let door = SeriesTemplate::gauge(Metric::new("open").unwrap(), Unit::Dimensionless, SignedDuration::from_mins(15))
//!     .with_t_source(TimeSource::Device)
//!     .record_on_change();
//! door.validate().unwrap();
//! ```

use jiff::SignedDuration;
use serde::{Deserialize, Serialize};

use crate::units::{Metric, Unit};

/// How long a late reading is still expected by default: 5 minutes.
pub const DEFAULT_LATENESS_BOUND: SignedDuration = SignedDuration::from_mins(5);

/// The default staleness limit is this many expected intervals.
pub const DEFAULT_STALENESS_INTERVALS: i32 = 3;

/// What a series is about: measured values, or values for a time that
/// were known in advance.
///
/// *Provisional:* may change when the store is added.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SeriesRole {
    /// Measured or sampled values: [`Reading`](crate::Reading)s.
    #[default]
    Measured,
    /// Predictions: [`Forecast`](crate::Forecast)s with
    /// [`Basis::Modelled`](crate::Basis::Modelled), such as weather. Keyed
    /// by valid time, versioned with `changed_at` = issue time, every issue
    /// kept; not treated as future-dated or suspect, and not live state.
    Forecast,
    /// Values the source has committed to for a coming time:
    /// [`Forecast`](crate::Forecast)s with
    /// [`Basis::Published`](crate::Basis::Published), such as a published
    /// price. Known in advance, live from their time, never flagged future
    /// or suspect.
    Scheduled,
}

impl SeriesRole {
    /// `measured`, `forecast` or `scheduled`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::Forecast => "forecast",
            Self::Scheduled => "scheduled",
        }
    }
}

/// How long the store will keep a series' detail.
///
/// *Provisional:* may change when the store is added.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Retention {
    /// Kept for the long term (the default).
    #[default]
    LongTerm,
    /// Diagnostics, such as host metrics: kept for a shorter time.
    Diagnostic,
}

/// What a series holds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SeriesKind {
    /// A value sampled at an instant (a temperature, a price).
    Gauge,
    /// A cumulative, non-negative total (an energy meter, an error count).
    /// A negative value is recorded as bad quality.
    Counter {
        /// The value at which the counter wraps back to zero, if it does.
        wrap_at: Option<f64>,
    },
    /// The amount over a fixed interval of `length` (rainfall per hour,
    /// consumption per half-hour). A reading is stamped at the interval's
    /// start, aligned to `length` (on UTC nanoseconds), and its value is
    /// the amount for `[t, t + length)`. Sums over a window are meaningful;
    /// averages and integrals are not, and heimr does not rely on them. Not
    /// record-on-change. Variable-length intervals (local days, months) are
    /// not covered.
    ///
    /// *Provisional:* may change when the store is added.
    Interval {
        /// The interval length; positive.
        length: SignedDuration,
    },
    /// A named or boolean state.
    State,
    /// Text of at most 255 bytes.
    Text,
}

/// Where a reading's time comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TimeSource {
    /// The source stamps it: a device's clock, a slot or hour in an API
    /// response. Accepted at once.
    Device,
    /// The host's clock stamps it when sampled. Held back until the host
    /// clock is synchronised.
    Host,
}

/// How one metric of an adapter kind will map to a stored series. See the
/// [module docs](self).
///
/// Build one with [`SeriesTemplate::gauge`], [`SeriesTemplate::counter`],
/// [`SeriesTemplate::new`], [`SeriesTemplate::forecast`] or
/// [`SeriesTemplate::scheduled`], then adjust it
/// with the `with_*` methods, [`SeriesTemplate::versioned()`] and
/// [`SeriesTemplate::record_on_change()`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SeriesTemplate {
    /// The metric this template is for.
    pub metric: Metric,
    /// Measured (readings), forecast or scheduled (forecast records).
    /// A kind may declare one measured template and one other for the same
    /// metric. The emitter requires a [`Basis::Modelled`](crate::Basis)
    /// forecast to use a [`SeriesRole::Forecast`] template and a
    /// [`Basis::Published`](crate::Basis) one a [`SeriesRole::Scheduled`]
    /// template.
    ///
    /// *Provisional:* may change when the store is added; forecast and
    /// scheduled series are the least settled.
    #[serde(default)]
    pub role: SeriesRole,
    /// What the series holds.
    pub kind: SeriesKind,
    /// The unit of every value; fixed for life.
    pub unit: Unit,
    /// How often a value is expected. For a record-on-change series this
    /// is the keep-alive interval (the longest the source stays silent
    /// while the value is unchanged), not a poll interval.
    pub expected_interval: SignedDuration,
    /// Where `t` comes from.
    pub t_source: TimeSource,
    /// Whether the source revises published values: each reading or
    /// forecast then carries `changed_at`, the source's revision time or
    /// version, and the one with the highest `changed_at` is current.
    /// Fixed for life.
    pub versioned: bool,
    /// Whether the source sends only changes. It must then re-send an
    /// unchanged value at least once per `expected_interval` as a
    /// keep-alive, or the series reads as stale.
    pub record_on_change: bool,
    /// How long without a value before the series counts as stale, if
    /// not the default ([`DEFAULT_STALENESS_INTERVALS`] ×
    /// `expected_interval`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staleness_limit: Option<SignedDuration>,
    /// How late a value may still arrive, if not
    /// [`DEFAULT_LATENESS_BOUND`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lateness_bound: Option<SignedDuration>,
    /// How long the store will keep the detail (*provisional*).
    #[serde(default)]
    pub retention: Retention,
}

impl SeriesTemplate {
    /// A template for readings of `metric`: host-stamped, not versioned,
    /// sampled (not record-on-change).
    pub fn new(
        metric: Metric,
        kind: SeriesKind,
        unit: Unit,
        expected_interval: SignedDuration,
    ) -> Self {
        Self {
            metric,
            role: SeriesRole::Measured,
            kind,
            unit,
            expected_interval,
            t_source: TimeSource::Host,
            versioned: false,
            record_on_change: false,
            staleness_limit: None,
            lateness_bound: None,
            retention: Retention::LongTerm,
        }
    }

    /// A [`SeriesKind::Gauge`] for readings.
    pub fn gauge(metric: Metric, unit: Unit, expected_interval: SignedDuration) -> Self {
        Self::new(metric, SeriesKind::Gauge, unit, expected_interval)
    }

    /// A [`SeriesKind::Interval`] for readings: the amount over each
    /// `length`, expected every `expected_interval`.
    pub fn interval(
        metric: Metric,
        unit: Unit,
        length: SignedDuration,
        expected_interval: SignedDuration,
    ) -> Self {
        Self::new(
            metric,
            SeriesKind::Interval { length },
            unit,
            expected_interval,
        )
    }

    /// The interval length, for a [`SeriesKind::Interval`] series.
    pub fn interval_length(&self) -> Option<SignedDuration> {
        match self.kind {
            SeriesKind::Interval { length } => Some(length),
            _ => None,
        }
    }

    /// Whether `t` starts an interval of this [`SeriesKind::Interval`]
    /// series (a multiple of its length since the Unix epoch, in UTC
    /// nanoseconds). Always true for other kinds.
    pub fn is_aligned(&self, t: jiff::Timestamp) -> bool {
        match self.interval_length() {
            Some(d) if d > SignedDuration::ZERO => t.as_nanosecond().rem_euclid(d.as_nanos()) == 0,
            _ => true,
        }
    }

    /// A [`SeriesKind::Counter`] for readings, wrapping at `wrap_at` if
    /// given.
    pub fn counter(
        metric: Metric,
        unit: Unit,
        expected_interval: SignedDuration,
        wrap_at: Option<f64>,
    ) -> Self {
        Self::new(
            metric,
            SeriesKind::Counter { wrap_at },
            unit,
            expected_interval,
        )
    }

    /// A [`SeriesRole::Forecast`] template for modelled
    /// [`Forecast`](crate::Forecast)s of `metric`: a versioned gauge keyed
    /// by valid time, stamped by the source, re-issued every
    /// `expected_interval`.
    pub fn forecast(metric: Metric, unit: Unit, expected_interval: SignedDuration) -> Self {
        Self {
            role: SeriesRole::Forecast,
            t_source: TimeSource::Device,
            versioned: true,
            ..Self::gauge(metric, unit, expected_interval)
        }
    }

    /// A [`SeriesRole::Scheduled`] template for published
    /// [`Forecast`](crate::Forecast)s of `metric` (a gauge keyed by valid
    /// time, stamped by the source). Mark it [`versioned`](Self::versioned())
    /// if the source corrects published values.
    pub fn scheduled(metric: Metric, unit: Unit, expected_interval: SignedDuration) -> Self {
        Self {
            role: SeriesRole::Scheduled,
            t_source: TimeSource::Device,
            ..Self::gauge(metric, unit, expected_interval)
        }
    }

    /// Whether it describes forecast records (any role but
    /// [`SeriesRole::Measured`]) rather than readings.
    pub fn for_forecasts(&self) -> bool {
        self.role != SeriesRole::Measured
    }

    /// Set the retention class (default [`Retention::LongTerm`]).
    #[must_use]
    pub fn with_retention(mut self, retention: Retention) -> Self {
        self.retention = retention;
        self
    }

    /// Set where `t` comes from (default [`TimeSource::Host`] for
    /// readings, [`TimeSource::Device`] for forecasts).
    #[must_use]
    pub fn with_t_source(mut self, t_source: TimeSource) -> Self {
        self.t_source = t_source;
        self
    }

    /// Mark the series versioned: its source revises published values.
    #[must_use]
    pub fn versioned(mut self) -> Self {
        self.versioned = true;
        self
    }

    /// Mark the series record-on-change; `expected_interval` becomes the
    /// keep-alive interval. Not for interval or forecast series:
    /// [`SeriesTemplate::validate`] refuses those.
    #[must_use]
    pub fn record_on_change(mut self) -> Self {
        self.record_on_change = true;
        self
    }

    /// Set how often a value is expected: for an instance whose
    /// configuration sets its poll interval ([`Adapter::series`](crate::Adapter::series)).
    #[must_use]
    pub fn with_expected_interval(mut self, interval: SignedDuration) -> Self {
        self.expected_interval = interval;
        self
    }

    /// Set the staleness limit (default 3 × `expected_interval`).
    #[must_use]
    pub fn with_staleness_limit(mut self, limit: SignedDuration) -> Self {
        self.staleness_limit = Some(limit);
        self
    }

    /// Set the lateness bound (default [`DEFAULT_LATENESS_BOUND`]).
    #[must_use]
    pub fn with_lateness_bound(mut self, bound: SignedDuration) -> Self {
        self.lateness_bound = Some(bound);
        self
    }

    /// The staleness limit in force: the declared one, or 3 ×
    /// `expected_interval`.
    pub fn staleness_limit(&self) -> SignedDuration {
        self.staleness_limit.unwrap_or_else(|| {
            self.expected_interval
                .checked_mul(DEFAULT_STALENESS_INTERVALS)
                .unwrap_or(SignedDuration::MAX)
        })
    }

    /// The lateness bound in force.
    pub fn lateness_bound(&self) -> SignedDuration {
        self.lateness_bound.unwrap_or(DEFAULT_LATENESS_BOUND)
    }

    /// `reading <metric>`, `forecast <metric>` or `scheduled <metric>`,
    /// for messages.
    pub fn label(&self) -> String {
        let role = match self.role {
            SeriesRole::Measured => "reading",
            SeriesRole::Forecast => "forecast",
            SeriesRole::Scheduled => "scheduled",
        };
        format!("{role} {}", self.metric)
    }

    /// Check the template on its own: a positive expected interval; a
    /// staleness limit no shorter than it; a non-negative lateness bound;
    /// a counter's `wrap_at` finite and positive; a forecast template
    /// versioned; only a measured series record-on-change.
    pub fn validate(&self) -> Result<(), SeriesError> {
        let problem = |what: &str| {
            Err(SeriesError {
                series: self.label(),
                problem: what.to_owned(),
            })
        };
        if self.expected_interval <= SignedDuration::ZERO {
            return problem("expected_interval must be positive");
        }
        if self
            .staleness_limit
            .is_some_and(|s| s < self.expected_interval)
        {
            return problem("staleness_limit must be at least expected_interval");
        }
        if self
            .lateness_bound
            .is_some_and(|b| b < SignedDuration::ZERO)
        {
            return problem("lateness_bound must not be negative");
        }
        if let SeriesKind::Counter {
            wrap_at: Some(w), ..
        } = self.kind
            && !(w.is_finite() && w > 0.0)
        {
            return problem("a counter's wrap_at must be finite and positive");
        }
        if let SeriesKind::Interval { length } = self.kind {
            if length <= SignedDuration::ZERO {
                return problem("an interval length must be positive");
            }
            if self.record_on_change {
                return problem("record-on-change does not apply to an interval series");
            }
        }
        if self.role == SeriesRole::Forecast && !self.versioned {
            return problem("a forecast series is versioned");
        }
        if self.for_forecasts() && self.record_on_change {
            return problem("only a measured series can be record-on-change");
        }
        Ok(())
    }
}

/// A [`SeriesTemplate`] is inconsistent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("series template for {series}: {problem}")]
#[non_exhaustive]
pub struct SeriesError {
    /// `reading <metric>` or `forecast <metric>`.
    pub series: String,
    /// What is wrong.
    pub problem: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(s: &str) -> Metric {
        Metric::new(s).unwrap()
    }

    #[test]
    fn defaults() {
        let g = SeriesTemplate::gauge(m("x"), Unit::Watt, SignedDuration::from_secs(10));
        assert!(!g.for_forecasts() && !g.versioned && !g.record_on_change);
        assert_eq!(
            (g.role, g.retention),
            (SeriesRole::Measured, Retention::LongTerm)
        );
        assert_eq!(g.t_source, TimeSource::Host);
        assert_eq!(g.staleness_limit(), SignedDuration::from_secs(30));
        assert_eq!(g.lateness_bound(), SignedDuration::from_mins(5));
        let g = g
            .with_staleness_limit(SignedDuration::from_mins(2))
            .with_lateness_bound(SignedDuration::ZERO);
        assert_eq!(g.staleness_limit(), SignedDuration::from_mins(2));
        assert_eq!(g.lateness_bound(), SignedDuration::ZERO);
        let f = SeriesTemplate::forecast(m("x"), Unit::Watt, SignedDuration::from_hours(1));
        assert!(f.for_forecasts() && f.versioned && !f.record_on_change);
        assert_eq!(f.role, SeriesRole::Forecast);
        let p = SeriesTemplate::scheduled(m("x"), Unit::Watt, SignedDuration::from_hours(1));
        assert_eq!(
            (p.role, p.versioned, p.t_source),
            (SeriesRole::Scheduled, false, TimeSource::Device)
        );
        assert_eq!(p.label(), "scheduled x");
        p.validate().unwrap();
        p.clone().versioned().validate().unwrap();
        let d = g.clone().with_retention(Retention::Diagnostic);
        assert_eq!(d.retention, Retention::Diagnostic);
        assert_eq!(f.t_source, TimeSource::Device);
        assert_eq!(f.label(), "forecast x");
        f.validate().unwrap();
        // A huge interval saturates rather than overflowing.
        let long = SeriesTemplate::gauge(m("x"), Unit::Watt, SignedDuration::MAX);
        assert_eq!(long.staleness_limit(), SignedDuration::MAX);
    }

    #[test]
    fn validation_names_the_series_and_the_rule() {
        let hour = SignedDuration::from_hours(1);
        let err = |t: SeriesTemplate| t.validate().unwrap_err().to_string();
        assert_eq!(
            err(SeriesTemplate::gauge(
                m("x"),
                Unit::Watt,
                SignedDuration::ZERO
            )),
            "series template for reading x: expected_interval must be positive"
        );
        assert!(
            err(SeriesTemplate::gauge(m("x"), Unit::Watt, hour)
                .with_staleness_limit(SignedDuration::from_mins(59)))
            .ends_with("staleness_limit must be at least expected_interval")
        );
        assert!(
            err(SeriesTemplate::gauge(m("x"), Unit::Watt, hour)
                .with_lateness_bound(SignedDuration::from_secs(-1)))
            .ends_with("lateness_bound must not be negative")
        );
        for w in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(
                err(SeriesTemplate::counter(
                    m("x"),
                    Unit::WattHour,
                    hour,
                    Some(w)
                ))
                .ends_with("wrap_at must be finite and positive"),
                "{w}"
            );
        }
        SeriesTemplate::counter(m("x"), Unit::WattHour, hour, Some(65536.0))
            .validate()
            .unwrap();
        let mut f = SeriesTemplate::forecast(m("x"), Unit::Watt, hour);
        f.versioned = false;
        assert_eq!(
            err(f),
            "series template for forecast x: a forecast series is versioned"
        );
        assert!(
            err(SeriesTemplate::forecast(m("x"), Unit::Watt, hour).record_on_change())
                .ends_with("only a measured series can be record-on-change")
        );
        assert!(
            err(SeriesTemplate::scheduled(m("x"), Unit::Watt, hour).record_on_change())
                .ends_with("only a measured series can be record-on-change")
        );
    }

    #[test]
    fn interval_series() {
        let hour = SignedDuration::from_hours(1);
        let rain = SeriesTemplate::interval(
            m("rain"),
            Unit::Millimetre,
            hour,
            SignedDuration::from_hours(3),
        );
        rain.validate().unwrap();
        assert_eq!(rain.interval_length(), Some(hour));
        let t = |s: &str| s.parse::<jiff::Timestamp>().unwrap();
        assert!(rain.is_aligned(t("2030-01-01T05:00:00Z")));
        assert!(!rain.is_aligned(t("2030-01-01T05:00:00.000000001Z")));
        assert!(
            rain.is_aligned(t("1969-12-31T23:00:00Z")),
            "before the epoch too"
        );
        assert!(!rain.is_aligned(t("1969-12-31T23:30:00Z")));
        let g = SeriesTemplate::gauge(m("x"), Unit::Watt, hour);
        assert!(g.is_aligned(t("2030-01-01T05:00:01Z")) && g.interval_length().is_none());
        let err = |t: SeriesTemplate| t.validate().unwrap_err().to_string();
        assert_eq!(
            err(rain.clone().record_on_change()),
            "series template for reading rain: record-on-change does not apply to an interval series"
        );
        assert!(
            err(SeriesTemplate::interval(
                m("rain"),
                Unit::Millimetre,
                SignedDuration::ZERO,
                hour
            ))
            .ends_with("an interval length must be positive")
        );
        let json = serde_json::to_value(&rain).unwrap();
        assert_eq!(json["kind"]["interval"]["length"], "PT1H");
        assert_eq!(
            serde_json::from_value::<SeriesTemplate>(json).unwrap(),
            rain
        );
    }

    #[test]
    fn serde_shape() {
        let t = SeriesTemplate::counter(
            m("energy"),
            Unit::KilowattHour,
            SignedDuration::from_secs(10),
            Some(1e6),
        )
        .with_t_source(TimeSource::Device);
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json["kind"]["counter"]["wrap_at"], 1e6);
        assert_eq!(json["t_source"], "device");
        assert_eq!(json["unit"], "kWh");
        assert!(json.get("staleness_limit").is_none());
        assert_eq!(json["role"], "measured");
        assert_eq!(json["retention"], "long_term");
        let back: SeriesTemplate = serde_json::from_value(json).unwrap();
        assert_eq!(back, t);
    }
}
