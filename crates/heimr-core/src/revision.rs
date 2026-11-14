//! Revision markers for sources that publish none: [`Revisions`].
//!
//! A versioned series needs a `changed_at` on every reading or forecast
//! that is the same each time one revision is re-sent and higher for each
//! new revision ([`Reading::with_changed_at`](crate::Reading::with_changed_at)).
//! When the source's API exposes no revision marker, the adapter makes one:
//! the time it first saw that revision of the value, kept per key in its
//! state directory.
//!
//! [`Revisions`] keeps, per `(series key, t)`, only the **last** value seen
//! and its `changed_at`:
//!
//! - an unchanged value (the same bits) reuses the stored `changed_at`, so
//!   a re-send is identical;
//! - a changed value mints `max(now, stored + 1)` in nanoseconds and
//!   replaces the entry, so the marker strictly increases per key even if
//!   the clock steps back;
//! - a value that returns to an earlier one (A, then B, then A again) is a
//!   new revision: there is no history keyed by value, which would hand the
//!   final A its old marker and leave B current.
//!
//! It is plain data (serde): the adapter writes it durably to its state
//! directory before it emits what it minted. If the saved map is lost or
//! unreadable, start a new one and report health degraded: values seen
//! again then get new, higher markers, so they look newer than they are
//! (redundant revisions of the same value; nothing is reverted).
//!
//! ```
//! use heimr_core::Revisions;
//! use jiff::{SignedDuration, Timestamp};
//!
//! let slot: Timestamp = "2030-01-02T00:00:00Z".parse().unwrap();
//! let mut now: Timestamp = "2030-01-01T16:00:00Z".parse().unwrap();
//! let mut revs = Revisions::new();
//! let a = revs.observe("unit_rate", slot, 0.20, now);
//! assert!(a.minted);
//! now += SignedDuration::from_hours(1);
//! let again = revs.observe("unit_rate", slot, 0.20, now); // re-fetched, unchanged
//! assert_eq!((again.changed_at, again.minted), (a.changed_at, false));
//! let corrected = revs.observe("unit_rate", slot, 0.19, now);
//! assert!(corrected.minted && corrected.changed_at > a.changed_at);
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::record::revision_time;

/// The revision of one value: its marker and when it was first seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Revision {
    /// The marker to send as `changed_at`.
    pub changed_at: u64,
    /// When this revision was first seen (the wall time it was minted
    /// at). A forecast's `issued_at`, when the source reports none.
    pub first_seen: Timestamp,
    /// Whether this call minted it (a new key or a changed value).
    pub minted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Entry {
    bits: u64,
    changed_at: u64,
    first_seen: Timestamp,
}

/// Per `(series key, t)`, the last value seen and its revision. See the
/// [module docs](self).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "Vec<Row>", into = "Vec<Row>")]
pub struct Revisions {
    entries: BTreeMap<(String, Timestamp), Entry>,
}

/// One saved entry (the on-disk shape).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Row {
    key: String,
    t: Timestamp,
    /// The value's bits, so NaN payloads and `-0.0` survive.
    bits: u64,
    changed_at: u64,
    first_seen: Timestamp,
}

impl From<Vec<Row>> for Revisions {
    fn from(rows: Vec<Row>) -> Self {
        let entries = rows
            .into_iter()
            .map(|r| {
                (
                    (r.key, r.t),
                    Entry {
                        bits: r.bits,
                        changed_at: r.changed_at,
                        first_seen: r.first_seen,
                    },
                )
            })
            .collect();
        Self { entries }
    }
}

impl From<Revisions> for Vec<Row> {
    fn from(r: Revisions) -> Self {
        r.entries
            .into_iter()
            .map(|((key, t), e)| Row {
                key,
                t,
                bits: e.bits,
                changed_at: e.changed_at,
                first_seen: e.first_seen,
            })
            .collect()
    }
}

impl Revisions {
    /// An empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// The revision of `value` at `(key, t)`, seen at `now`: the stored one
    /// if the value is bit-identical to the last one seen there, else a new
    /// one minted at `max(now, stored + 1)` (nanoseconds since the epoch),
    /// which replaces the entry.
    pub fn observe(&mut self, key: &str, t: Timestamp, value: f64, now: Timestamp) -> Revision {
        let bits = value.to_bits();
        let slot = (key.to_owned(), t);
        let previous = self.entries.get(&slot).copied();
        if let Some(e) = previous
            && e.bits == bits
        {
            return Revision {
                changed_at: e.changed_at,
                first_seen: e.first_seen,
                minted: false,
            };
        }
        let floor = previous.map_or(0, |e| e.changed_at.saturating_add(1));
        let entry = Entry {
            bits,
            changed_at: revision_time(now).max(floor),
            first_seen: now,
        };
        self.entries.insert(slot, entry);
        Revision {
            changed_at: entry.changed_at,
            first_seen: entry.first_seen,
            minted: true,
        }
    }

    /// The revision stored for `(key, t)`, without changing anything.
    pub fn get(&self, key: &str, t: Timestamp) -> Option<Revision> {
        self.entries.get(&(key.to_owned(), t)).map(|e| Revision {
            changed_at: e.changed_at,
            first_seen: e.first_seen,
            minted: false,
        })
    }

    /// Forget every entry with `t` before `earliest`: the map stays bounded
    /// to the adapter's fetch window.
    pub fn retain_from(&mut self, earliest: Timestamp) {
        self.entries.retain(|(_, t), _| *t >= earliest);
    }

    /// Keep only the entries for which `keep(key, t)` holds (for maps
    /// holding series with different windows).
    pub fn retain(&mut self, mut keep: impl FnMut(&str, Timestamp) -> bool) {
        self.entries.retain(|(k, t), _| keep(k, *t));
    }

    /// Load the map saved as `dir/name`: empty if there is none yet, `Err`
    /// naming the file and the problem if it is empty or unreadable (then
    /// start a new map and report health degraded).
    pub fn load(dir: &Path, name: &str) -> Result<Self, String> {
        let path = dir.join(name);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::new()),
            Err(e) => return Err(format!("revisions {} unreadable: {e}", path.display())),
        };
        serde_json::from_str(&text)
            .map_err(|e| format!("revisions {} unreadable: {e}", path.display()))
    }

    /// Save the map as `dir/name`, durably ([`write_durably`]). Send
    /// nothing minted since the last save until this succeeds.
    pub fn save(&self, dir: &Path, name: &str) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        write_durably(dir, name, &bytes)
    }

    /// How many entries are kept.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no entry is kept.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Replace `dir/name` with `bytes` so that a crash leaves the old or the
/// new contents, never a mix: write a temporary file (`name.tmp`), sync
/// it, rename it over the old one, then (on unix) sync the directory so
/// the rename itself is durable. Elsewhere the standard library cannot
/// open a directory to sync it, so the rename is as durable as the
/// platform makes it.
pub fn write_durably(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = dir.join(format!("{name}.tmp"));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, dir.join(name))?;
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::SignedDuration;

    fn t(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn a_return_to_an_earlier_value_is_a_new_revision() {
        let slot = t("2030-01-02T00:00:00Z");
        let mut now = t("2030-01-01T00:00:00Z");
        let mut revs = Revisions::new();
        let mut marks = Vec::new();
        for v in [0.2, 0.3, 0.2] {
            let r = revs.observe("rate", slot, v, now);
            assert!(r.minted, "{v}");
            marks.push(r.changed_at);
            now += SignedDuration::from_mins(1);
        }
        assert!(marks[0] < marks[1] && marks[1] < marks[2], "{marks:?}");
        // The final A is current: re-observing it keeps its marker.
        let again = revs.observe("rate", slot, 0.2, now);
        assert_eq!((again.changed_at, again.minted), (marks[2], false));
        assert_eq!(revs.len(), 1, "one entry per key, never per value");
    }

    #[test]
    fn a_backward_clock_step_still_mints_higher() {
        let slot = t("2030-01-02T00:00:00Z");
        let mut revs = Revisions::new();
        let first = revs.observe("rate", slot, 1.0, t("2030-01-01T12:00:00Z"));
        // The clock steps back an hour; a changed value must still outrank.
        let back = t("2030-01-01T11:00:00Z");
        let second = revs.observe("rate", slot, 2.0, back);
        assert_eq!(second.changed_at, first.changed_at + 1);
        assert_eq!(second.first_seen, back);
        let third = revs.observe("rate", slot, 3.0, back);
        assert_eq!(third.changed_at, first.changed_at + 2);
        // Once the clock is past it again, the clock wins.
        let later = t("2030-01-01T13:00:00Z");
        assert_eq!(
            revs.observe("rate", slot, 4.0, later).changed_at,
            revision_time(later)
        );
    }

    #[test]
    fn identity_is_by_bits_and_per_key() {
        let slot = t("2030-01-02T00:00:00Z");
        let now = t("2030-01-01T00:00:00Z");
        let later = now + SignedDuration::from_hours(1);
        let mut revs = Revisions::new();
        let nan = revs.observe("a", slot, f64::NAN, now);
        assert!(
            !revs.observe("a", slot, f64::NAN, later).minted,
            "NaN repeats"
        );
        assert!(revs.observe("a", slot, 0.0, later).minted);
        assert!(revs.observe("a", slot, -0.0, later).minted, "-0.0 differs");
        // Another key, or another t, is independent.
        assert_eq!(
            revs.observe("b", slot, f64::NAN, later).changed_at,
            revision_time(later)
        );
        let other = slot + SignedDuration::from_mins(30);
        assert!(revs.observe("a", other, 1.0, later).minted);
        assert_eq!(nan.first_seen, now);
        assert_eq!(revs.get("b", slot).map(|r| r.first_seen), Some(later));
        assert_eq!(revs.get("c", slot), None);
    }

    #[test]
    fn bounded_and_saved_whole() {
        let mut revs = Revisions::new();
        let now = t("2030-01-01T00:00:00Z");
        for h in 0..4 {
            revs.observe("x", now + SignedDuration::from_hours(h), -0.0, now);
        }
        revs.retain_from(now + SignedDuration::from_hours(2));
        assert_eq!(revs.len(), 2);
        revs.observe("y", now, 1.0, now);
        revs.retain(|k, t| k != "x" || t > now + SignedDuration::from_hours(2));
        assert_eq!(revs.len(), 2, "one x and the y");
        let json = serde_json::to_string(&revs).unwrap();
        let back: Revisions = serde_json::from_str(&json).unwrap();
        assert_eq!(back, revs);
        assert_eq!(
            back.get("x", now + SignedDuration::from_hours(3))
                .unwrap()
                .changed_at,
            revision_time(now)
        );
        assert!(serde_json::from_str::<Revisions>("{ not json").is_err());
        // Saved and loaded as a file; garbage is reported naming it.
        let dir = std::env::temp_dir().join(format!("heimr-core-revisions-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(Revisions::load(&dir, "r.json"), Ok(Revisions::new()));
        back.save(&dir, "r.json").unwrap();
        assert_eq!(Revisions::load(&dir, "r.json"), Ok(back.clone()));
        assert!(!dir.join("r.json.tmp").exists());
        std::fs::write(dir.join("r.json"), "").unwrap();
        let e = Revisions::load(&dir, "r.json").unwrap_err();
        assert!(e.contains("r.json") && e.contains("unreadable"), "{e}");
        // A blocked temporary file fails the save; the old file stays.
        std::fs::create_dir(dir.join("r.json.tmp")).unwrap();
        assert!(back.save(&dir, "r.json").is_err());
        assert_eq!(std::fs::read_to_string(dir.join("r.json")).unwrap(), "");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(Revisions::new().is_empty());
    }
}
