//! Device classes: their observed state and the commands they accept.
//!
//! - **Climate** (heating zones): mode, target, measured temperature.
//! - **Fan** (air treatment: purifiers, fans, ventilation units): `Off`,
//!   `Auto`, `Manual(level)` in a level range the adapter declares, and
//!   adapter-declared named presets. Air-quality and filter figures are
//!   ordinary [`Reading`](crate::Reading)s with vendor-neutral
//!   [`Metric`](crate::Metric)s (`pm25`, `filter_life`, …).
//! - **Switch** (plugs, relays): on or off.
//!
//! Every command that changes a device's mode or setpoint carries a
//! [`Hold`]: it ends at the device's next schedule change or after a bounded
//! duration, so a device never stays overridden indefinitely if heimr stops.
//!
//! All types here are `#[non_exhaustive]`: build them with their
//! constructors, and match with a wildcard arm.

use std::fmt;

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use crate::id::{DeviceId, ZoneId};
use crate::units::Setpoint;

/// How long an override lasts.
///
/// Adapters must map a hold to the device's native timed or "until next
/// schedule block" override, so the device itself reverts even if heimr is
/// gone. They must never implement it as an open-ended override plus a timer
/// of their own. The [`Actuator`](crate::Actuator) refuses durations that are
/// not positive or exceed the policy's
/// [`max_hold`](crate::SafetyPolicy::max_hold).
///
/// ```
/// use heimr_core::Hold;
/// use jiff::SignedDuration;
/// let h = Hold::for_duration(SignedDuration::from_mins(90)).unwrap();
/// assert_eq!(h.duration(), Some(SignedDuration::from_mins(90)));
/// assert!(Hold::for_duration(SignedDuration::ZERO).is_err());
/// // A `Hold` value always satisfies the bound: the duration is opaque.
/// assert!(heimr_core::HoldDuration::new(SignedDuration::from_hours(25)).is_err());
/// assert_eq!(serde_json::to_value(Hold::NextScheduleChange).unwrap(), "next_schedule_change");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawHold", into = "RawHold")]
#[non_exhaustive]
pub enum Hold {
    /// Until the device's own schedule next changes block.
    NextScheduleChange,
    /// For a bounded duration, after which the device reverts to its
    /// schedule.
    For(HoldDuration),
}

/// The duration of a timed [`Hold`]: positive and at most
/// [`Hold::ABSOLUTE_MAX`], checked when built (also when deserialised).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "SignedDuration", into = "SignedDuration")]
pub struct HoldDuration(SignedDuration);

impl HoldDuration {
    /// `d`, which must be positive and at most [`Hold::ABSOLUTE_MAX`].
    pub fn new(d: SignedDuration) -> Result<Self, HoldError> {
        if d > SignedDuration::ZERO && d <= Hold::ABSOLUTE_MAX {
            Ok(Self(d))
        } else {
            Err(HoldError(d))
        }
    }

    /// The duration.
    pub fn get(self) -> SignedDuration {
        self.0
    }
}

impl TryFrom<SignedDuration> for HoldDuration {
    type Error = HoldError;
    fn try_from(d: SignedDuration) -> Result<Self, HoldError> {
        Self::new(d)
    }
}

impl From<HoldDuration> for SignedDuration {
    fn from(d: HoldDuration) -> SignedDuration {
        d.0
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum RawHold {
    NextScheduleChange,
    For(SignedDuration),
}

/// A [`Hold`] duration was not positive or exceeded [`Hold::ABSOLUTE_MAX`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("hold duration {0:#} must be positive and at most 24h")]
pub struct HoldError(pub SignedDuration);

impl Hold {
    /// Longest hold any policy can allow.
    pub const ABSOLUTE_MAX: SignedDuration = SignedDuration::from_hours(24);

    /// Until the next schedule change.
    pub fn until_next_schedule_change() -> Self {
        Self::NextScheduleChange
    }

    /// For `d`, which must be positive and at most [`Hold::ABSOLUTE_MAX`].
    pub fn for_duration(d: SignedDuration) -> Result<Self, HoldError> {
        HoldDuration::new(d).map(Self::For)
    }

    /// The duration, for a timed hold.
    pub fn duration(&self) -> Option<SignedDuration> {
        match self {
            Self::For(d) => Some(d.get()),
            Self::NextScheduleChange => None,
        }
    }
}

impl TryFrom<RawHold> for Hold {
    type Error = HoldError;
    fn try_from(r: RawHold) -> Result<Self, HoldError> {
        match r {
            RawHold::NextScheduleChange => Ok(Self::NextScheduleChange),
            RawHold::For(d) => Self::for_duration(d),
        }
    }
}

impl From<Hold> for RawHold {
    fn from(h: Hold) -> Self {
        match h {
            Hold::NextScheduleChange => Self::NextScheduleChange,
            Hold::For(d) => Self::For(d.get()),
        }
    }
}

impl fmt::Display for Hold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NextScheduleChange => f.write_str("until the next schedule change"),
            Self::For(d) => write!(f, "for {:#}", d.get()),
        }
    }
}

/// What a heating zone is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClimateMode {
    /// Following the device's own schedule (the fail-safe fallback).
    Schedule,
    /// Holding an overridden target.
    Manual,
    /// Heating off (the device may still apply frost protection).
    Off,
}

/// Observed state of a heating zone.
///
/// ```
/// use heimr_core::{ClimateMode, ClimateState, ZoneId};
/// let s = ClimateState::new(ZoneId::new("zone-a").unwrap(), jiff::Timestamp::UNIX_EPOCH, ClimateMode::Schedule)
///     .with_target_c(19.0)
///     .with_measured_c(18.4);
/// assert_eq!(s.target_c, Some(19.0));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ClimateState {
    /// Zone.
    pub zone: ZoneId,
    /// When observed (UTC).
    pub time: Timestamp,
    /// Mode.
    pub mode: ClimateMode,
    /// Target as reported by the device, °C. Not checked against any band:
    /// devices can report e.g. a frost-protection target.
    pub target_c: Option<f64>,
    /// Measured air temperature, °C.
    pub measured_c: Option<f64>,
    /// Heating demand, 0–100 %, if the device reports it.
    pub heating_pct: Option<f64>,
}

impl ClimateState {
    /// A state with only the mode known.
    pub fn new(zone: ZoneId, time: Timestamp, mode: ClimateMode) -> Self {
        Self {
            zone,
            time,
            mode,
            target_c: None,
            measured_c: None,
            heating_pct: None,
        }
    }

    /// Set the reported target.
    #[must_use]
    pub fn with_target_c(mut self, c: f64) -> Self {
        self.target_c = Some(c);
        self
    }

    /// Set the measured temperature.
    #[must_use]
    pub fn with_measured_c(mut self, c: f64) -> Self {
        self.measured_c = Some(c);
        self
    }

    /// Set the heating demand.
    #[must_use]
    pub fn with_heating_pct(mut self, pct: f64) -> Self {
        self.heating_pct = Some(pct);
        self
    }
}

/// A command to a heating zone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClimateCommand {
    /// Override the target temperature until the hold ends.
    #[non_exhaustive]
    SetTarget {
        /// Zone.
        zone: ZoneId,
        /// Target (re-checked against the policy band by the actuator).
        target: Setpoint,
        /// How long the override lasts.
        hold: Hold,
    },
    /// Return to the device's own schedule (the safe direction: never
    /// refused by a rate limit).
    #[non_exhaustive]
    ResumeSchedule {
        /// Zone.
        zone: ZoneId,
    },
    /// Turn heating off until the hold ends. Refused by the actuator unless
    /// the safety policy allows it.
    #[non_exhaustive]
    TurnOff {
        /// Zone.
        zone: ZoneId,
        /// How long heating stays off.
        hold: Hold,
    },
}

impl ClimateCommand {
    /// Override the target.
    pub fn set_target(zone: ZoneId, target: Setpoint, hold: Hold) -> Self {
        Self::SetTarget { zone, target, hold }
    }

    /// Return to the schedule.
    pub fn resume_schedule(zone: ZoneId) -> Self {
        Self::ResumeSchedule { zone }
    }

    /// Turn heating off.
    pub fn turn_off(zone: ZoneId, hold: Hold) -> Self {
        Self::TurnOff { zone, hold }
    }

    /// The zone addressed.
    pub fn zone(&self) -> &ZoneId {
        match self {
            Self::SetTarget { zone, .. }
            | Self::ResumeSchedule { zone, .. }
            | Self::TurnOff { zone, .. } => zone,
        }
    }
}

/// What an air-treatment device (purifier, fan, ventilation unit) is doing,
/// or should do.
///
/// Levels and presets are declared by the adapter in its
/// [`Capabilities`](crate::Capabilities); the actuator refuses others.
///
/// ```
/// use heimr_core::FanMode;
/// assert_eq!(serde_json::to_value(FanMode::Manual(3)).unwrap(), serde_json::json!({"mode": "manual", "value": 3}));
/// assert_eq!(serde_json::to_value(FanMode::preset("night")).unwrap(), serde_json::json!({"mode": "preset", "value": "night"}));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "mode", content = "value", rename_all = "snake_case")]
#[non_exhaustive]
pub enum FanMode {
    /// Off.
    Off,
    /// Device-controlled (e.g. from its own sensor).
    Auto,
    /// A fixed level within the adapter's declared range.
    Manual(u8),
    /// A named preset the adapter declares (e.g. `night`, `boost`).
    Preset(String),
}

impl FanMode {
    /// A named preset.
    pub fn preset(name: impl Into<String>) -> Self {
        Self::Preset(name.into())
    }
}

/// Observed state of an air-treatment device. Air quality, filter life and
/// the running fan level are emitted as readings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FanState {
    /// Device.
    pub device: DeviceId,
    /// When observed (UTC).
    pub time: Timestamp,
    /// Mode, if known.
    pub mode: Option<FanMode>,
}

impl FanState {
    /// A state with the mode unknown.
    pub fn new(device: DeviceId, time: Timestamp) -> Self {
        Self {
            device,
            time,
            mode: None,
        }
    }

    /// Set the mode.
    #[must_use]
    pub fn with_mode(mut self, mode: FanMode) -> Self {
        self.mode = Some(mode);
        self
    }
}

/// A command to an air-treatment device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
#[non_exhaustive]
pub enum FanCommand {
    /// Change mode until the hold ends.
    #[non_exhaustive]
    SetMode {
        /// Device.
        device: DeviceId,
        /// New mode.
        mode: FanMode,
        /// How long it lasts.
        hold: Hold,
    },
}

impl FanCommand {
    /// Change mode.
    pub fn set_mode(device: DeviceId, mode: FanMode, hold: Hold) -> Self {
        Self::SetMode { device, mode, hold }
    }
}

/// Observed state of a switch or smart plug.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SwitchState {
    /// Device.
    pub device: DeviceId,
    /// When observed (UTC).
    pub time: Timestamp,
    /// Whether it is on.
    pub on: bool,
}

impl SwitchState {
    /// A state.
    pub fn new(device: DeviceId, time: Timestamp, on: bool) -> Self {
        Self { device, time, on }
    }
}

/// A command to a switch or smart plug. Switches have no hold: most have no
/// native timed override. Turning off a switch declared heating-critical is
/// governed by the heating-off rule of the safety policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SwitchCommand {
    /// Turn on or off.
    #[non_exhaustive]
    Set {
        /// Device.
        device: DeviceId,
        /// Desired state.
        on: bool,
    },
}

impl SwitchCommand {
    /// Turn on or off.
    pub fn set(device: DeviceId, on: bool) -> Self {
        Self::Set { device, on }
    }
}

/// Any command an adapter can be asked to carry out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "domain", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Command {
    /// Heating.
    Climate(ClimateCommand),
    /// Air treatment.
    Fan(FanCommand),
    /// Switch or plug.
    Switch(SwitchCommand),
}

/// The kind of a [`Command`], used to declare and check write capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CommandKind {
    /// [`ClimateCommand::SetTarget`].
    ClimateTarget,
    /// [`ClimateCommand::ResumeSchedule`].
    ClimateSchedule,
    /// [`ClimateCommand::TurnOff`].
    ClimateOff,
    /// [`FanCommand::SetMode`].
    FanMode,
    /// [`SwitchCommand::Set`].
    Switch,
}

impl CommandKind {
    /// Every kind, in declaration order.
    pub const ALL: &'static [Self] = &[
        Self::ClimateTarget,
        Self::ClimateSchedule,
        Self::ClimateOff,
        Self::FanMode,
        Self::Switch,
    ];

    /// The kind's name, as serialised.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClimateTarget => "climate_target",
            Self::ClimateSchedule => "climate_schedule",
            Self::ClimateOff => "climate_off",
            Self::FanMode => "fan_mode",
            Self::Switch => "switch",
        }
    }
}

impl fmt::Display for CommandKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Command {
    /// The command's kind.
    pub fn kind(&self) -> CommandKind {
        match self {
            Self::Climate(ClimateCommand::SetTarget { .. }) => CommandKind::ClimateTarget,
            Self::Climate(ClimateCommand::ResumeSchedule { .. }) => CommandKind::ClimateSchedule,
            Self::Climate(ClimateCommand::TurnOff { .. }) => CommandKind::ClimateOff,
            Self::Fan(FanCommand::SetMode { .. }) => CommandKind::FanMode,
            Self::Switch(SwitchCommand::Set { .. }) => CommandKind::Switch,
        }
    }

    /// The zone or device the command addresses, within its adapter
    /// instance (the per-target rate-limit key).
    pub fn target(&self) -> &str {
        match self {
            Self::Climate(c) => c.zone().as_str(),
            Self::Fan(FanCommand::SetMode { device, .. })
            | Self::Switch(SwitchCommand::Set { device, .. }) => device.as_str(),
        }
    }

    /// The hold, for commands that override a device.
    pub fn hold(&self) -> Option<Hold> {
        match self {
            Self::Climate(
                ClimateCommand::SetTarget { hold, .. } | ClimateCommand::TurnOff { hold, .. },
            )
            | Self::Fan(FanCommand::SetMode { hold, .. }) => Some(*hold),
            Self::Climate(ClimateCommand::ResumeSchedule { .. }) | Self::Switch(_) => None,
        }
    }
}

impl From<ClimateCommand> for Command {
    fn from(c: ClimateCommand) -> Self {
        Self::Climate(c)
    }
}
impl From<FanCommand> for Command {
    fn from(c: FanCommand) -> Self {
        Self::Fan(c)
    }
}
impl From<SwitchCommand> for Command {
    fn from(c: SwitchCommand) -> Self {
        Self::Switch(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone() -> ZoneId {
        ZoneId::new("zone-a").unwrap()
    }

    #[test]
    fn command_json_shape() {
        let cmd = Command::from(ClimateCommand::set_target(
            zone(),
            Setpoint::new(19.5).unwrap(),
            Hold::for_duration(SignedDuration::from_hours(2)).unwrap(),
        ));
        let json = serde_json::to_value(&cmd).unwrap();
        assert_eq!(json["domain"], "climate");
        assert_eq!(json["action"], "set_target");
        assert_eq!(json["zone"], "zone-a");
        assert_eq!(json["target"], 19.5);
        assert!(json["hold"].get("for").is_some(), "{json}");
        assert_eq!(
            serde_json::from_value::<Command>(json.clone()).unwrap(),
            cmd
        );

        // A hold is required, and validated on the way in.
        let mut no_hold = json.clone();
        no_hold.as_object_mut().unwrap().remove("hold");
        assert!(serde_json::from_value::<Command>(no_hold).is_err());
        let mut long = json.clone();
        long["hold"] = serde_json::to_value(RawHold::For(SignedDuration::from_hours(25))).unwrap();
        assert!(serde_json::from_value::<Command>(long).is_err());
        let mut zero = json.clone();
        zero["hold"] = serde_json::to_value(RawHold::For(SignedDuration::ZERO)).unwrap();
        assert!(serde_json::from_value::<Command>(zero).is_err());

        // Setpoints outside the envelope cannot be deserialised at all.
        let mut hot = json;
        hot["target"] = serde_json::json!(35.0);
        assert!(serde_json::from_value::<Command>(hot).is_err());
    }

    #[test]
    fn kinds_targets_holds() {
        let dev = DeviceId::new("plug-1").unwrap();
        let cmds: Vec<Command> = vec![
            ClimateCommand::set_target(
                zone(),
                Setpoint::new(20.0).unwrap(),
                Hold::NextScheduleChange,
            )
            .into(),
            ClimateCommand::resume_schedule(zone()).into(),
            ClimateCommand::turn_off(zone(), Hold::NextScheduleChange).into(),
            FanCommand::set_mode(dev.clone(), FanMode::Auto, Hold::NextScheduleChange).into(),
            SwitchCommand::set(dev, false).into(),
        ];
        let kinds: Vec<_> = cmds.iter().map(Command::kind).collect();
        assert_eq!(kinds, CommandKind::ALL);
        let holds: Vec<_> = cmds.iter().map(|c| c.hold().is_some()).collect();
        assert_eq!(holds, [true, false, true, true, false]);
        assert_eq!(cmds[4].target(), "plug-1");
        for k in CommandKind::ALL {
            assert_eq!(serde_json::to_value(k).unwrap(), k.as_str());
        }
    }
}
