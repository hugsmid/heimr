//! The durable command audit trail.
//!
//! The [`Actuator`](crate::Actuator) writes an [`AuditRecord`] for every
//! command it decides on (delivered, shadowed or rejected) and awaits
//! [`AuditLog::append`] before delivering: `append` resolves only once the
//! record is durable, and a command whose record cannot be made durable
//! within the actuator's deadline is withheld. A later
//! [`AuditKind::Dropped`] record with the same id says that an accepted
//! command never reached its instance.
//!
//! At start-up the actuator reads the recent tail ([`AuditLog::tail`]) to
//! rebuild its rate limits and to choose a boot number above every id
//! already issued. Entries an implementation cannot read (torn, unparsable,
//! or written in a format version it does not know) are reported as
//! [`UnreadableEntry`]s, with whatever could be salvaged, and the actuator
//! counts each as an accepted command: never fewer limits than were real.
//!
//! The audit trail has no shared queue with adapter telemetry: an adapter
//! flooding the record channel cannot delay or block an audit write.
//!
//! Implementations: [`MemoryAuditLog`] (tests) here; the `heimr` app ships
//! an fsynced JSON-lines journal in its state directory.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::actuator::{Mode, Rejection};
use crate::clock::BoxFuture;
use crate::domain::{Command, Hold};
use crate::id::CommandId;
use crate::record::Event;

/// Which kind of audit record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AuditKind {
    /// The actuator's decision on a submitted command.
    #[serde(rename = "command")]
    Decision,
    /// An accepted command did not reach its instance.
    #[serde(rename = "command_dropped")]
    Dropped,
}

impl AuditKind {
    /// The matching [`Event`] kind.
    pub fn event_kind(self) -> &'static str {
        match self {
            Self::Decision => Event::COMMAND,
            Self::Dropped => Event::COMMAND_DROPPED,
        }
    }
}

/// What the actuator decided, or what became of an accepted command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AuditResult {
    /// Accepted for a live instance. Recorded before delivery; a
    /// [`AuditKind::Dropped`] record with the same id follows if it never
    /// reached the instance.
    Delivered,
    /// Accepted, logged only (shadow mode).
    Shadowed,
    /// Refused; nothing was delivered.
    Rejected {
        /// Why.
        rejection: Rejection,
    },
    /// Not delivered after all.
    Dropped {
        /// Why.
        why: String,
    },
}

/// One entry of the audit trail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AuditRecord {
    /// Decision or drop.
    pub kind: AuditKind,
    /// Command id (unique across restarts).
    pub id: CommandId,
    /// When (wall time).
    pub time: Timestamp,
    /// Adapter instance addressed.
    pub adapter: String,
    /// Who asked (`planner`, `assistant`, `user`, …); `actuator` for drops.
    pub actor: String,
    /// The instance's mode, if it is registered.
    pub mode: Option<Mode>,
    /// Zone or device addressed (the rate-limit key).
    pub target: String,
    /// The command.
    pub command: Command,
    /// The command's hold, if it overrides a device.
    pub hold: Option<Hold>,
    /// Why, in words.
    pub reason: String,
    /// The inputs the decision was based on.
    pub inputs: serde_json::Value,
    /// Outcome.
    pub result: AuditResult,
}

impl AuditRecord {
    /// Whether this is an accepted decision (it counts against rate limits).
    pub fn accepted(&self) -> bool {
        self.kind == AuditKind::Decision
            && matches!(self.result, AuditResult::Delivered | AuditResult::Shadowed)
    }

    /// The record as an [`Event`] for sinks (kind `command` or
    /// `command_dropped`, payload = the record).
    pub fn to_event(&self) -> Event {
        Event::new(
            self.time,
            self.kind.event_kind(),
            self.actor.clone(),
            serde_json::to_value(self).unwrap_or_default(),
        )
    }
}

/// An audit write or read failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AuditError {
    /// I/O failed.
    #[error("audit log I/O: {0}")]
    Io(#[from] std::io::Error),
    /// The record could not be encoded.
    #[error("audit record could not be encoded: {0}")]
    Encode(String),
    /// The log is closed or otherwise unavailable.
    #[error("audit log unavailable: {0}")]
    Unavailable(String),
}

/// An audit entry that could not be read as an [`AuditRecord`] (torn,
/// unparsable, or of an unknown format version), with what could be
/// salvaged from it. `None` means unknown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct UnreadableEntry {
    /// When it was written, if readable.
    pub time: Option<Timestamp>,
    /// The instance it addressed, if readable.
    pub adapter: Option<String>,
    /// The zone or device it addressed, if readable.
    pub target: Option<String>,
}

impl UnreadableEntry {
    /// An entry about which nothing is known.
    pub fn unknown() -> Self {
        Self::default()
    }

    /// With what was salvaged.
    pub fn new(time: Option<Timestamp>, adapter: Option<String>, target: Option<String>) -> Self {
        Self {
            time,
            adapter,
            target,
        }
    }
}

/// What [`AuditLog::tail`] found.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct AuditTail {
    /// The highest command id anywhere in the trail (including ids salvaged
    /// from unreadable entries).
    pub last_id: Option<CommandId>,
    /// Records with `time >= since`, in append order.
    pub records: Vec<AuditRecord>,
    /// Unreadable entries that may be at or after `since` (those whose time
    /// could be read and is earlier are left out). Each counts as an
    /// accepted command.
    pub unknown: Vec<UnreadableEntry>,
    /// How many entries could not be read in total.
    pub skipped: usize,
}

impl AuditTail {
    /// A tail.
    pub fn new(last_id: Option<CommandId>, records: Vec<AuditRecord>) -> Self {
        Self {
            last_id,
            records,
            unknown: Vec::new(),
            skipped: 0,
        }
    }

    /// Record unreadable entries that may be recent.
    #[must_use]
    pub fn with_unknown(mut self, unknown: Vec<UnreadableEntry>) -> Self {
        self.unknown = unknown;
        self
    }

    /// Record how many entries could not be parsed.
    #[must_use]
    pub fn with_skipped(mut self, skipped: usize) -> Self {
        self.skipped = skipped;
        self
    }
}

/// A durable, append-only audit trail.
pub trait AuditLog: Send + Sync + fmt::Debug + 'static {
    /// Append a record. Resolves only once the record is durable (e.g.
    /// written and fsynced). The caller applies a deadline.
    fn append(&self, record: AuditRecord) -> BoxFuture<Result<(), AuditError>>;

    /// The highest id ever written, and the records written at or after
    /// `since` (wall time).
    fn tail(&self, since: Timestamp) -> BoxFuture<Result<AuditTail, AuditError>>;
}

/// An in-memory [`AuditLog`] for tests. Clones share the records, so one
/// clone can outlive an actuator and seed the next (a simulated restart).
#[derive(Debug, Clone, Default)]
pub struct MemoryAuditLog {
    records: Arc<Mutex<Vec<AuditRecord>>>,
}

impl MemoryAuditLog {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every record, in append order.
    pub fn records(&self) -> Vec<AuditRecord> {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl AuditLog for MemoryAuditLog {
    fn append(&self, record: AuditRecord) -> BoxFuture<Result<(), AuditError>> {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(record);
        Box::pin(std::future::ready(Ok(())))
    }

    fn tail(&self, since: Timestamp) -> BoxFuture<Result<AuditTail, AuditError>> {
        let records = self.records();
        let last_id = records.iter().map(|r| r.id).max();
        let recent = records.into_iter().filter(|r| r.time >= since).collect();
        Box::pin(std::future::ready(Ok(AuditTail::new(last_id, recent))))
    }
}
