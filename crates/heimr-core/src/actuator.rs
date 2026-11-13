//! The safety layer between deciders (planner, assistant, user) and adapters.
//!
//! Every command goes through a [`CommandSubmitter`] (a submit-only handle
//! bound to one actor when it is created), which, in order:
//! 1. gives it a [`CommandId`] that is unique across restarts;
//! 2. checks the instance is registered and declares the command's kind;
//! 3. enforces the instance's [`SafetyPolicy`] (the global policy, possibly
//!    narrowed per instance): setpoint band, hold duration, fan levels and
//!    presets, and the heating-off rule (which also covers switching off a
//!    device declared heating-critical);
//! 4. enforces rate limits ([`RateLimiter`]) on monotonic time — except for
//!    [`ClimateCommand::ResumeSchedule`], the safe direction, which no rate
//!    limit ever refuses;
//! 5. writes an [`AuditRecord`] and waits until it is durable
//!    ([`AuditLog::append`], with a deadline) — rejections are audited too,
//!    and a command whose record is not durable in time is withheld;
//! 6. delivers it only if the instance is `live` and running. In
//!    [`Mode::Shadow`] (the default) there is no queue to deliver to: the
//!    outcome is [`Outcome::Shadowed`] and the adapter never sees it.
//!
//! Shadowed commands count against rate limits, so shadow mode rehearses
//! exactly what live mode would do.
//!
//! A submission runs to completion even if the caller stops waiting for it
//! (it is driven by its own task): a command whose decision was audited is
//! either queued or audited as dropped, never silently lost.
//!
//! # Restarts
//! At start-up ([`ActuatorBuilder::build`]) the actuator reads the audit
//! trail's last hour to rebuild rate-limit history, and picks a boot number
//! above every id already issued. Entries the trail cannot read (torn,
//! unparsable or of an unknown format version) are counted as accepted
//! commands, against every instance and target they might belong to, so an
//! unreadable entry never under-counts. With the boot hold-off (on by
//! default; [`ActuatorBuilder::without_boot_holdoff`] opts out), no
//! non-exempt command to any target is accepted for the per-target minimum
//! interval after start, so a crash loop cannot exceed the per-target limit
//! even if the trail is unreadable or the wall clock is wrong.
//!
//! If the trail is missing altogether (deleted, or a fresh state
//! directory), the hourly-cap history is lost: after such a start the boot
//! hold-off is the only limit until new history builds up. History older
//! than an hour by the wall clock is ignored, so a forward wall-clock step
//! across a restart can under-count the hourly cap (by at most the step);
//! within a run, limits use monotonic time and wall steps change nothing.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use jiff::SignedDuration;
use serde::{Deserialize, Serialize};

use crate::adapter::{Capabilities, DescriptorError};
use crate::audit::{AuditError, AuditKind, AuditLog, AuditRecord, AuditResult};
use crate::clock::SharedClock;
use crate::context::{CommandEnvelope, Commands, Emitter, Mailbox, Reservation, ReserveError};
use crate::domain::{
    ClimateCommand, Command, CommandKind, FanCommand, FanMode, Hold, SwitchCommand,
};
use crate::id::{self, CommandId, CommandIdError, CommandIds, DeviceId, IdError};
use crate::record::Record;
use crate::telemetry::StatsHandle;
use crate::units::{Setpoint, SetpointBand, SetpointError};

/// The `actor` of audit records the actuator writes on its own behalf
/// (drops), and the source of its sink copies.
pub const ACTUATOR: &str = "actuator";

/// Whether an instance's commands are delivered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Mode {
    /// Log commands, never deliver them (the default).
    #[default]
    Shadow,
    /// Deliver commands. Must be opted into per instance.
    Live,
}

impl Mode {
    /// `"shadow"` or `"live"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shadow => "shadow",
            Self::Live => "live",
        }
    }
}

/// An invalid [`SafetyPolicy`] or [`InstanceSafety`].
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum PolicyError {
    /// The setpoint band is invalid.
    #[error(transparent)]
    Band(#[from] SetpointError),
    /// The minimum command interval was negative.
    #[error("minimum command interval must not be negative")]
    NegativeInterval,
    /// The maximum hold was not positive or exceeded [`Hold::ABSOLUTE_MAX`].
    #[error("maximum hold must be positive and at most 24h")]
    InvalidMaxHold,
    /// A per-instance setting would widen the global policy.
    #[error("{0} may only narrow the global safety policy, not widen it")]
    Widens(&'static str),
}

/// Limits every command must satisfy.
///
/// ```
/// use heimr_core::{SafetyPolicy, SetpointBand};
/// use jiff::SignedDuration;
/// let p = SafetyPolicy::default()
///     .with_setpoint(SetpointBand::new(16.0, 21.0).unwrap())
///     .with_max_hold(SignedDuration::from_hours(1));
/// p.validate().unwrap();
/// assert!(p.setpoint(21.5).is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SafetyPolicy {
    /// Allowed setpoints.
    pub setpoint: SetpointBand,
    /// Minimum time between accepted commands to the same zone/device.
    pub min_interval: SignedDuration,
    /// Maximum accepted commands per instance in any rolling hour.
    pub max_per_hour: u32,
    /// Longest [`Hold::For`] accepted.
    pub max_hold: SignedDuration,
    /// Whether heating may be turned off: [`ClimateCommand::TurnOff`], and
    /// switching off a device declared heating-critical.
    pub allow_heating_off: bool,
}

impl Default for SafetyPolicy {
    /// The conservative default band (15.0..=24.0 °C), 5 minutes per target,
    /// 12 per instance per hour, holds of at most 2 hours, heating may not
    /// be turned off.
    fn default() -> Self {
        Self {
            setpoint: SetpointBand::DEFAULT,
            min_interval: SignedDuration::from_mins(5),
            max_per_hour: 12,
            max_hold: SignedDuration::from_hours(2),
            allow_heating_off: false,
        }
    }
}

impl SafetyPolicy {
    /// Replace the setpoint band.
    #[must_use]
    pub fn with_setpoint(mut self, band: SetpointBand) -> Self {
        self.setpoint = band;
        self
    }

    /// Replace the per-target minimum interval.
    #[must_use]
    pub fn with_min_interval(mut self, d: SignedDuration) -> Self {
        self.min_interval = d;
        self
    }

    /// Replace the hourly cap.
    #[must_use]
    pub fn with_max_per_hour(mut self, n: u32) -> Self {
        self.max_per_hour = n;
        self
    }

    /// Replace the maximum hold.
    #[must_use]
    pub fn with_max_hold(mut self, d: SignedDuration) -> Self {
        self.max_hold = d;
        self
    }

    /// Allow or forbid turning heating off.
    #[must_use]
    pub fn with_heating_off(mut self, allow: bool) -> Self {
        self.allow_heating_off = allow;
        self
    }

    /// Check internal consistency.
    pub fn validate(&self) -> Result<(), PolicyError> {
        if self.min_interval < SignedDuration::ZERO {
            return Err(PolicyError::NegativeInterval);
        }
        if self.max_hold <= SignedDuration::ZERO || self.max_hold > Hold::ABSOLUTE_MAX {
            return Err(PolicyError::InvalidMaxHold);
        }
        Ok(())
    }

    /// A setpoint inside this policy's band.
    pub fn setpoint(&self, value: f64) -> Result<Setpoint, SetpointError> {
        self.setpoint.setpoint(value)
    }

    /// This policy narrowed by `instance`. Any setting that would widen it
    /// is an error.
    pub fn narrowed(&self, instance: &InstanceSafety) -> Result<Self, PolicyError> {
        let mut p = self.clone();
        if let Some(band) = instance.setpoint {
            if !self.setpoint.includes(&band) {
                return Err(PolicyError::Widens("setpoint band"));
            }
            p.setpoint = band;
        }
        if let Some(d) = instance.min_interval {
            if d < self.min_interval {
                return Err(PolicyError::Widens("min_command_interval"));
            }
            p.min_interval = d;
        }
        if let Some(n) = instance.max_per_hour {
            if n > self.max_per_hour {
                return Err(PolicyError::Widens("max_commands_per_hour"));
            }
            p.max_per_hour = n;
        }
        if let Some(d) = instance.max_hold {
            if d > self.max_hold {
                return Err(PolicyError::Widens("max_hold"));
            }
            p.max_hold = d;
        }
        if let Some(allow) = instance.allow_heating_off {
            if allow && !self.allow_heating_off {
                return Err(PolicyError::Widens("allow_heating_off"));
            }
            p.allow_heating_off = allow;
        }
        p.validate()?;
        Ok(p)
    }
}

/// Per-instance safety settings: may only narrow the global
/// [`SafetyPolicy`], and declare which of the instance's devices are
/// heating-critical (switching them off counts as turning heating off).
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct InstanceSafety {
    /// Narrower setpoint band.
    pub setpoint: Option<SetpointBand>,
    /// Longer per-target interval.
    pub min_interval: Option<SignedDuration>,
    /// Lower hourly cap.
    pub max_per_hour: Option<u32>,
    /// Shorter maximum hold.
    pub max_hold: Option<SignedDuration>,
    /// `false` to forbid turning heating off even if the global policy
    /// allows it.
    pub allow_heating_off: Option<bool>,
    /// Devices whose switching off is governed by the heating-off rule
    /// (e.g. a plug powering a boiler or pump).
    pub heating_critical: BTreeSet<DeviceId>,
}

impl InstanceSafety {
    /// No narrowing, no heating-critical devices.
    pub fn new() -> Self {
        Self::default()
    }

    /// Narrow the setpoint band.
    #[must_use]
    pub fn with_setpoint(mut self, band: SetpointBand) -> Self {
        self.setpoint = Some(band);
        self
    }

    /// Lengthen the per-target interval.
    #[must_use]
    pub fn with_min_interval(mut self, d: SignedDuration) -> Self {
        self.min_interval = Some(d);
        self
    }

    /// Lower the hourly cap.
    #[must_use]
    pub fn with_max_per_hour(mut self, n: u32) -> Self {
        self.max_per_hour = Some(n);
        self
    }

    /// Shorten the maximum hold.
    #[must_use]
    pub fn with_max_hold(mut self, d: SignedDuration) -> Self {
        self.max_hold = Some(d);
        self
    }

    /// Forbid (or keep allowing) turning heating off.
    #[must_use]
    pub fn with_heating_off(mut self, allow: bool) -> Self {
        self.allow_heating_off = Some(allow);
        self
    }

    /// Declare a device heating-critical.
    #[must_use]
    pub fn with_heating_critical(mut self, device: DeviceId) -> Self {
        self.heating_critical.insert(device);
        self
    }
}

/// Rate limiter on monotonic time: a minimum interval per (instance,
/// target) and a maximum count per instance in any rolling hour, plus an
/// optional boot hold-off. Pure, so it can be tested with explicit times;
/// the limits come from each instance's policy at check time.
#[derive(Debug, Clone, Default)]
pub struct RateLimiter {
    last: HashMap<(String, String), SignedDuration>,
    recent: HashMap<String, VecDeque<SignedDuration>>,
    boot: Option<SignedDuration>,
    /// Accepted commands of unknown instance and/or target (unreadable
    /// audit entries): they count against every instance and target they
    /// might belong to.
    unknown: Vec<(Option<String>, Option<String>, SignedDuration)>,
}

const HOUR: SignedDuration = SignedDuration::from_hours(1);

impl RateLimiter {
    /// A limiter with no history and no hold-off.
    pub fn new() -> Self {
        Self::default()
    }

    /// A limiter that behaves as if every target had just been commanded
    /// at monotonic time `boot`: no non-exempt command is allowed for the
    /// per-target interval after it.
    pub fn after_boot(boot: SignedDuration) -> Self {
        Self {
            boot: Some(boot),
            ..Self::default()
        }
    }

    /// `Ok` if a command at monotonic time `now` is allowed under `policy`,
    /// else how long to wait. History "in the future" counts against the
    /// caller (conservative).
    pub fn check(
        &self,
        instance: &str,
        target: &str,
        now: SignedDuration,
        policy: &SafetyPolicy,
    ) -> Result<(), SignedDuration> {
        let mut wait = SignedDuration::ZERO;
        let mut spacing = |since: SignedDuration| {
            if since < policy.min_interval {
                wait = wait.max(policy.min_interval - since);
            }
        };
        if let Some(boot) = self.boot {
            spacing(now - boot);
        }
        if let Some(last) = self.last.get(&(instance.to_owned(), target.to_owned())) {
            spacing(now - *last);
        }
        let matches = |want: &Option<String>, have: &str| want.as_deref().is_none_or(|w| w == have);
        let mut in_window: Vec<SignedDuration> = self
            .recent
            .get(instance)
            .map(|r| r.iter().copied().filter(|t| now - *t < HOUR).collect())
            .unwrap_or_default();
        for (i, t, at) in &self.unknown {
            if matches(i, instance) {
                if now - *at < HOUR {
                    in_window.push(*at);
                }
                if matches(t, target) {
                    spacing(now - *at);
                }
            }
        }
        if in_window.len() >= policy.max_per_hour as usize {
            let oldest = in_window.iter().min().copied().unwrap_or(now);
            wait = wait
                .max(HOUR - (now - oldest))
                .max(SignedDuration::from_secs(1));
        }
        if wait > SignedDuration::ZERO {
            Err(wait)
        } else {
            Ok(())
        }
    }

    /// Record an accepted command whose instance and/or target is unknown
    /// (e.g. an unreadable audit entry) at monotonic time `at`: it counts
    /// against every instance (`None`) or target (`None`) it might belong
    /// to.
    pub fn record_unknown(
        &mut self,
        instance: Option<&str>,
        target: Option<&str>,
        at: SignedDuration,
    ) {
        self.unknown
            .push((instance.map(str::to_owned), target.map(str::to_owned), at));
    }

    /// Record an accepted command at monotonic time `now`.
    pub fn record(&mut self, instance: &str, target: &str, now: SignedDuration) {
        let last = self
            .last
            .entry((instance.to_owned(), target.to_owned()))
            .or_insert(now);
        *last = (*last).max(now);
        let recent = self.recent.entry(instance.to_owned()).or_default();
        recent.retain(|t| now - *t < HOUR);
        recent.push_back(now);
    }
}

/// A command plus why it is wanted. Who wants it is not part of the
/// request: it is the actor the [`CommandSubmitter`] was created for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CommandRequest {
    /// Adapter instance to deliver to.
    pub adapter: String,
    /// The command.
    pub command: Command,
    /// Why, in words.
    pub reason: String,
    /// The inputs the decision was based on.
    pub inputs: serde_json::Value,
}

impl CommandRequest {
    /// A request with empty inputs.
    pub fn new(
        adapter: impl Into<String>,
        command: impl Into<Command>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            adapter: adapter.into(),
            command: command.into(),
            reason: reason.into(),
            inputs: serde_json::Value::Null,
        }
    }

    /// Attach the decision inputs.
    #[must_use]
    pub fn with_inputs(mut self, inputs: serde_json::Value) -> Self {
        self.inputs = inputs;
        self
    }
}

/// What happened to an accepted command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Outcome {
    /// Audited, then queued for the live instance.
    Delivered {
        /// Command id.
        id: CommandId,
    },
    /// Audited only (shadow mode).
    Shadowed {
        /// Command id.
        id: CommandId,
    },
}

impl Outcome {
    /// The command id.
    pub fn id(&self) -> CommandId {
        match self {
            Self::Delivered { id } | Self::Shadowed { id } => *id,
        }
    }
}

/// Why a command was refused. Nothing was delivered.
#[derive(Debug, Clone, PartialEq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "rejection", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Rejection {
    /// No instance with that name is registered.
    #[error("unknown adapter {adapter:?}")]
    UnknownAdapter {
        /// Name asked for.
        adapter: String,
    },
    /// The instance does not declare this command kind.
    #[error("adapter {adapter:?} cannot carry out {kind} commands")]
    NotCapable {
        /// Instance.
        adapter: String,
        /// Command kind.
        kind: CommandKind,
    },
    /// The setpoint is outside the instance's band.
    #[error("setpoint {target} °C is outside the allowed band {min}..={max} °C")]
    OutsideBand {
        /// Requested, °C.
        target: f64,
        /// Band lower bound, °C.
        min: f64,
        /// Band upper bound, °C.
        max: f64,
    },
    /// Turning heating off (or switching off a heating-critical device) is
    /// not allowed by the policy.
    #[error("turning heating off is not allowed by the safety policy")]
    HeatingOffNotAllowed,
    /// The hold is not positive or longer than the policy allows.
    #[error("hold {hold} is not allowed (maximum {max:#})")]
    InvalidHold {
        /// Requested hold.
        hold: Hold,
        /// The policy's maximum.
        max: SignedDuration,
    },
    /// The fan level or preset is not one the instance declares.
    #[error("fan mode {mode:?} is not supported by this adapter")]
    UnsupportedFanMode {
        /// Requested mode.
        mode: FanMode,
    },
    /// Too soon after the previous command, too many this hour, or within
    /// the hold-off after start.
    #[error("rate limited; retry in {retry_after:#}")]
    RateLimited {
        /// Earliest sensible retry.
        retry_after: SignedDuration,
    },
    /// The instance's command queue is full.
    #[error("adapter {adapter:?} is not keeping up with commands")]
    AdapterBusy {
        /// Instance.
        adapter: String,
    },
    /// The instance is not running (restarting, down or stopped).
    #[error("adapter {adapter:?} is not running")]
    AdapterGone {
        /// Instance.
        adapter: String,
    },
    /// The audit record could not be written, so the command was withheld.
    #[error("the audit log is unavailable; command withheld")]
    AuditUnavailable,
    /// The audit record was not durable within the deadline, so the command
    /// was withheld.
    #[error("the audit log did not confirm in time; command withheld")]
    AuditTimedOut,
    /// This run has issued every possible command id; restart to continue.
    #[error("command ids exhausted for this run")]
    IdsExhausted,
}

impl Rejection {
    /// Whether the command was refused by capability or safety policy.
    pub fn is_policy(&self) -> bool {
        matches!(
            self,
            Self::NotCapable { .. }
                | Self::OutsideBand { .. }
                | Self::HeatingOffNotAllowed
                | Self::InvalidHold { .. }
                | Self::UnsupportedFanMode { .. }
        )
    }
}

/// Registering an instance with the actuator failed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum RegisterError {
    /// The name is already registered.
    #[error("adapter instance {0:?} is already registered")]
    Duplicate(String),
    /// No instance with that name is registered.
    #[error("adapter instance {0:?} is not registered")]
    Unknown(String),
    /// The instance name breaks the naming rules.
    #[error("invalid instance name {0:?}: {1}")]
    InvalidName(String, IdError),
    /// The instance name is reserved for the core
    /// ([`id::is_reserved_name`]).
    #[error("adapter instance name {0:?} is reserved for the core")]
    ReservedName(String),
    /// `live` was requested for an instance that cannot write.
    #[error("adapter instance {0:?} is read-only and cannot be live")]
    LiveButReadOnly(String),
    /// The capabilities are inconsistent.
    #[error(transparent)]
    Capabilities(#[from] DescriptorError),
    /// The per-instance safety settings are invalid or widen the policy.
    #[error(transparent)]
    Policy(#[from] PolicyError),
}

/// A decider's actor name was refused by [`Actuator::submitter`].
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ActorError {
    /// The name breaks the id rules.
    #[error("invalid actor {0:?}: {1}")]
    Invalid(String, IdError),
    /// The name is reserved for the core ([`id::is_reserved_name`]).
    #[error("actor name {0:?} is reserved for the core")]
    Reserved(String),
}

/// Building an actuator failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ActuatorError {
    /// The policy is invalid.
    #[error(transparent)]
    Policy(#[from] PolicyError),
    /// The audit trail could not be read.
    #[error("reading the audit trail: {0}")]
    Audit(#[from] AuditError),
    /// No boot number could be chosen.
    #[error(transparent)]
    Ids(#[from] CommandIdError),
}

/// What to register: an instance, its kind, capabilities, mode and safety
/// settings.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct InstanceSpec {
    /// Instance name ([`id::validate_name`] rules).
    pub name: String,
    /// Adapter kind.
    pub kind: String,
    /// What the instance can do.
    pub capabilities: Capabilities,
    /// Shadow or live.
    pub mode: Mode,
    /// Narrowing and heating-critical devices.
    pub safety: InstanceSafety,
}

impl InstanceSpec {
    /// A spec with no per-instance safety settings.
    pub fn new(
        name: impl Into<String>,
        kind: impl Into<String>,
        capabilities: Capabilities,
        mode: Mode,
    ) -> Self {
        Self {
            name: name.into(),
            kind: kind.into(),
            capabilities,
            mode,
            safety: InstanceSafety::default(),
        }
    }

    /// Use these safety settings.
    #[must_use]
    pub fn with_safety(mut self, safety: InstanceSafety) -> Self {
        self.safety = safety;
        self
    }
}

#[derive(Debug)]
enum Route {
    /// No queue exists. Shadow mode and read-only instances.
    Shadow,
    /// Live: the current run's queue, if the instance is running. The only
    /// place a command queue is reachable from.
    Live(Option<Arc<Mailbox>>),
}

#[derive(Debug)]
struct Registered {
    capabilities: Capabilities,
    mode: Mode,
    policy: SafetyPolicy,
    heating_critical: BTreeSet<DeviceId>,
    route: Route,
    stats: StatsHandle,
}

#[derive(Debug)]
struct State {
    adapters: HashMap<String, Registered>,
    limiter: RateLimiter,
    ids: CommandIds,
}

#[derive(Debug)]
struct Inner {
    policy: SafetyPolicy,
    clock: SharedClock,
    audit: Arc<dyn AuditLog>,
    audit_timeout: Duration,
    events: Option<Emitter>,
    queue: usize,
    state: Mutex<State>,
}

/// The command gate. Cheap to clone; clones share state. Owned by the app:
/// give deciders a [`CommandSubmitter`] instead.
#[derive(Debug, Clone)]
pub struct Actuator {
    inner: Arc<Inner>,
}

/// Default capacity of each live instance's command queue.
pub const COMMAND_QUEUE: usize = 16;
/// Default deadline for an audit record to become durable.
pub const AUDIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Builds an [`Actuator`]; see [`Actuator::builder`].
#[derive(Debug)]
pub struct ActuatorBuilder {
    policy: SafetyPolicy,
    clock: SharedClock,
    audit: Arc<dyn AuditLog>,
    audit_timeout: Duration,
    events: Option<Emitter>,
    queue: usize,
    boot_holdoff: bool,
}

impl ActuatorBuilder {
    /// How long an audit append may take before the command is withheld
    /// (default [`AUDIT_TIMEOUT`]).
    #[must_use]
    pub fn audit_timeout(mut self, d: Duration) -> Self {
        self.audit_timeout = d;
        self
    }

    /// Also copy every audit record to sinks through this core emitter,
    /// without ever waiting: if the sinks are behind, the copy is dropped
    /// (the durable record is unaffected).
    #[must_use]
    pub fn events(mut self, emitter: Emitter) -> Self {
        self.events = Some(emitter);
        self
    }

    /// Capacity of each live instance's queue (default [`COMMAND_QUEUE`]).
    #[must_use]
    pub fn queue_capacity(mut self, n: usize) -> Self {
        self.queue = n.max(1);
        self
    }

    /// Turn off the boot hold-off (see the [module docs](self)), which is
    /// on by default. For tests that start from a known-empty trail; a
    /// running system should keep it.
    #[must_use]
    pub fn without_boot_holdoff(mut self) -> Self {
        self.boot_holdoff = false;
        self
    }

    /// Validate the policy, read the audit trail's last hour to rebuild
    /// rate limits and pick a boot number, and build.
    pub async fn build(self) -> Result<Actuator, ActuatorError> {
        self.policy.validate()?;
        let now = self.clock.now();
        let mono = self.clock.monotonic();
        let tail = self.audit.tail(now - HOUR).await?;
        if tail.skipped > 0 || !tail.unknown.is_empty() {
            tracing::warn!(
                skipped = tail.skipped,
                unreadable = tail.unknown.len(),
                "unreadable audit entries counted as accepted commands"
            );
        }
        let mut limiter = if self.boot_holdoff {
            RateLimiter::after_boot(mono)
        } else {
            RateLimiter::new()
        };
        let mut accepted: Vec<&AuditRecord> =
            tail.records.iter().filter(|r| r.accepted()).collect();
        accepted.sort_by_key(|r| r.time);
        for r in accepted {
            // Entries "from the future" (wall clock stepped back) count as now.
            let age = now.duration_since(r.time).max(SignedDuration::ZERO);
            if age < HOUR {
                limiter.record(&r.adapter, &r.target, mono - age);
            }
        }
        // Fail safe: an entry that cannot be read counts as an accepted
        // command, as recent as it might be, for whatever it might address.
        for u in &tail.unknown {
            let age = u.time.map_or(SignedDuration::ZERO, |t| {
                now.duration_since(t).max(SignedDuration::ZERO)
            });
            if age < HOUR {
                limiter.record_unknown(u.adapter.as_deref(), u.target.as_deref(), mono - age);
            }
        }
        let ids = CommandIds::starting_after(tail.last_id, now)?;
        tracing::info!(boot = ids.boot(), "actuator ready");
        Ok(Actuator {
            inner: Arc::new(Inner {
                policy: self.policy,
                clock: self.clock,
                audit: self.audit,
                audit_timeout: self.audit_timeout,
                events: self.events,
                queue: self.queue,
                state: Mutex::new(State {
                    adapters: HashMap::new(),
                    limiter,
                    ids,
                }),
            }),
        })
    }
}

enum Decision {
    Shadow,
    Deliver(Reservation),
}

enum AppendError {
    Failed(AuditError),
    TimedOut,
}

impl Actuator {
    /// Start building an actuator enforcing `policy`, timing with `clock`
    /// and auditing to `audit`.
    pub fn builder(
        policy: SafetyPolicy,
        clock: SharedClock,
        audit: Arc<dyn AuditLog>,
    ) -> ActuatorBuilder {
        ActuatorBuilder {
            policy,
            clock,
            audit,
            audit_timeout: AUDIT_TIMEOUT,
            events: None,
            queue: COMMAND_QUEUE,
            boot_holdoff: true,
        }
    }

    /// The global policy in force.
    pub fn policy(&self) -> &SafetyPolicy {
        &self.inner.policy
    }

    /// A submit-only handle for one decider. Every command submitted
    /// through it is audited with `actor` (e.g. `planner`, `user:alice`),
    /// which follows the id rules and must not be reserved for the core
    /// ([`id::is_reserved_name`]).
    pub fn submitter(&self, actor: impl Into<String>) -> Result<CommandSubmitter, ActorError> {
        let actor = actor.into();
        id::validate(&actor).map_err(|e| ActorError::Invalid(actor.clone(), e))?;
        if id::is_reserved_name(&actor) {
            return Err(ActorError::Reserved(actor));
        }
        Ok(CommandSubmitter {
            actuator: self.clone(),
            actor: actor.into(),
        })
    }

    /// Register an instance. Its effective policy is the global one narrowed
    /// by `spec.safety`. A live instance gets a command queue only while it
    /// runs ([`Actuator::attach`]); a shadow or read-only one never does.
    pub fn register(&self, spec: InstanceSpec) -> Result<(), RegisterError> {
        id::validate_name(&spec.name)
            .map_err(|e| RegisterError::InvalidName(spec.name.clone(), e))?;
        if id::is_reserved_name(&spec.name) {
            return Err(RegisterError::ReservedName(spec.name));
        }
        spec.capabilities.validate()?;
        let policy = self.inner.policy.narrowed(&spec.safety)?;
        let route = match spec.mode {
            Mode::Live if !spec.capabilities.can_write() => {
                return Err(RegisterError::LiveButReadOnly(spec.name));
            }
            Mode::Live => Route::Live(None),
            Mode::Shadow => Route::Shadow,
        };
        let mut state = self.lock();
        if state.adapters.contains_key(&spec.name) {
            return Err(RegisterError::Duplicate(spec.name));
        }
        let stats = StatsHandle::new(&spec.name, &spec.kind, spec.mode);
        state.adapters.insert(
            spec.name,
            Registered {
                capabilities: spec.capabilities,
                mode: spec.mode,
                policy,
                heating_critical: spec.safety.heating_critical,
                route,
                stats,
            },
        );
        Ok(())
    }

    /// The command handle for a new run of `instance`. Live instances get a
    /// fresh queue (any previous run's queue is closed and its undelivered
    /// commands audited as dropped); shadow and read-only instances get
    /// [`Commands::disconnected`].
    pub async fn attach(&self, instance: &str) -> Result<Commands, RegisterError> {
        let (commands, drained) = {
            let mut state = self.lock();
            let reg = state
                .adapters
                .get_mut(instance)
                .ok_or_else(|| RegisterError::Unknown(instance.to_owned()))?;
            match &mut reg.route {
                Route::Shadow => (Commands::disconnected(), Vec::new()),
                Route::Live(slot) => {
                    let mb = Mailbox::new(self.inner.queue);
                    // Closed under the lock: no reservation can be made on
                    // the old queue after this, and one already made fails
                    // to send (and is audited as dropped).
                    let drained = slot
                        .replace(mb.clone())
                        .map(|p| p.close())
                        .unwrap_or_default();
                    (Commands::live(mb), drained)
                }
            }
        };
        self.drop_queued(
            instance,
            drained,
            "the instance restarted before receiving it",
        )
        .await;
        Ok(commands)
    }

    /// `instance`'s run ended: close its queue so nothing more is accepted
    /// for it, and audit every command it had not received as dropped.
    /// Commands are never kept for a later run.
    pub async fn detach(&self, instance: &str) {
        let drained = {
            let mut state = self.lock();
            match state.adapters.get_mut(instance).map(|r| &mut r.route) {
                Some(Route::Live(slot)) => slot.take().map(|p| p.close()).unwrap_or_default(),
                _ => Vec::new(),
            }
        };
        self.drop_queued(
            instance,
            drained,
            "the instance stopped before receiving it",
        )
        .await;
    }

    /// The mode an instance is registered with.
    pub fn mode(&self, instance: &str) -> Option<Mode> {
        self.lock().adapters.get(instance).map(|r| r.mode)
    }

    /// An instance's effective (possibly narrowed) policy.
    pub fn policy_for(&self, instance: &str) -> Option<SafetyPolicy> {
        self.lock().adapters.get(instance).map(|r| r.policy.clone())
    }

    /// The telemetry handle created when `instance` was registered. The app
    /// shares it with the instance's emitter and supervisor so one
    /// [`AdapterStats`](crate::AdapterStats) covers commands, records and
    /// runs.
    pub fn stats(&self, instance: &str) -> Option<StatsHandle> {
        self.lock().adapters.get(instance).map(|r| r.stats.clone())
    }

    /// Check, audit and (if live and running) deliver a command on behalf
    /// of `actor`. See the [module docs](self). Runs on its own task, so it
    /// completes even if the caller's future is dropped. The outcome is
    /// also counted in the instance's stats.
    pub(crate) async fn submit_as(
        &self,
        actor: &str,
        req: CommandRequest,
    ) -> Result<Outcome, Rejection> {
        let this = self.clone();
        let actor = actor.to_owned();
        let task = tokio::spawn(async move {
            let stats = this.stats(&req.adapter);
            let result = this.submit_inner(&actor, req, stats.as_ref()).await;
            if let Some(stats) = stats {
                stats.command(&result);
            }
            result
        });
        match task.await {
            Ok(result) => result,
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            // The runtime is shutting down; nothing was confirmed.
            Err(_) => Err(Rejection::AuditUnavailable),
        }
    }

    async fn submit_inner(
        &self,
        actor: &str,
        req: CommandRequest,
        stats: Option<&StatsHandle>,
    ) -> Result<Outcome, Rejection> {
        let now = self.inner.clock.now();
        let mono = self.inner.clock.monotonic();
        let (id, mode, decision) = self.decide(&req, mono);
        let Some(id) = id else {
            tracing::error!("command ids exhausted; restart to continue");
            return Err(Rejection::IdsExhausted);
        };
        let result = match &decision {
            Ok(Decision::Shadow) => AuditResult::Shadowed,
            Ok(Decision::Deliver(_)) => AuditResult::Delivered,
            Err(r) => AuditResult::Rejected {
                rejection: r.clone(),
            },
        };
        let record = AuditRecord {
            kind: AuditKind::Decision,
            id,
            time: now,
            adapter: req.adapter.clone(),
            actor: actor.to_owned(),
            mode,
            target: req.command.target().to_owned(),
            hold: req.command.hold(),
            command: req.command.clone(),
            reason: req.reason.clone(),
            inputs: req.inputs.clone(),
            result,
        };
        let logged = self.append(&record).await;
        match (decision, logged) {
            (Err(rejection), logged) => {
                if logged.is_err() {
                    tracing::error!(%id, "could not audit a rejected command");
                }
                tracing::warn!(adapter = %req.adapter, actor = %actor, %id, %rejection, "command rejected");
                Err(rejection)
            }
            (Ok(_), Err(AppendError::Failed(e))) => {
                tracing::error!(adapter = %req.adapter, %id, error = %e, "audit log unavailable; command withheld");
                Err(Rejection::AuditUnavailable)
            }
            (Ok(_), Err(AppendError::TimedOut)) => {
                tracing::error!(adapter = %req.adapter, %id, "audit log did not confirm in time; command withheld");
                // The decision record may still land; say it was withheld.
                self.audit_drop(
                    &req.adapter,
                    id,
                    &req.command,
                    "the audit write timed out; withheld",
                )
                .await;
                Err(Rejection::AuditTimedOut)
            }
            (Ok(Decision::Shadow), Ok(())) => {
                tracing::info!(adapter = %req.adapter, actor = %actor, %id, command = ?req.command, "command shadowed");
                Ok(Outcome::Shadowed { id })
            }
            (Ok(Decision::Deliver(slot)), Ok(())) => {
                match slot.send(CommandEnvelope::new(id, now, req.command)) {
                    Ok(()) => {
                        tracing::info!(adapter = %req.adapter, actor = %actor, %id, "command delivered");
                        Ok(Outcome::Delivered { id })
                    }
                    Err(env) => {
                        self.audit_drop(
                            &req.adapter,
                            id,
                            &env.command,
                            "the instance stopped before the command was queued",
                        )
                        .await;
                        if let Some(s) = stats {
                            s.command_dropped();
                        }
                        Err(Rejection::AdapterGone {
                            adapter: req.adapter,
                        })
                    }
                }
            }
        }
    }

    /// Everything that needs the lock, done synchronously: id, checks, rate
    /// limit check-and-record and queue reservation are one atomic step.
    fn decide(
        &self,
        req: &CommandRequest,
        now: SignedDuration,
    ) -> (Option<CommandId>, Option<Mode>, Result<Decision, Rejection>) {
        let mut guard = self.lock();
        let state = &mut *guard;
        let Some(id) = state.ids.next_id() else {
            return (None, None, Err(Rejection::IdsExhausted));
        };
        let Some(reg) = state.adapters.get(&req.adapter) else {
            let r = Rejection::UnknownAdapter {
                adapter: req.adapter.clone(),
            };
            return (Some(id), None, Err(r));
        };
        let mode = Some(reg.mode);
        if let Err(r) = check_policy(reg, &req.adapter, &req.command) {
            return (Some(id), mode, Err(r));
        }
        let target = req.command.target();
        // The safe direction is never rate-limited away (it still counts).
        let exempt = req.command.kind() == CommandKind::ClimateSchedule;
        if !exempt
            && let Err(retry_after) = state.limiter.check(&req.adapter, target, now, &reg.policy)
        {
            return (Some(id), mode, Err(Rejection::RateLimited { retry_after }));
        }
        let decision = match &reg.route {
            Route::Shadow => Decision::Shadow,
            Route::Live(Some(mb)) => match mb.try_reserve() {
                Ok(slot) => Decision::Deliver(slot),
                Err(ReserveError::Full) => {
                    let r = Rejection::AdapterBusy {
                        adapter: req.adapter.clone(),
                    };
                    return (Some(id), mode, Err(r));
                }
                Err(ReserveError::Closed) => {
                    let r = Rejection::AdapterGone {
                        adapter: req.adapter.clone(),
                    };
                    return (Some(id), mode, Err(r));
                }
            },
            Route::Live(None) => {
                let r = Rejection::AdapterGone {
                    adapter: req.adapter.clone(),
                };
                return (Some(id), mode, Err(r));
            }
        };
        state.limiter.record(&req.adapter, target, now);
        (Some(id), mode, Ok(decision))
    }

    async fn append(&self, record: &AuditRecord) -> Result<(), AppendError> {
        let fut = self.inner.audit.append(record.clone());
        let res = match tokio::time::timeout(self.inner.audit_timeout, fut).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(AppendError::Failed(e)),
            Err(_) => Err(AppendError::TimedOut),
        };
        // Sinks only ever see records that are durable.
        if res.is_ok()
            && let Some(events) = &self.inner.events
            && !events.try_emit(Record::Event(record.to_event()))
        {
            tracing::warn!(id = %record.id, "sinks are behind; audit copy not forwarded (the audit trail has it)");
        }
        res
    }

    async fn audit_drop(&self, instance: &str, id: CommandId, command: &Command, why: &str) {
        let record = AuditRecord {
            kind: AuditKind::Dropped,
            id,
            time: self.inner.clock.now(),
            adapter: instance.to_owned(),
            actor: ACTUATOR.to_owned(),
            mode: self.mode(instance),
            target: command.target().to_owned(),
            hold: command.hold(),
            command: command.clone(),
            reason: why.to_owned(),
            inputs: serde_json::Value::Null,
            result: AuditResult::Dropped {
                why: why.to_owned(),
            },
        };
        if self.append(&record).await.is_err() {
            tracing::error!(%id, "could not audit a dropped command");
        }
    }

    async fn drop_queued(&self, instance: &str, drained: Vec<CommandEnvelope>, why: &str) {
        let stats = self.stats(instance);
        for env in drained {
            tracing::warn!(adapter = instance, id = %env.id, why, "command dropped");
            self.audit_drop(instance, env.id, &env.command, why).await;
            if let Some(s) = &stats {
                s.command_dropped();
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // Every mutation is a single insert or replace, so the state stays
        // consistent across a panic and a poisoned lock is safe to reuse.
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

fn check_hold(hold: Hold, policy: &SafetyPolicy) -> Result<(), Rejection> {
    match hold {
        // Positive and within the absolute maximum by construction.
        Hold::For(d) if d.get() > policy.max_hold => Err(Rejection::InvalidHold {
            hold,
            max: policy.max_hold,
        }),
        _ => Ok(()),
    }
}

fn check_policy(reg: &Registered, adapter: &str, command: &Command) -> Result<(), Rejection> {
    let kind = command.kind();
    if !reg.capabilities.supports(kind) {
        return Err(Rejection::NotCapable {
            adapter: adapter.to_owned(),
            kind,
        });
    }
    let policy = &reg.policy;
    match command {
        Command::Climate(ClimateCommand::SetTarget { target, hold, .. }) => {
            // Re-checked whatever way the setpoint was built.
            if !policy.setpoint.contains(*target) {
                return Err(Rejection::OutsideBand {
                    target: target.get(),
                    min: policy.setpoint.min(),
                    max: policy.setpoint.max(),
                });
            }
            check_hold(*hold, policy)
        }
        Command::Climate(ClimateCommand::TurnOff { hold, .. }) => {
            if !policy.allow_heating_off {
                return Err(Rejection::HeatingOffNotAllowed);
            }
            check_hold(*hold, policy)
        }
        Command::Climate(ClimateCommand::ResumeSchedule { .. }) => Ok(()),
        Command::Fan(FanCommand::SetMode { mode, hold, .. }) => {
            if !reg.capabilities.allows_fan_mode(mode) {
                return Err(Rejection::UnsupportedFanMode { mode: mode.clone() });
            }
            check_hold(*hold, policy)
        }
        Command::Switch(SwitchCommand::Set { device, on }) => {
            if !*on && reg.heating_critical.contains(device) && !policy.allow_heating_off {
                return Err(Rejection::HeatingOffNotAllowed);
            }
            Ok(())
        }
    }
}

/// A submit-only handle to the [`Actuator`] for one decider (planner,
/// assistant, a front-end user), from [`Actuator::submitter`]. Its actor is
/// fixed when it is created and recorded on every command it submits. It
/// cannot register instances, attach command queues or touch stats.
#[derive(Debug, Clone)]
pub struct CommandSubmitter {
    actuator: Actuator,
    actor: Arc<str>,
}

impl CommandSubmitter {
    /// Check, audit and (if live) deliver a command. Completes even if the
    /// returned future is dropped.
    pub async fn submit(&self, req: CommandRequest) -> Result<Outcome, Rejection> {
        self.actuator.submit_as(&self.actor, req).await
    }

    /// The actor recorded on this handle's commands.
    pub fn actor(&self) -> &str {
        &self.actor
    }

    /// The global policy in force.
    pub fn policy(&self) -> SafetyPolicy {
        self.actuator.policy().clone()
    }

    /// An instance's effective (possibly narrowed) policy, e.g. to build a
    /// setpoint inside its band.
    pub fn policy_for(&self, instance: &str) -> Option<SafetyPolicy> {
        self.actuator.policy_for(instance)
    }
}

#[cfg(test)]
mod tests;
