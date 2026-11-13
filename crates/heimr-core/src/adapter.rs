//! The adapter contract.
//!
//! Two layers:
//!
//! - [`Adapter`]: the typed trait Rust adapter authors implement, with an
//!   associated `Config` type and a native `async fn run`.
//! - [`AdapterFactory`] / [`AdapterInstance`]: the object-safe layer the app
//!   registers and runs. [`TypedFactory`] adapts any [`Adapter`] into it. A
//!   host for adapters in other processes implements [`AdapterFactory`]
//!   directly, with a descriptor that arrives at run time.
//!
//! A *kind* (e.g. `demo`) is a registered factory; an *instance* is one
//! `[adapters.<instance>]` section with `type = "<kind>"`. Several instances
//! of one kind may run; the instance name is the `source` of everything an
//! instance emits.

use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::clock::BoxFuture;
use crate::context::{AdapterContext, ContextParts, EmitError};
use crate::domain::{CommandKind, FanMode};
use crate::id::{self, IdError};

/// An inclusive range of fan levels an adapter accepts in
/// [`FanMode::Manual`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LevelRange {
    /// Lowest level.
    pub min: u8,
    /// Highest level.
    pub max: u8,
}

impl LevelRange {
    /// `min..=max`; checked by [`Capabilities::validate`].
    pub fn new(min: u8, max: u8) -> Self {
        Self { min, max }
    }

    /// Whether `level` lies in the range.
    pub fn contains(&self, level: u8) -> bool {
        (self.min..=self.max).contains(&level)
    }
}

/// What an adapter (kind or instance) can do.
///
/// ```
/// use heimr_core::{Capabilities, CommandKind, FanMode};
/// let caps = Capabilities::read_write([CommandKind::FanMode])
///     .with_fan_levels(1, 5)
///     .with_fan_presets(["night", "boost"]);
/// caps.validate().unwrap();
/// assert!(caps.allows_fan_mode(&FanMode::Manual(5)));
/// assert!(!caps.allows_fan_mode(&FanMode::Manual(6)));
/// assert!(caps.allows_fan_mode(&FanMode::preset("night")));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Capabilities {
    /// Emits readings, state or events.
    pub read: bool,
    /// Command kinds it can carry out. Empty means read-only: such an
    /// adapter never gets a command channel, whatever its config says.
    pub write: Vec<CommandKind>,
    /// Levels accepted in [`FanMode::Manual`] (with [`CommandKind::FanMode`]).
    pub fan_levels: Option<LevelRange>,
    /// Presets accepted in [`FanMode::Preset`] (with [`CommandKind::FanMode`]).
    pub fan_presets: Vec<String>,
}

impl Capabilities {
    /// Reads only.
    pub fn read_only() -> Self {
        Self {
            read: true,
            ..Self::default()
        }
    }

    /// Reads and carries out the given command kinds.
    pub fn read_write(write: impl IntoIterator<Item = CommandKind>) -> Self {
        Self {
            read: true,
            write: write.into_iter().collect(),
            ..Self::default()
        }
    }

    /// Declare the levels accepted in [`FanMode::Manual`].
    #[must_use]
    pub fn with_fan_levels(mut self, min: u8, max: u8) -> Self {
        self.fan_levels = Some(LevelRange::new(min, max));
        self
    }

    /// Declare the presets accepted in [`FanMode::Preset`].
    #[must_use]
    pub fn with_fan_presets(
        mut self,
        presets: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.fan_presets = presets.into_iter().map(Into::into).collect();
        self
    }

    /// Whether it accepts any command.
    pub fn can_write(&self) -> bool {
        !self.write.is_empty()
    }

    /// Whether it accepts commands of `kind`.
    pub fn supports(&self, kind: CommandKind) -> bool {
        self.write.contains(&kind)
    }

    /// Whether `mode` is acceptable: `Off` and `Auto` always, `Manual` within
    /// the declared levels, `Preset` if declared.
    pub fn allows_fan_mode(&self, mode: &FanMode) -> bool {
        match mode {
            FanMode::Manual(level) => self.fan_levels.is_some_and(|r| r.contains(*level)),
            FanMode::Preset(name) => self.fan_presets.iter().any(|p| p == name),
            FanMode::Off | FanMode::Auto => true,
        }
    }

    /// Whether everything `other` can do, this can do too.
    pub fn includes(&self, other: &Self) -> bool {
        (self.read || !other.read)
            && other.write.iter().all(|k| self.supports(*k))
            && match (self.fan_levels, other.fan_levels) {
                (_, None) => true,
                (None, Some(_)) => false,
                (Some(a), Some(b)) => a.min <= b.min && b.max <= a.max,
            }
            && other
                .fan_presets
                .iter()
                .all(|p| self.fan_presets.contains(p))
    }

    /// Check internal consistency: no duplicate kinds; a valid level range;
    /// presets are unique identifiers; a fan-capable adapter declares
    /// levels or presets.
    pub fn validate(&self) -> Result<(), DescriptorError> {
        let mut seen = BTreeSet::new();
        if let Some(k) = self.write.iter().find(|k| !seen.insert(**k)) {
            return Err(DescriptorError::DuplicateKind(*k));
        }
        if let Some(r) = self.fan_levels
            && r.min > r.max
        {
            return Err(DescriptorError::InvalidLevelRange {
                min: r.min,
                max: r.max,
            });
        }
        let mut names = BTreeSet::new();
        for p in &self.fan_presets {
            id::validate(p).map_err(|e| DescriptorError::InvalidPreset(p.clone(), e))?;
            if !names.insert(p.as_str()) {
                return Err(DescriptorError::DuplicatePreset(p.clone()));
            }
        }
        if self.supports(CommandKind::FanMode)
            && self.fan_levels.is_none()
            && self.fan_presets.is_empty()
        {
            return Err(DescriptorError::FanWithoutLevels);
        }
        Ok(())
    }
}

/// Description of an adapter kind. Owned data, so a descriptor can be built
/// at run time (e.g. received from another process).
///
/// ```
/// use heimr_core::{AdapterDescriptor, Capabilities, CommandKind};
/// let d = AdapterDescriptor::new(
///     "thermostat",
///     "Heating zones of a thermostat",
///     Capabilities::read_write([CommandKind::ClimateTarget, CommandKind::ClimateSchedule]),
/// );
/// d.validate().unwrap();
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AdapterDescriptor {
    /// Kind name, as written in `type = "<kind>"`: lowercase letters,
    /// digits, `-` and `_` (see [`id::validate_name`]).
    pub kind: String,
    /// One-line description for logs and `check-config`.
    pub summary: String,
    /// The most a single instance of this kind can do. An instance may
    /// declare less ([`AdapterInstance::capabilities`]), never more.
    pub capabilities: Capabilities,
}

impl AdapterDescriptor {
    /// A descriptor.
    pub fn new(
        kind: impl Into<String>,
        summary: impl Into<String>,
        capabilities: Capabilities,
    ) -> Self {
        Self {
            kind: kind.into(),
            summary: summary.into(),
            capabilities,
        }
    }

    /// Check the kind name and the capabilities.
    pub fn validate(&self) -> Result<(), DescriptorError> {
        id::validate_name(&self.kind)
            .map_err(|e| DescriptorError::InvalidKind(self.kind.clone(), e))?;
        self.capabilities.validate()
    }
}

/// An [`AdapterDescriptor`] or instance capabilities are inconsistent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DescriptorError {
    /// The kind name breaks the naming rules.
    #[error("invalid adapter kind {0:?}: {1}")]
    InvalidKind(String, IdError),
    /// A command kind was listed twice.
    #[error("command kind {0} is listed twice")]
    DuplicateKind(CommandKind),
    /// `min > max` in the fan level range.
    #[error("fan level range {min}..={max} is empty")]
    InvalidLevelRange {
        /// Lowest level.
        min: u8,
        /// Highest level.
        max: u8,
    },
    /// A preset name breaks the id rules.
    #[error("invalid fan preset {0:?}: {1}")]
    InvalidPreset(String, IdError),
    /// A preset was listed twice.
    #[error("fan preset {0:?} is listed twice")]
    DuplicatePreset(String),
    /// `fan_mode` is writable but no level range or preset is declared.
    #[error("fan_mode commands need declared fan levels or presets")]
    FanWithoutLevels,
    /// An instance declared more than its kind's descriptor allows.
    #[error("instance capabilities exceed those of kind {0:?}")]
    ExceedsKind(String),
}

/// Errors an adapter returns while being built or while running.
///
/// The supervisor restarts an instance that fails with a *transient* error
/// (with backoff). A *fatal* error ([`AdapterError::is_fatal`]: invalid
/// configuration, a contract violation, an emit the core refused) keeps the
/// instance down and unhealthy; the rest of the app keeps running.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AdapterError {
    /// The configuration is invalid. Fatal.
    #[non_exhaustive]
    #[error("invalid configuration: {message}")]
    Config {
        /// The offending key within the instance section, if known.
        key: Option<String>,
        /// What is wrong.
        message: String,
    },
    /// The adapter broke the contract (e.g. a device returned something the
    /// adapter cannot represent safely). Fatal.
    #[error("contract violation: {0}")]
    Contract(String),
    /// Emitting failed. Fatal if the core refused the record (provenance,
    /// validation); transient if the app was closing.
    #[error(transparent)]
    Emit(#[from] EmitError),
    /// A failure that a restart may fix (transport, timeouts, a device
    /// offline). The instance is restarted with backoff.
    #[error("{0}")]
    Transient(Box<dyn std::error::Error + Send + Sync>),
    /// A failure a restart cannot fix (credentials rejected, unsupported
    /// device). The instance stays down.
    #[error("{0}")]
    Fatal(Box<dyn std::error::Error + Send + Sync>),
}

impl AdapterError {
    /// A configuration error.
    pub fn config(msg: impl Into<String>) -> Self {
        Self::Config {
            key: None,
            message: msg.into(),
        }
    }

    /// A configuration error about one key of the instance section.
    pub fn config_key(key: impl Into<String>, msg: impl Into<String>) -> Self {
        Self::Config {
            key: Some(key.into()),
            message: msg.into(),
        }
    }

    /// A contract violation.
    pub fn contract(msg: impl Into<String>) -> Self {
        Self::Contract(msg.into())
    }

    /// A transient failure.
    pub fn transient(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self::Transient(err.into())
    }

    /// A fatal failure.
    pub fn fatal(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self::Fatal(err.into())
    }

    /// Whether restarting cannot help.
    pub fn is_fatal(&self) -> bool {
        match self {
            Self::Config { .. } | Self::Contract(_) | Self::Fatal(_) => true,
            Self::Emit(e) => !matches!(e, EmitError::Closed),
            Self::Transient(_) => false,
        }
    }
}

/// The typed adapter trait for Rust adapter authors.
///
/// The app builds the adapter from its instance section with
/// [`Adapter::new`] (no I/O: this also runs for `check-config` and before
/// every restart), then calls [`Adapter::run`] with an [`AdapterContext`].
/// Everything the adapter may touch comes through the context; it never sees
/// the bus, the store or other adapters. Register it with
/// `App::builder(..).adapter::<A>()`, which wraps it in a [`TypedFactory`].
///
/// # Async shape
/// `run` is a native `async fn` in a trait (return-position `impl Future +
/// Send`), so implementors write a plain `async fn run(self, ctx) -> …` with
/// no macro. [`TypedFactory`] erases the type by boxing the one future `run`
/// returns per start.
///
/// # Example
/// <!-- ANCHOR: thermometer -->
/// ```
/// use heimr_core::{Adapter, AdapterContext, AdapterDescriptor, AdapterError, Capabilities};
/// use heimr_core::{DeviceId, Metric, Unit};
/// use jiff::SignedDuration;
///
/// #[derive(Debug, serde::Deserialize)]
/// struct Config { interval: SignedDuration }
///
/// struct Thermometer;
///
/// impl Adapter for Thermometer {
///     type Config = Config;
///
///     fn descriptor() -> AdapterDescriptor {
///         AdapterDescriptor::new("thermometer", "Reads one temperature sensor", Capabilities::read_only())
///     }
///
///     fn new(config: &Config) -> Result<Self, AdapterError> {
///         if config.interval <= SignedDuration::ZERO {
///             return Err(AdapterError::config_key("interval", "must be positive"));
///         }
///         Ok(Self)
///     }
///
///     async fn run(self, ctx: AdapterContext<Config>) -> Result<(), AdapterError> {
///         let device = DeviceId::new("sensor-1").map_err(AdapterError::fatal)?;
///         let mut next = ctx.now();
///         loop {
///             tokio::select! {
///                 () = ctx.shutdown().wait() => return Ok(()),
///                 () = ctx.sleep_until(next) => {}
///             }
///             let r = ctx.reading(device.clone(), Metric::TEMPERATURE, 20.0, Unit::Celsius);
///             ctx.emit(r).await?;
///             next += ctx.config().interval;
///         }
///     }
/// }
///
/// // The object-safe form the app registers:
/// let factory = heimr_core::TypedFactory::<Thermometer>::new();
/// assert_eq!(heimr_core::AdapterFactory::descriptor(&factory).kind, "thermometer");
/// ```
/// <!-- ANCHOR_END: thermometer -->
// ANCHOR: adapter-trait
pub trait Adapter: Sized + Send + 'static {
    /// The adapter's own keys in its instance section (everything except
    /// `type`, `enabled`, `mode` and `safety`, which the app handles).
    /// Unknown keys are rejected.
    type Config: DeserializeOwned + Send + Sync + 'static;

    /// Kind name, summary and the most any instance can do.
    fn descriptor() -> AdapterDescriptor;

    /// Build the adapter from its config. Validate here; do no I/O.
    fn new(config: &Self::Config) -> Result<Self, AdapterError>;

    /// What this instance can do, if it depends on its config (e.g. the
    /// levels of the configured device model). Must not exceed the
    /// descriptor. Defaults to the descriptor's capabilities.
    fn capabilities(&self) -> Capabilities {
        Self::descriptor().capabilities
    }

    /// Run until [`AdapterContext::shutdown`] fires, then return `Ok(())`
    /// promptly. Handle routine failures (reconnects) inside. Returning
    /// `Err`, panicking, or returning `Ok(())` before shutdown marks the
    /// instance unhealthy; transient errors restart it with backoff, fatal
    /// ones (and repeated panics) keep it down.
    fn run(
        self,
        ctx: AdapterContext<Self::Config>,
    ) -> impl Future<Output = Result<(), AdapterError>> + Send;
}
// ANCHOR_END: adapter-trait

/// The object-safe registration of one adapter kind.
///
/// The app keeps the factory and each instance's settings, and calls
/// [`AdapterFactory::prepare`] for configuration checks, at start-up and
/// again before every restart.
pub trait AdapterFactory: Send + Sync + fmt::Debug + 'static {
    /// The kind's descriptor.
    fn descriptor(&self) -> AdapterDescriptor;

    /// Validate an instance's settings (a JSON object of the adapter's own
    /// keys) and build an instance ready to run, including what it can do
    /// ([`AdapterInstance::capabilities`]). Asynchronous and fallible so a
    /// host for an adapter in another process can validate the settings and
    /// learn the capabilities over its protocol; it must not start the
    /// adapter's work. Unknown keys must be reported as
    /// [`AdapterError::Config`] with the key. [`TypedFactory`] completes at
    /// once and does no I/O.
    fn prepare(
        &self,
        settings: &serde_json::Value,
    ) -> BoxFuture<Result<Box<dyn AdapterInstance>, AdapterError>>;
}

/// A configured instance, ready to run once.
pub trait AdapterInstance: Send + 'static {
    /// What this instance can do (within the kind's descriptor), as
    /// determined by [`AdapterFactory::prepare`].
    fn capabilities(&self) -> Capabilities;

    /// Run with the given context parts until shutdown.
    fn run(self: Box<Self>, parts: ContextParts) -> BoxFuture<Result<(), AdapterError>>;
}

/// Adapts a typed [`Adapter`] into an [`AdapterFactory`]: deserialises the
/// settings into `A::Config`, rejecting unknown keys by name, and calls
/// [`Adapter::new`].
pub struct TypedFactory<A> {
    descriptor: AdapterDescriptor,
    _adapter: PhantomData<fn() -> A>,
}

impl<A: Adapter> TypedFactory<A> {
    /// The factory for `A`.
    pub fn new() -> Self {
        Self {
            descriptor: A::descriptor(),
            _adapter: PhantomData,
        }
    }
}

impl<A: Adapter> Default for TypedFactory<A> {
    fn default() -> Self {
        Self::new()
    }
}

impl<A> fmt::Debug for TypedFactory<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TypedFactory")
            .field("kind", &self.descriptor.kind)
            .finish()
    }
}

struct TypedInstance<A: Adapter> {
    adapter: A,
    config: Arc<A::Config>,
}

impl<A: Adapter> AdapterFactory for TypedFactory<A> {
    fn descriptor(&self) -> AdapterDescriptor {
        self.descriptor.clone()
    }

    fn prepare(
        &self,
        settings: &serde_json::Value,
    ) -> BoxFuture<Result<Box<dyn AdapterInstance>, AdapterError>> {
        let result = typed_instance::<A>(settings);
        Box::pin(std::future::ready(result))
    }
}

fn typed_instance<A: Adapter>(
    settings: &serde_json::Value,
) -> Result<Box<dyn AdapterInstance>, AdapterError> {
    let mut unknown = Vec::new();
    let config: A::Config =
        serde_ignored::deserialize(settings.clone(), |path| unknown.push(path.to_string()))
            .map_err(|e| AdapterError::config(e.to_string()))?;
    if let Some(first) = unknown.into_iter().next() {
        return Err(AdapterError::config_key(first, "unknown key"));
    }
    let adapter = A::new(&config)?;
    Ok(Box::new(TypedInstance::<A> {
        adapter,
        config: Arc::new(config),
    }))
}

impl<A: Adapter> AdapterInstance for TypedInstance<A> {
    fn capabilities(&self) -> Capabilities {
        self.adapter.capabilities()
    }

    fn run(self: Box<Self>, parts: ContextParts) -> BoxFuture<Result<(), AdapterError>> {
        let ctx = AdapterContext::new(parts, self.config);
        Box::pin(self.adapter.run(ctx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize)]
    struct Cfg {
        #[serde(default)]
        n: u32,
    }

    struct Dummy;

    impl Adapter for Dummy {
        type Config = Cfg;
        fn descriptor() -> AdapterDescriptor {
            AdapterDescriptor::new("dummy", "test", Capabilities::read_only())
        }
        fn new(c: &Cfg) -> Result<Self, AdapterError> {
            if c.n > 3 {
                return Err(AdapterError::config_key("n", "too big"));
            }
            Ok(Self)
        }
        async fn run(self, _ctx: AdapterContext<Cfg>) -> Result<(), AdapterError> {
            Ok(())
        }
    }

    fn prep(v: serde_json::Value) -> Result<Box<dyn AdapterInstance>, AdapterError> {
        // TypedFactory completes at once.
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(TypedFactory::<Dummy>::new().prepare(&v))
    }

    #[test]
    fn typed_factory_rejects_unknown_keys_by_name() {
        assert!(prep(serde_json::json!({})).is_ok());
        match prep(serde_json::json!({"nn": 1})) {
            Err(AdapterError::Config { key, message }) => {
                assert_eq!(key.as_deref(), Some("nn"));
                assert_eq!(message, "unknown key");
            }
            _ => panic!("expected an unknown-key error"),
        }
        assert!(matches!(
            prep(serde_json::json!({"n": "x"})),
            Err(AdapterError::Config { key: None, .. })
        ));
        assert!(prep(serde_json::json!({"n": 4})).is_err_and(|e| e.is_fatal()));
    }

    #[test]
    fn capability_checks() {
        let fan = Capabilities::read_write([CommandKind::FanMode]);
        assert_eq!(fan.validate(), Err(DescriptorError::FanWithoutLevels));
        assert!(fan.clone().with_fan_levels(3, 1).validate().is_err());
        assert!(fan.clone().with_fan_presets(["a", "a"]).validate().is_err());
        assert!(fan.clone().with_fan_presets(["a b"]).validate().is_err());
        let both =
            Capabilities::read_write([CommandKind::ClimateTarget, CommandKind::ClimateTarget]);
        assert!(both.validate().is_err());
        let wide = fan
            .clone()
            .with_fan_levels(1, 10)
            .with_fan_presets(["night"]);
        let narrow = fan.with_fan_levels(2, 5);
        assert!(wide.includes(&narrow));
        assert!(!narrow.includes(&wide));
        assert!(!Capabilities::read_only().includes(&narrow));
        assert!(
            AdapterDescriptor::new("Bad", "x", Capabilities::read_only())
                .validate()
                .is_err()
        );
    }

    #[test]
    fn error_classes() {
        assert!(!AdapterError::transient("timeout").is_fatal());
        assert!(AdapterError::fatal("bad credentials").is_fatal());
        assert!(AdapterError::contract("x").is_fatal());
        assert!(!AdapterError::Emit(EmitError::Closed).is_fatal());
        assert!(AdapterError::Emit(EmitError::ReservedKind("command".into())).is_fatal());
    }
}
