//! Half-hour settlement slots.
//!
//! Octopus prices (Agile) and smart-meter consumption are published per
//! half-hour. Slots are aligned to **UTC** half-hours, which are also UK
//! half-hours in both GMT and BST because the offset is a whole hour. A UK
//! calendar day therefore has 48 slots, except the day the clocks go forward
//! (46) and the day they go back (50).

use std::fmt;

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

/// Length of one settlement slot.
pub const SLOT: SignedDuration = SignedDuration::from_mins(30);
const SLOT_SECS: i64 = 30 * 60;

/// The start of a half-hour slot, guaranteed to lie on a UTC `:00` or `:30`
/// boundary with zero seconds.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "Timestamp", into = "Timestamp")]
pub struct HalfHour(Timestamp);

/// A timestamp that is not on a half-hour boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not aligned to a UTC half-hour")]
pub struct Misaligned(pub Timestamp);

impl HalfHour {
    /// The slot containing `t` (rounds down).
    pub fn containing(t: Timestamp) -> Self {
        let secs = t.as_second();
        // Sub-second part counts as "inside" the slot; floor the seconds.
        let floored = secs.div_euclid(SLOT_SECS) * SLOT_SECS;
        Self(Timestamp::from_second(floored).expect("floored timestamp is in range"))
    }

    /// `t` as a slot start, if it is exactly aligned.
    pub fn new(t: Timestamp) -> Result<Self, Misaligned> {
        if t.subsec_nanosecond() == 0 && t.as_second().rem_euclid(SLOT_SECS) == 0 {
            Ok(Self(t))
        } else {
            Err(Misaligned(t))
        }
    }

    /// The first slot starting at or after `t` (rounds up).
    pub fn at_or_after(t: Timestamp) -> Self {
        let s = Self::containing(t);
        if s.0 == t { s } else { s.next() }
    }

    /// Slot start.
    pub fn start(self) -> Timestamp {
        self.0
    }

    /// Slot end (exclusive) — the next slot's start.
    pub fn end(self) -> Timestamp {
        self.0 + SLOT
    }

    /// The following slot.
    pub fn next(self) -> Self {
        Self(self.end())
    }

    /// The preceding slot.
    pub fn prev(self) -> Self {
        Self(self.0 - SLOT)
    }

    /// Whether `t` falls inside this slot (`start <= t < end`).
    pub fn contains(self, t: Timestamp) -> bool {
        self.0 <= t && t < self.end()
    }

    /// Slots covering `[from, to)`: the slot containing `from` up to, but
    /// excluding, the slot starting at or after `to`.
    /// An empty or inverted window (`from >= to`) yields no slots.
    pub fn range(from: Timestamp, to: Timestamp) -> SlotRange {
        let next = Self::containing(from);
        let end = if from < to {
            Self::at_or_after(to)
        } else {
            next
        };
        SlotRange { next, end }
    }

    /// All slots of a calendar day in `tz` (46, 48 or 50 for Europe/London).
    pub fn local_day(date: Date, tz: &TimeZone) -> Result<SlotRange, jiff::Error> {
        let (start, end) = local_day_bounds(date, tz)?;
        Ok(Self::range(start, end))
    }
}

/// UTC instants of midnight-to-midnight for `date` in `tz`.
pub fn local_day_bounds(date: Date, tz: &TimeZone) -> Result<(Timestamp, Timestamp), jiff::Error> {
    let start = date.to_zoned(tz.clone())?.timestamp();
    let end = date.tomorrow()?.to_zoned(tz.clone())?.timestamp();
    Ok((start, end))
}

impl fmt::Debug for HalfHour {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HalfHour({})", self.0)
    }
}

impl fmt::Display for HalfHour {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl TryFrom<Timestamp> for HalfHour {
    type Error = Misaligned;
    fn try_from(t: Timestamp) -> Result<Self, Misaligned> {
        Self::new(t)
    }
}

impl From<HalfHour> for Timestamp {
    fn from(s: HalfHour) -> Timestamp {
        s.0
    }
}

/// Iterator over consecutive slots, see [`HalfHour::range`].
#[derive(Debug, Clone)]
pub struct SlotRange {
    next: HalfHour,
    end: HalfHour,
}

impl Iterator for SlotRange {
    type Item = HalfHour;

    fn next(&mut self) -> Option<HalfHour> {
        if self.next >= self.end {
            return None;
        }
        let s = self.next;
        self.next = s.next();
        Some(s)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = if self.next >= self.end {
            0
        } else {
            let secs = self.end.0.as_second() - self.next.0.as_second();
            usize::try_from(secs / SLOT_SECS).unwrap_or(usize::MAX)
        };
        (n, Some(n))
    }
}

impl ExactSizeIterator for SlotRange {}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn uk() -> TimeZone {
        TimeZone::get("Europe/London").expect("tzdb has Europe/London")
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().expect("timestamp")
    }

    #[test]
    fn uk_days_have_46_48_or_50_slots() {
        let tz = uk();
        let n = |d: &str| HalfHour::local_day(d.parse().unwrap(), &tz).unwrap().len();
        assert_eq!(n("2026-03-29"), 46); // clocks forward
        assert_eq!(n("2026-03-30"), 48);
        assert_eq!(n("2026-07-01"), 48);
        assert_eq!(n("2026-10-25"), 50); // clocks back
        assert_eq!(n("2026-12-25"), 48);
    }

    #[test]
    fn bst_day_starts_at_2300_utc() {
        let first = HalfHour::local_day("2026-07-01".parse().unwrap(), &uk())
            .unwrap()
            .next()
            .unwrap();
        assert_eq!(first.start(), ts("2026-06-30T23:00:00Z"));
    }

    #[test]
    fn rejects_misaligned() {
        assert!(HalfHour::new(ts("2026-01-01T00:30:00Z")).is_ok());
        assert!(HalfHour::new(ts("2026-01-01T00:15:00Z")).is_err());
        assert!(HalfHour::new(ts("2026-01-01T00:30:00.5Z")).is_err());
        let json = serde_json::to_string(&ts("2026-01-01T00:10:00Z")).unwrap();
        assert!(serde_json::from_str::<HalfHour>(&json).is_err());
    }

    fn any_ts() -> impl Strategy<Value = Timestamp> {
        // 2000-01-01 .. 2100-01-01, nanosecond resolution.
        (946_684_800i64..4_102_444_800i64, 0i32..1_000_000_000)
            .prop_map(|(s, ns)| Timestamp::new(s, ns).unwrap())
    }

    proptest! {
        #[test]
        fn containing_brackets_the_instant(t in any_ts()) {
            let s = HalfHour::containing(t);
            prop_assert!(s.contains(t));
            prop_assert!(HalfHour::new(s.start()).is_ok());
            prop_assert_eq!(HalfHour::containing(s.start()), s);
        }

        #[test]
        fn empty_window_has_no_slots(t in any_ts()) {
            prop_assert_eq!(HalfHour::range(t, t).len(), 0);
        }

        #[test]
        fn at_or_after_is_the_least_upper_slot(t in any_ts()) {
            let s = HalfHour::at_or_after(t);
            prop_assert!(s.start() >= t);
            prop_assert!(s.prev().start() < t);
        }

        #[test]
        fn ranges_are_contiguous_and_cover(a in any_ts(), len_min in 0i64..20_000) {
            let b = a + SignedDuration::from_mins(len_min);
            let slots: Vec<_> = HalfHour::range(a, b).collect();
            prop_assert_eq!(slots.len(), HalfHour::range(a, b).len());
            for w in slots.windows(2) {
                prop_assert_eq!(w[0].next(), w[1]);
            }
            if a < b {
                prop_assert!(slots.first().unwrap().contains(a));
                prop_assert!(slots.last().unwrap().end() >= b);
                prop_assert!(slots.last().unwrap().start() < b);
            } else {
                prop_assert!(slots.is_empty());
            }
        }

        /// Every UK day splits into whole slots, and the count matches the
        /// day's real length — i.e. DST transitions never split a slot.
        #[test]
        fn uk_day_slot_count_matches_day_length(days in 0i32..36_500) {
            let tz = uk();
            let date = Date::constant(2000, 1, 1).checked_add(jiff::Span::new().days(days)).unwrap();
            let (start, end) = local_day_bounds(date, &tz).unwrap();
            let slots: Vec<_> = HalfHour::local_day(date, &tz).unwrap().collect();
            prop_assert_eq!(slots.first().unwrap().start(), start);
            prop_assert_eq!(slots.last().unwrap().end(), end);
            let mins = end.duration_since(start).as_mins();
            prop_assert_eq!(slots.len() as i64 * 30, mins);
            prop_assert!(matches!(slots.len(), 46 | 48 | 50));
        }
    }
}
