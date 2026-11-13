//! Domain types, the adapter contract and the command safety layer for
//! [heimr](https://github.com/hugsmid/heimr).
//!
//! - **Types**: [`ZoneId`], [`DeviceId`], [`Reading`] (shaped like the
//!   store's `reading` row), [`Event`], climate/fan/switch state and
//!   [`Command`]s. Every override carries a [`Hold`] (until the next
//!   schedule change, or for a bounded time). Setpoints are [`Setpoint`]s,
//!   checked against a [`SetpointBand`] when built and again by the actuator.
//! - **Adapter API**: implement [`Adapter`] (typed) and register it; the app
//!   runs it through the object-safe [`AdapterFactory`]. Everything an
//!   instance may touch arrives in an [`AdapterContext`]: its typed config,
//!   an emitter for [`Record`]s, a command handle, a [`Clock`], a
//!   [`Shutdown`] signal, a state directory and the [`Site`] (time zone
//!   and currency, from the site's configuration). It never sees the bus, the
//!   store or other adapters, and cannot emit as anyone else.
//! - **Telemetry for free** ([`AdapterStats`]): the core counts what each
//!   instance emits, every command offered, delivered, shadowed, rejected or
//!   dropped, and every run and restart; adapters report health in one line
//!   (`ctx.health().ok()`).
//! - **Safety** ([`Actuator`]): every command is checked against the
//!   instance's [`SafetyPolicy`] (band, holds, heating-off rule, rate
//!   limits), written durably to an [`AuditLog`] with actor, reason and
//!   inputs, and only then delivered — and only to instances configured
//!   [`Mode::Live`]. **Shadow mode is the default**: a shadow instance has no
//!   command queue at all. Deciders get a submit-only [`CommandSubmitter`]
//!   bound to their actor name.
//! - **Limits of the guarantees**: rate limits survive restarts only as far
//!   as the audit trail does. If it is missing altogether, the hourly-cap
//!   history is lost and the boot hold-off (on by default) is the only limit
//!   after that start; unreadable entries count as accepted commands.
//!
//! ```
//! use std::sync::Arc;
//! use heimr_core::*;
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() {
//! let clock = Arc::new(ManualClock::new("2030-01-01T00:00:00Z".parse().unwrap()));
//! let audit = MemoryAuditLog::new();
//! let actuator = Actuator::builder(SafetyPolicy::default(), clock.clone(), Arc::new(audit.clone()))
//!     .build()
//!     .await
//!     .unwrap();
//!
//! let caps = Capabilities::read_write([CommandKind::ClimateTarget]);
//! actuator.register(InstanceSpec::new("heating", "thermostat", caps, Mode::Shadow)).unwrap();
//! assert!(!actuator.attach("heating").await.unwrap().is_live());
//!
//! // No command but "resume schedule" for the per-target interval after start.
//! clock.advance(SafetyPolicy::default().min_interval);
//! let planner = actuator.submitter("planner").unwrap();
//! let target = planner.policy_for("heating").unwrap().setpoint(19.5).unwrap();
//! let cmd = ClimateCommand::set_target("zone-a".parse().unwrap(), target, Hold::NextScheduleChange);
//! let out = planner.submit(CommandRequest::new("heating", cmd, "pre-heat")).await;
//! assert!(matches!(out, Ok(Outcome::Shadowed { .. })));
//! assert_eq!(audit.records().len(), 1); // audited, not delivered
//! # }
//! ```
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod actuator;
pub mod adapter;
pub mod audit;
pub mod clock;
pub mod context;
pub mod domain;
pub mod id;
pub mod record;
pub mod shutdown;
pub mod site;
pub mod telemetry;
pub mod units;

pub use actuator::{
    ACTUATOR, ActorError, Actuator, ActuatorBuilder, ActuatorError, CommandRequest,
    CommandSubmitter, InstanceSafety, InstanceSpec, Mode, Outcome, PolicyError, RateLimiter,
    RegisterError, Rejection, SafetyPolicy,
};
pub use adapter::{
    Adapter, AdapterDescriptor, AdapterError, AdapterFactory, AdapterInstance, Capabilities,
    DescriptorError, LevelRange, TypedFactory,
};
pub use audit::{
    AuditError, AuditKind, AuditLog, AuditRecord, AuditResult, AuditTail, MemoryAuditLog,
    UnreadableEntry,
};
pub use clock::{BoxFuture, Clock, ManualClock, SharedClock, SystemClock, TokioClock};
pub use context::{AdapterContext, CommandEnvelope, Commands, ContextParts, EmitError, Emitter};
pub use domain::{
    ClimateCommand, ClimateMode, ClimateState, Command, CommandKind, FanCommand, FanMode, FanState,
    Hold, HoldDuration, HoldError, SwitchCommand, SwitchState,
};
pub use id::{CommandId, CommandIdError, CommandIds, DeviceId, IdError, ZoneId};
pub use record::{Envelope, Event, Quality, Reading, ReadingError, Record, StateClass, StateKey};
pub use shutdown::{Shutdown, ShutdownTrigger, shutdown_channel};
pub use site::{Currency, Site, SiteError};
pub use telemetry::{
    AdapterStats, HealthReporter, HealthStatus, InstanceStats, Metrics, RunState, StatsHandle,
};
pub use units::{Metric, Setpoint, SetpointBand, SetpointError, Unit};
