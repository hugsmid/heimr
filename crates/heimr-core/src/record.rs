//! What adapters emit: readings, device state and events.
//!
//! Provenance is the core's job: every record travels in an [`Envelope`]
//! whose `source` the core stamps, and an adapter's emitter rejects
//! readings and events that name anyone but the adapter instance, and events
//! of a kind reserved for the core (see [`Event::is_reserved_kind`]).

use std::fmt;

use jiff::Timestamp;
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

/// One sample, shaped like a row of the store's narrow `reading` table.
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
    /// Value in `unit`; must be finite.
    pub value: f64,
    /// Unit of `value`.
    pub unit: Unit,
    /// Quality flag.
    pub quality: Quality,
}

/// A reading failed validation at the adapter boundary.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ReadingError {
    /// The value was NaN or infinite.
    #[error("{metric} on {device}: value {value} is not finite")]
    NotFinite {
        /// Device.
        device: DeviceId,
        /// Metric.
        metric: Metric,
        /// Offending value.
        value: f64,
    },
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
        }
    }

    /// Replace the quality flag.
    #[must_use]
    pub fn with_quality(mut self, quality: Quality) -> Self {
        self.quality = quality;
        self
    }

    /// Replace the sample time.
    #[must_use]
    pub fn with_time(mut self, time: Timestamp) -> Self {
        self.time = time;
        self
    }

    /// Check invariants the store relies on (finite value).
    pub fn validate(&self) -> Result<(), ReadingError> {
        if self.value.is_finite() {
            Ok(())
        } else {
            Err(ReadingError::NotFinite {
                device: self.device.clone(),
                metric: self.metric.clone(),
                value: self.value,
            })
        }
    }
}

/// An entry for the store's `event` table: a command decision with its
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
    /// The record's own timestamp.
    pub fn time(&self) -> Timestamp {
        match self {
            Self::Reading(r) => r.time,
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
            Record::Reading(_) | Record::Event(_) => return None,
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
