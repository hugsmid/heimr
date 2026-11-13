//! Typed configuration, loaded from TOML and validated at start-up.
//!
//! Unknown keys are rejected everywhere, including inside adapter instance
//! sections. See `heimr.example.toml` for every key with its default.
//!
//! Adapter instances are `[adapters.<instance>]` sections with a required
//! `type = "<kind>"`. Several instances of one kind may exist; the instance
//! name is the `source` of everything the instance emits. Besides the
//! adapter's own keys, an instance section takes `enabled`, `mode` and an
//! optional `[adapters.<instance>.safety]` table that may only narrow
//! `[safety]`; these four names are reserved.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use heimr_core::id::{is_reserved_name, validate_name};
use heimr_core::{DeviceId, InstanceSafety, Mode, SafetyPolicy, SetpointBand};
use jiff::SignedDuration;
use serde::Deserialize;

/// A configuration problem. Messages name the offending key.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The file could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// File.
        path: PathBuf,
        /// Cause.
        source: std::io::Error,
    },
    /// The TOML is malformed or does not match the schema (includes line
    /// and column).
    #[error("{0}")]
    Parse(#[from] toml::de::Error),
    /// A value is well-formed but not acceptable.
    #[error("{key}: {message}")]
    Invalid {
        /// Dotted key, e.g. `safety.setpoint_min`.
        key: String,
        /// What is wrong.
        message: String,
    },
}

impl ConfigError {
    pub(crate) fn invalid(key: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Invalid {
            key: key.into(),
            message: message.into(),
        }
    }
}

/// The whole configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Config {
    /// General settings.
    #[serde(default)]
    pub heimr: General,
    /// Logging.
    #[serde(default)]
    pub log: Log,
    /// Command safety limits (applied by the actuator to every instance).
    #[serde(default)]
    pub safety: Safety,
    /// The command audit journal.
    #[serde(default)]
    pub audit: Audit,
    /// Restarting failed adapter instances.
    #[serde(default)]
    pub supervision: Supervision,
    /// Built-in sinks.
    #[serde(default)]
    pub sinks: Sinks,
    /// Adapter instances by name: `[adapters.<instance>]`.
    #[serde(default)]
    pub adapters: BTreeMap<String, InstanceSection>,
}

/// `[heimr]`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct General {
    /// Root for persistent state: the audit journal in `<state_dir>/audit/`,
    /// and `<state_dir>/adapters/<instance>/` for each instance.
    pub state_dir: PathBuf,
    /// How long instances get to stop after a shutdown signal before they
    /// are aborted.
    pub shutdown_grace: SignedDuration,
}

impl Default for General {
    fn default() -> Self {
        Self {
            state_dir: PathBuf::from("/var/lib/heimr"),
            shutdown_grace: SignedDuration::from_secs(10),
        }
    }
}

/// `[log]`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct Log {
    /// `tracing` filter, e.g. `info` or `info,heimr=debug`. `RUST_LOG`
    /// overrides it.
    pub filter: String,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            filter: "info".to_owned(),
        }
    }
}

/// `[safety]`: limits the actuator enforces on every command.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct Safety {
    /// Lowest setpoint accepted (°C, within 5.0..=30.0).
    pub setpoint_min: f64,
    /// Highest setpoint accepted (°C, within 5.0..=30.0).
    pub setpoint_max: f64,
    /// Minimum time between commands to the same zone or device (also the
    /// hold-off after start).
    pub min_command_interval: SignedDuration,
    /// Maximum commands per instance in any rolling hour.
    pub max_commands_per_hour: u32,
    /// Longest timed hold a command may ask for (at most 24h).
    pub max_hold: SignedDuration,
    /// Whether commands may turn heating off (including switching off
    /// heating-critical devices).
    pub allow_heating_off: bool,
}

impl Default for Safety {
    fn default() -> Self {
        let p = SafetyPolicy::default();
        Self {
            setpoint_min: p.setpoint.min(),
            setpoint_max: p.setpoint.max(),
            min_command_interval: p.min_interval,
            max_commands_per_hour: p.max_per_hour,
            max_hold: p.max_hold,
            allow_heating_off: p.allow_heating_off,
        }
    }
}

impl Safety {
    /// The actuator policy these settings describe.
    pub fn policy(&self) -> Result<SafetyPolicy, ConfigError> {
        let band = SetpointBand::new(self.setpoint_min, self.setpoint_max)
            .map_err(|e| ConfigError::invalid("safety.setpoint_min", e.to_string()))?;
        let policy = SafetyPolicy::default()
            .with_setpoint(band)
            .with_min_interval(self.min_command_interval)
            .with_max_per_hour(self.max_commands_per_hour)
            .with_max_hold(self.max_hold)
            .with_heating_off(self.allow_heating_off);
        policy.validate().map_err(|e| {
            let key = match e {
                heimr_core::PolicyError::InvalidMaxHold => "safety.max_hold",
                _ => "safety.min_command_interval",
            };
            ConfigError::invalid(key, e.to_string())
        })?;
        Ok(policy)
    }
}

/// `[audit]`: the append-only JSON-lines journal of command decisions in
/// `<state_dir>/audit/`. Every record is fsynced before the command it
/// describes is delivered.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct Audit {
    /// Start a new journal file once the current one reaches this many
    /// bytes (at least 4096).
    pub max_file_bytes: u64,
    /// Journal files kept; older ones are deleted on rotation. The newest
    /// `retain_files` files are never deleted (at least 2).
    pub retain_files: u32,
    /// How long a write may take before the command is withheld.
    pub append_timeout: SignedDuration,
}

impl Default for Audit {
    fn default() -> Self {
        Self {
            max_file_bytes: 16 * 1024 * 1024,
            retain_files: 64,
            append_timeout: SignedDuration::from_secs(2),
        }
    }
}

/// `[supervision]`: restarting failed instances with exponential backoff
/// and jitter.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct Supervision {
    /// Delay before the first restart (jittered to between half and all of
    /// it); doubles with each consecutive failure.
    pub backoff_initial: SignedDuration,
    /// Longest delay between restarts.
    pub backoff_max: SignedDuration,
    /// An instance that ran this long before failing counts as healthy: its
    /// backoff starts again from `backoff_initial`, and its panic count
    /// from zero.
    pub reset_after: SignedDuration,
    /// After this many panics with no healthy run (`reset_after`) in
    /// between, the instance is kept down instead of restarted. Other
    /// failures in between do not reset the count. At least 1.
    pub max_consecutive_panics: u32,
}

impl Default for Supervision {
    fn default() -> Self {
        Self {
            backoff_initial: SignedDuration::from_secs(1),
            backoff_max: SignedDuration::from_mins(5),
            reset_after: SignedDuration::from_mins(10),
            max_consecutive_panics: 5,
        }
    }
}

/// `[sinks]`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct Sinks {
    /// Log every record through `tracing` (readings and state at `debug`,
    /// events at `info`).
    pub logging: bool,
}

impl Default for Sinks {
    fn default() -> Self {
        Self { logging: true }
    }
}

/// `[adapters.<instance>.safety]`: may only narrow `[safety]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct InstanceSafetySection {
    /// Narrower lower setpoint bound (the global one if omitted).
    pub setpoint_min: Option<f64>,
    /// Narrower upper setpoint bound (the global one if omitted).
    pub setpoint_max: Option<f64>,
    /// Longer per-target interval.
    pub min_command_interval: Option<SignedDuration>,
    /// Lower hourly cap.
    pub max_commands_per_hour: Option<u32>,
    /// Shorter maximum hold.
    pub max_hold: Option<SignedDuration>,
    /// `false` to forbid turning heating off for this instance.
    pub allow_heating_off: Option<bool>,
    /// Devices of this instance whose switching off counts as turning
    /// heating off (e.g. a plug powering a boiler or pump).
    #[serde(default)]
    pub heating_critical: Vec<DeviceId>,
}

impl InstanceSafetySection {
    /// The core's per-instance settings (bounds not given fall back to the
    /// global band).
    pub fn to_instance_safety(&self, global: &Safety) -> Result<InstanceSafety, String> {
        let mut s = InstanceSafety::new();
        if self.setpoint_min.is_some() || self.setpoint_max.is_some() {
            let band = SetpointBand::new(
                self.setpoint_min.unwrap_or(global.setpoint_min),
                self.setpoint_max.unwrap_or(global.setpoint_max),
            )
            .map_err(|e| e.to_string())?;
            s = s.with_setpoint(band);
        }
        if let Some(d) = self.min_command_interval {
            s = s.with_min_interval(d);
        }
        if let Some(n) = self.max_commands_per_hour {
            s = s.with_max_per_hour(n);
        }
        if let Some(d) = self.max_hold {
            s = s.with_max_hold(d);
        }
        if let Some(b) = self.allow_heating_off {
            s = s.with_heating_off(b);
        }
        for d in &self.heating_critical {
            s = s.with_heating_critical(d.clone());
        }
        Ok(s)
    }
}

/// `[adapters.<instance>]`: the reserved keys plus the adapter's own
/// settings.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct InstanceSection {
    /// The adapter kind (`type = "<kind>"`). Required.
    pub kind: String,
    /// Whether to run it. Defaults to `true`.
    pub enabled: bool,
    /// `shadow` (default: commands are audited, never delivered) or `live`.
    pub mode: Mode,
    /// Per-instance safety narrowing.
    pub safety: InstanceSafetySection,
    /// Everything else, as a JSON object, for the adapter's own config.
    pub settings: serde_json::Value,
}

/// Keys of an instance section that belong to heimr, not the adapter.
pub const RESERVED_INSTANCE_KEYS: &[&str] = &["type", "enabled", "mode", "safety"];

fn to_json(key: &str, v: toml::Value) -> Result<serde_json::Value, String> {
    use serde_json::Value as J;
    Ok(match v {
        toml::Value::String(s) => J::String(s),
        toml::Value::Integer(i) => J::from(i),
        toml::Value::Float(f) => serde_json::Number::from_f64(f)
            .map(J::Number)
            .ok_or_else(|| format!("`{key}` must be a finite number"))?,
        toml::Value::Boolean(b) => J::Bool(b),
        toml::Value::Datetime(d) => J::String(d.to_string()),
        toml::Value::Array(a) => J::Array(
            a.into_iter()
                .map(|x| to_json(key, x))
                .collect::<Result<_, _>>()?,
        ),
        toml::Value::Table(t) => J::Object(
            t.into_iter()
                .map(|(k, x)| {
                    let v = to_json(&format!("{key}.{k}"), x)?;
                    Ok((k, v))
                })
                .collect::<Result<_, String>>()?,
        ),
    })
}

impl<'de> Deserialize<'de> for InstanceSection {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let mut table = toml::Table::deserialize(d)?;
        let kind = match table.remove("type") {
            Some(toml::Value::String(s)) => s,
            None => {
                return Err(D::Error::custom(
                    "`type` is required: the adapter kind, e.g. type = \"demo\"",
                ));
            }
            Some(other) => {
                return Err(D::Error::custom(format!(
                    "`type` must be a string, not {other}"
                )));
            }
        };
        let enabled = match table.remove("enabled") {
            None => true,
            Some(toml::Value::Boolean(b)) => b,
            Some(other) => {
                return Err(D::Error::custom(format!(
                    "`enabled` must be true or false, not {other}"
                )));
            }
        };
        let mode = match table.remove("mode") {
            None => Mode::Shadow,
            Some(v) => Mode::deserialize(v).map_err(|_| {
                D::Error::custom("`mode` must be \"shadow\" or \"live\"".to_owned())
            })?,
        };
        let safety = match table.remove("safety") {
            None => InstanceSafetySection::default(),
            Some(v) => InstanceSafetySection::deserialize(v)
                .map_err(|e| D::Error::custom(format!("`safety`: {e}")))?,
        };
        let settings = to_json("", toml::Value::Table(table)).map_err(D::Error::custom)?;
        Ok(Self {
            kind,
            enabled,
            mode,
            safety,
            settings,
        })
    }
}

impl Config {
    /// Parse and validate a TOML document.
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(s)?;
        config.validate()?;
        Ok(config)
    }

    /// Read, parse and validate a TOML file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::from_toml_str(&text)
    }

    /// Check everything that does not depend on which adapter kinds are
    /// registered (those checks happen in
    /// [`AppBuilder::check`](crate::AppBuilder::check)).
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.heimr.state_dir.as_os_str().is_empty() {
            return Err(ConfigError::invalid("heimr.state_dir", "must not be empty"));
        }
        if self.heimr.shutdown_grace <= SignedDuration::ZERO {
            return Err(ConfigError::invalid(
                "heimr.shutdown_grace",
                "must be positive",
            ));
        }
        tracing_subscriber::EnvFilter::try_new(&self.log.filter)
            .map_err(|e| ConfigError::invalid("log.filter", e.to_string()))?;
        let policy = self.safety.policy()?;
        if self.audit.max_file_bytes < 4096 {
            return Err(ConfigError::invalid(
                "audit.max_file_bytes",
                "must be at least 4096",
            ));
        }
        if self.audit.retain_files < 2 {
            return Err(ConfigError::invalid(
                "audit.retain_files",
                "must be at least 2",
            ));
        }
        if self.audit.append_timeout <= SignedDuration::ZERO {
            return Err(ConfigError::invalid(
                "audit.append_timeout",
                "must be positive",
            ));
        }
        let s = &self.supervision;
        if s.backoff_initial <= SignedDuration::ZERO {
            return Err(ConfigError::invalid(
                "supervision.backoff_initial",
                "must be positive",
            ));
        }
        if s.backoff_max < s.backoff_initial {
            return Err(ConfigError::invalid(
                "supervision.backoff_max",
                "must not be shorter than backoff_initial",
            ));
        }
        if s.reset_after <= SignedDuration::ZERO {
            return Err(ConfigError::invalid(
                "supervision.reset_after",
                "must be positive",
            ));
        }
        if s.max_consecutive_panics == 0 {
            return Err(ConfigError::invalid(
                "supervision.max_consecutive_panics",
                "must be at least 1",
            ));
        }
        for (name, section) in &self.adapters {
            if is_reserved_name(name) {
                return Err(ConfigError::invalid(
                    format!("adapters.{name}"),
                    "this instance name is reserved for the core (actuator, core, heimr…)",
                ));
            }
            validate_name(name).map_err(|_| {
                ConfigError::invalid(
                    format!("adapters.{name}"),
                    "instance names use lowercase letters, digits, '-' and '_'",
                )
            })?;
            validate_name(&section.kind).map_err(|_| {
                ConfigError::invalid(
                    format!("adapters.{name}.type"),
                    "adapter kinds use lowercase letters, digits, '-' and '_'",
                )
            })?;
            let key = format!("adapters.{name}.safety");
            let instance = section
                .safety
                .to_instance_safety(&self.safety)
                .map_err(|e| ConfigError::invalid(&key, e))?;
            policy
                .narrowed(&instance)
                .map_err(|e| ConfigError::invalid(&key, e.to_string()))?;
        }
        Ok(())
    }

    /// The core's per-instance safety settings for `instance`.
    pub fn instance_safety(&self, instance: &str) -> Result<InstanceSafety, ConfigError> {
        let section = self.adapters.get(instance).ok_or_else(|| {
            ConfigError::invalid(format!("adapters.{instance}"), "no such instance")
        })?;
        section
            .safety
            .to_instance_safety(&self.safety)
            .map_err(|e| ConfigError::invalid(format!("adapters.{instance}.safety"), e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(toml: &str) -> String {
        Config::from_toml_str(toml).unwrap_err().to_string()
    }

    #[test]
    fn empty_config_uses_safe_defaults() {
        let c = Config::from_toml_str("").unwrap();
        assert!(c.adapters.is_empty());
        let p = c.safety.policy().unwrap();
        assert_eq!(p, SafetyPolicy::default());
        assert!(!p.allow_heating_off);
    }

    #[test]
    fn instance_section_defaults_to_enabled_shadow() {
        let c = Config::from_toml_str("[adapters.a]\ntype = \"demo\"\ninterval = \"1m\"").unwrap();
        let s = &c.adapters["a"];
        assert_eq!(s.kind, "demo");
        assert!(s.enabled);
        assert_eq!(s.mode, Mode::Shadow);
        assert_eq!(s.settings, serde_json::json!({"interval": "1m"}));
    }

    #[test]
    fn rejects_unknown_and_bad_values() {
        assert!(err("[heimr]\nstate_dri = \"/x\"").contains("unknown field `state_dri`"));
        assert!(err("bogus = 1").contains("unknown field `bogus`"));
        assert!(err("[safety]\nsetpoint_max = 31.0").contains("outside the absolute envelope"));
        assert!(
            err("[safety]\nsetpoint_min = 22.0\nsetpoint_max = 18.0")
                .starts_with("safety.setpoint_min: setpoint band is inverted")
        );
        assert!(err("[safety]\nmin_command_interval = \"-5m\"").contains("must not be negative"));
        assert!(err("[safety]\nmax_hold = \"25h\"").starts_with("safety.max_hold"));
        assert!(err("[adapters.a]\nmode = \"live\"").contains("`type` is required"));
        assert!(err("[adapters.a]\ntype = 3").contains("`type` must be a string"));
        assert!(err("[adapters.a]\ntype = \"Demo\"").starts_with("adapters.a.type:"));
        assert!(err("[adapters.a]\ntype = \"demo\"\nmode = \"yolo\"").contains("`mode` must be"));
        assert!(err("[adapters.a]\ntype = \"demo\"\nmode = \"Live\"").contains("`mode` must be"));
        assert!(
            err("[adapters.a]\ntype = \"demo\"\nenabled = \"yes\"").contains("`enabled` must be")
        );
        assert!(err("[adapters.Demo]\ntype = \"demo\"").starts_with("adapters.Demo:"));
        for reserved in ["actuator", "core", "heimr", "heimr-x"] {
            assert!(
                err(&format!("[adapters.{reserved}]\ntype = \"demo\""))
                    .contains("reserved for the core"),
                "{reserved}"
            );
        }
        assert!(err("[log]\nfilter = \"[[[\"").starts_with("log.filter:"));
        assert!(err("[heimr]\nshutdown_grace = \"0s\"").starts_with("heimr.shutdown_grace"));
        assert!(err("[audit]\nretain_files = 1").starts_with("audit.retain_files"));
        assert!(
            err("[supervision]\nbackoff_initial = \"10s\"\nbackoff_max = \"1s\"")
                .starts_with("supervision.backoff_max")
        );
        assert!(
            err("[supervision]\nmax_consecutive_panics = 0")
                .starts_with("supervision.max_consecutive_panics")
        );
        assert!(err("[adapters.a]\ntype = \"demo\"\nx = nan").contains("finite"));
    }

    #[test]
    fn instance_safety_only_narrows() {
        let ok = Config::from_toml_str(
            "[adapters.a]\ntype = \"demo\"\n[adapters.a.safety]\nsetpoint_max = 20.0\nheating_critical = [\"boiler\"]",
        )
        .unwrap();
        let s = ok.instance_safety("a").unwrap();
        assert_eq!(s.setpoint.unwrap().max(), 20.0);
        assert_eq!(s.setpoint.unwrap().min(), 15.0);
        assert_eq!(s.heating_critical.len(), 1);
        assert!(
            err("[adapters.a]\ntype = \"demo\"\n[adapters.a.safety]\nsetpoint_max = 25.0")
                .starts_with("adapters.a.safety: setpoint band may only narrow")
        );
        assert!(
            err("[adapters.a]\ntype = \"demo\"\n[adapters.a.safety]\nallow_heating_off = true")
                .contains("allow_heating_off may only narrow")
        );
        assert!(
            err("[adapters.a]\ntype = \"demo\"\n[adapters.a.safety]\nmax_commands_per_hour = 99")
                .contains("may only narrow")
        );
        assert!(
            err("[adapters.a]\ntype = \"demo\"\n[adapters.a.safety]\nsetpoint_mx = 20.0")
                .contains("unknown field")
        );
        assert!(
            err("[adapters.a]\ntype = \"demo\"\n[adapters.a.safety]\nallow_heating_off = \"no\"")
                .contains("`safety`")
        );
    }
}
