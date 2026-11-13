//! Identifiers: zones, devices, adapter instance and kind names, command ids.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Longest identifier accepted.
pub const MAX_ID_LEN: usize = 64;

/// An identifier was empty, too long or contained a disallowed character.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IdError {
    /// The identifier was empty.
    #[error("identifier is empty")]
    Empty,
    /// The identifier was longer than [`MAX_ID_LEN`] bytes.
    #[error("identifier is longer than {MAX_ID_LEN} bytes")]
    TooLong,
    /// The identifier contained a disallowed character.
    #[error("identifier contains {0:?}, which is not allowed here")]
    InvalidChar(char),
}

/// Check an identifier against the shared rules: 1 to [`MAX_ID_LEN`] bytes
/// of ASCII letters, digits and `.` `_` `:` `-` (zones, devices, metrics,
/// event kinds, custom counters).
pub(crate) fn validate(s: &str) -> Result<(), IdError> {
    check(s, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-')
    })
}

fn check(s: &str, allowed: impl Fn(char) -> bool) -> Result<(), IdError> {
    if s.is_empty() {
        return Err(IdError::Empty);
    }
    if s.len() > MAX_ID_LEN {
        return Err(IdError::TooLong);
    }
    match s.chars().find(|c| !allowed(*c)) {
        Some(c) => Err(IdError::InvalidChar(c)),
        None => Ok(()),
    }
}

/// Check an adapter instance or kind name: 1 to [`MAX_ID_LEN`] bytes of
/// lowercase ASCII letters, digits, `-` and `_`. Instance names are the
/// `source` of everything an adapter emits and name its state directory, so
/// the rules are deliberately narrow.
///
/// ```
/// assert!(heimr_core::id::validate_name("heating-1").is_ok());
/// assert!(heimr_core::id::validate_name("Heating").is_err());
/// ```
pub fn validate_name(name: &str) -> Result<(), IdError> {
    check(name, |c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_')
    })
}

/// Whether `name` is reserved for the core: `actuator`, `core`, or anything
/// starting with `heimr` (the app's own source names). Adapter instances
/// cannot use these names, and deciders cannot use them as actors, so
/// nothing but the core can emit or audit under them.
///
/// ```
/// use heimr_core::id::is_reserved_name;
/// assert!(is_reserved_name("actuator") && is_reserved_name("heimr-x"));
/// assert!(!is_reserved_name("planner"));
/// ```
pub fn is_reserved_name(name: &str) -> bool {
    name == crate::actuator::ACTUATOR || name == "core" || name.starts_with("heimr")
}

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Validate and wrap an identifier.
            pub fn new(id: impl Into<String>) -> Result<Self, IdError> {
                let id = id.into();
                validate(&id)?;
                Ok(Self(id))
            }

            /// The identifier as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = IdError;
            fn from_str(s: &str) -> Result<Self, IdError> {
                Self::new(s)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;
            fn try_from(s: String) -> Result<Self, IdError> {
                Self::new(s)
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> String {
                id.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(
    /// A heating zone, stable within its adapter instance (e.g. `zone-a`).
    ZoneId
);
id_type!(
    /// A device, stable within its adapter instance (e.g. `sensor-1`).
    DeviceId
);

impl From<ZoneId> for DeviceId {
    /// A zone is addressable as a device (e.g. for readings about it).
    fn from(z: ZoneId) -> Self {
        Self(z.0)
    }
}

/// Identifies one command across the whole audit trail, across restarts.
///
/// A command id is a *boot number* and a *sequence number* within that boot.
/// The boot number is chosen at start-up as one more than the highest boot
/// number in the audit trail, and never less than the current Unix time in
/// seconds; the sequence restarts at 1. So ids are unique across restarts
/// even if the audit trail was lost (as long as the wall clock is sane), and
/// their order never depends on the wall clock moving forwards within a run.
/// No randomness and no extra dependency are needed.
///
/// Serialised as `"BBBBBBBBBBBB-SSSSSSSSSS"` (12 and 10 zero-padded decimal
/// digits), so string order equals numeric order.
///
/// ```
/// use heimr_core::CommandId;
/// let a = CommandId::new(1_900_000_000, 7).unwrap();
/// assert_eq!(a.to_string(), "001900000000-0000000007");
/// assert_eq!("001900000000-0000000007".parse::<CommandId>().unwrap(), a);
/// assert!(a < CommandId::new(1_900_000_001, 1).unwrap());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CommandId {
    boot: u64,
    seq: u32,
}

/// A [`CommandId`] could not be built or parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CommandIdError {
    /// The boot number does not fit in 12 decimal digits.
    #[error("boot number {0} is too large")]
    BootTooLarge(u64),
    /// The sequence number was 0 (sequences start at 1).
    #[error("sequence numbers start at 1")]
    ZeroSequence,
    /// The text is not `BBBBBBBBBBBB-SSSSSSSSSS`.
    #[error("{0:?} is not a command id (expected 12 digits, '-', 10 digits)")]
    Malformed(String),
}

impl CommandId {
    /// Largest boot number (12 decimal digits).
    pub const MAX_BOOT: u64 = 999_999_999_999;

    /// An id; `seq` starts at 1.
    pub fn new(boot: u64, seq: u32) -> Result<Self, CommandIdError> {
        if boot > Self::MAX_BOOT {
            return Err(CommandIdError::BootTooLarge(boot));
        }
        if seq == 0 {
            return Err(CommandIdError::ZeroSequence);
        }
        Ok(Self { boot, seq })
    }

    /// The boot number.
    pub fn boot(self) -> u64 {
        self.boot
    }

    /// The sequence number within the boot.
    pub fn seq(self) -> u32 {
        self.seq
    }
}

impl fmt::Display for CommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:012}-{:010}", self.boot, self.seq)
    }
}

impl FromStr for CommandId {
    type Err = CommandIdError;
    fn from_str(s: &str) -> Result<Self, CommandIdError> {
        let malformed = || CommandIdError::Malformed(s.to_owned());
        let (boot, seq) = s.split_once('-').ok_or_else(malformed)?;
        let digits = |p: &str, n: usize| p.len() == n && p.bytes().all(|b| b.is_ascii_digit());
        if !digits(boot, 12) || !digits(seq, 10) {
            return Err(malformed());
        }
        let boot = boot.parse().map_err(|_| malformed())?;
        let seq = seq.parse().map_err(|_| malformed())?;
        Self::new(boot, seq)
    }
}

impl TryFrom<String> for CommandId {
    type Error = CommandIdError;
    fn try_from(s: String) -> Result<Self, CommandIdError> {
        s.parse()
    }
}

impl From<CommandId> for String {
    fn from(id: CommandId) -> String {
        id.to_string()
    }
}

/// Hands out [`CommandId`]s for one run.
#[derive(Debug, Clone)]
pub struct CommandIds {
    boot: u64,
    next: Option<u32>,
}

impl CommandIds {
    /// Ids for a new boot: the boot number is above `last` (the highest id
    /// already in the audit trail, if any) and at least `now`'s Unix second.
    pub fn starting_after(
        last: Option<CommandId>,
        now: jiff::Timestamp,
    ) -> Result<Self, CommandIdError> {
        let floor = u64::try_from(now.as_second()).unwrap_or(0);
        let after = last.map_or(0, |id| id.boot.saturating_add(1));
        let boot = floor.max(after);
        if boot > CommandId::MAX_BOOT {
            return Err(CommandIdError::BootTooLarge(boot));
        }
        Ok(Self {
            boot,
            next: Some(1),
        })
    }

    /// The boot number of this run.
    pub fn boot(&self) -> u64 {
        self.boot
    }

    /// The next id, or `None` once the sequence is exhausted (after
    /// 4 294 967 295 ids in one run; callers fail closed).
    pub fn next_id(&mut self) -> Option<CommandId> {
        let seq = self.next?;
        self.next = seq.checked_add(1);
        Some(CommandId {
            boot: self.boot,
            seq,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn accepts_and_rejects() {
        assert!(ZoneId::new("zone-a").is_ok());
        assert!(DeviceId::new("dev_1:2.3").is_ok());
        assert_eq!(ZoneId::new(""), Err(IdError::Empty));
        assert_eq!(ZoneId::new("a b"), Err(IdError::InvalidChar(' ')));
        assert_eq!(ZoneId::new("x".repeat(65)), Err(IdError::TooLong));
    }

    #[test]
    fn serde_validates() {
        assert!(serde_json::from_str::<ZoneId>("\"ok\"").is_ok());
        assert!(serde_json::from_str::<ZoneId>("\"not ok\"").is_err());
    }

    #[test]
    fn names() {
        assert!(validate_name("demo_1-x").is_ok());
        assert_eq!(validate_name(""), Err(IdError::Empty));
        assert_eq!(validate_name("a.b"), Err(IdError::InvalidChar('.')));
        assert_eq!(validate_name("A"), Err(IdError::InvalidChar('A')));
    }

    #[test]
    fn command_id_parsing() {
        assert!("1-1".parse::<CommandId>().is_err());
        assert!("000000000001-0000000000".parse::<CommandId>().is_err());
        assert!("00000000000a-0000000001".parse::<CommandId>().is_err());
        assert!(CommandId::new(CommandId::MAX_BOOT + 1, 1).is_err());
        let mut ids = CommandIds {
            boot: 1,
            next: Some(u32::MAX),
        };
        assert!(ids.next_id().is_some());
        assert!(ids.next_id().is_none(), "fails closed when exhausted");
    }

    proptest! {
        /// Over any sequence of restarts at arbitrary (even backwards) wall
        /// times, ids stay unique and strictly increasing, and the string
        /// form sorts the same way.
        #[test]
        fn ids_unique_and_ordered_across_restarts(
            boots in proptest::collection::vec((0i64..4_000_000_000, 0usize..20), 1..20),
        ) {
            let mut all: Vec<CommandId> = vec![];
            for (secs, n) in boots {
                let now = jiff::Timestamp::from_second(secs).unwrap();
                let mut ids = CommandIds::starting_after(all.iter().max().copied(), now).unwrap();
                for _ in 0..n {
                    all.push(ids.next_id().unwrap());
                }
            }
            for w in all.windows(2) {
                prop_assert!(w[0] < w[1]);
                prop_assert!(w[0].to_string() < w[1].to_string());
            }
            for id in &all {
                let json = serde_json::to_string(id).unwrap();
                prop_assert_eq!(serde_json::from_str::<CommandId>(&json).unwrap(), *id);
            }
        }
    }
}
