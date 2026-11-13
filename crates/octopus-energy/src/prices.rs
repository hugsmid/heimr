//! Turning rate rows into per-slot prices.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::model::{PaymentMethod, Rate};
use crate::slot::HalfHour;

/// The unit rate for one half-hour slot.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SlotPrice {
    /// Slot.
    pub slot: HalfHour,
    /// p/kWh inc. VAT (may be negative on Agile).
    pub inc_vat: f64,
    /// p/kWh exc. VAT.
    pub exc_vat: f64,
}

/// A rate row whose boundaries split a half-hour slot.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("rate boundary {0} is not on a half-hour")]
pub struct SplitSlot(pub Timestamp);

/// Expand rate rows to one price per half-hour slot within `[from, to)`.
///
/// Works for Agile (one row per slot) and for flat or long-window rates
/// (one row covering many slots, `valid_to = None` meaning open-ended).
/// Rows for non-direct-debit payment are ignored when a direct-debit or
/// unqualified row covers the same slot. Slots with no covering rate (e.g.
/// tomorrow's Agile prices before ~16:00 UK) are simply omitted, so the
/// output may be shorter than the range.
pub fn slot_prices(
    rates: &[Rate],
    from: Timestamp,
    to: Timestamp,
) -> Result<Vec<SlotPrice>, SplitSlot> {
    let mut rows: Vec<&Rate> = rates
        .iter()
        .filter(|r| r.payment_method != Some(PaymentMethod::NonDirectDebit))
        .collect();
    if rows.is_empty() {
        rows = rates.iter().collect();
    }
    for r in &rows {
        HalfHour::new(r.valid_from).map_err(|_| SplitSlot(r.valid_from))?;
        if let Some(t) = r.valid_to {
            HalfHour::new(t).map_err(|_| SplitSlot(t))?;
        }
    }
    // Newest valid_from wins if rows overlap (a later revision supersedes).
    rows.sort_by_key(|r| std::cmp::Reverse(r.valid_from));

    Ok(HalfHour::range(from, to)
        .filter_map(|slot| {
            rows.iter()
                .find(|r| {
                    r.valid_from <= slot.start() && r.valid_to.is_none_or(|end| slot.end() <= end)
                })
                .map(|r| SlotPrice {
                    slot,
                    inc_vat: r.value_inc_vat,
                    exc_vat: r.value_exc_vat,
                })
        })
        .collect())
}

/// The `n` cheapest slots (ascending price, ties by time).
pub fn cheapest(prices: &[SlotPrice], n: usize) -> Vec<SlotPrice> {
    let mut v = prices.to_vec();
    v.sort_by(|a, b| a.inc_vat.total_cmp(&b.inc_vat).then(a.slot.cmp(&b.slot)));
    v.truncate(n);
    v
}

/// The cheapest contiguous run of `len` slots, e.g. for a 2-hour appliance
/// cycle use `len = 4`. Returns the slots and their mean price.
pub fn cheapest_window(prices: &[SlotPrice], len: usize) -> Option<(Vec<SlotPrice>, f64)> {
    if len == 0 {
        return None;
    }
    let mut sorted = prices.to_vec();
    sorted.sort_by_key(|p| p.slot);
    sorted
        .windows(len)
        .filter(|w| w.windows(2).all(|p| p[0].slot.next() == p[1].slot))
        .map(|w| {
            (
                w.to_vec(),
                w.iter().map(|p| p.inc_vat).sum::<f64>() / len as f64,
            )
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::SignedDuration;
    use proptest::prelude::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn rate(from: Timestamp, to: Option<Timestamp>, p: f64) -> Rate {
        Rate {
            value_exc_vat: p / 1.05,
            value_inc_vat: p,
            valid_from: from,
            valid_to: to,
            payment_method: None,
        }
    }

    #[test]
    fn flat_rate_expands_to_every_slot() {
        let r = [rate(ts("2026-01-01T00:00:00Z"), None, 24.5)];
        let p = slot_prices(&r, ts("2026-11-12T00:00:00Z"), ts("2026-11-13T00:00:00Z")).unwrap();
        assert_eq!(p.len(), 48);
        assert!(p.iter().all(|s| s.inc_vat == 24.5));
    }

    #[test]
    fn non_direct_debit_rows_are_ignored_when_dd_exists() {
        let from = ts("2026-01-01T00:00:00Z");
        let mut nd = rate(from, None, 30.0);
        nd.payment_method = Some(PaymentMethod::NonDirectDebit);
        let mut dd = rate(from, None, 25.0);
        dd.payment_method = Some(PaymentMethod::DirectDebit);
        let p = slot_prices(&[nd, dd], from, from + SignedDuration::from_hours(1)).unwrap();
        assert_eq!(
            p.iter().map(|s| s.inc_vat).collect::<Vec<_>>(),
            [25.0, 25.0]
        );
    }

    #[test]
    fn misaligned_rate_is_an_error() {
        let r = [rate(ts("2026-01-01T00:10:00Z"), None, 1.0)];
        assert!(slot_prices(&r, ts("2026-01-01T00:00:00Z"), ts("2026-01-01T01:00:00Z")).is_err());
    }

    #[test]
    fn cheapest_window_requires_contiguity() {
        let t = ts("2026-01-01T00:00:00Z");
        let mk = |i: i64, p: f64| SlotPrice {
            slot: HalfHour::new(t + SignedDuration::from_mins(30 * i)).unwrap(),
            inc_vat: p,
            exc_vat: p,
        };
        // Slots 0,1 cheap but slot 2 missing; 3,4,5 moderately cheap.
        let prices = [
            mk(0, 1.0),
            mk(1, 1.0),
            mk(3, 5.0),
            mk(4, 5.0),
            mk(5, 5.0),
            mk(6, 50.0),
        ];
        let (w, mean) = cheapest_window(&prices, 3).unwrap();
        assert_eq!(w[0].slot, mk(3, 0.0).slot);
        assert_eq!(mean, 5.0);
        assert!(cheapest_window(&prices, 0).is_none());
    }

    proptest! {
        /// Agile-style input: one row per slot. Every published slot comes
        /// out exactly once with its own price; totals are conserved.
        #[test]
        fn per_slot_rates_round_trip(
            start_slot in 0i64..100_000,
            prices in prop::collection::vec(-30.0f64..100.0, 1..200),
        ) {
            let t0 = Timestamp::from_second(1_700_000_000 / 1800 * 1800 + start_slot * 1800).unwrap();
            let rates: Vec<Rate> = prices.iter().enumerate().map(|(i, p)| {
                let s = t0 + SignedDuration::from_mins(30 * i as i64);
                rate(s, Some(s + SignedDuration::from_mins(30)), *p)
            }).rev().collect(); // API order: newest first
            let end = t0 + SignedDuration::from_mins(30 * prices.len() as i64);
            let out = slot_prices(&rates, t0, end).unwrap();
            prop_assert_eq!(out.len(), prices.len());
            for (o, p) in out.iter().zip(&prices) {
                prop_assert_eq!(o.inc_vat, *p);
            }
            let total: f64 = out.iter().map(|s| s.inc_vat).sum();
            prop_assert!((total - prices.iter().sum::<f64>()).abs() < 1e-6);

            let cheapest_n = cheapest(&out, 5);
            let min = prices.iter().cloned().fold(f64::INFINITY, f64::min);
            prop_assert_eq!(cheapest_n[0].inc_vat, min);
        }

        /// Requesting a window larger than the published prices yields only
        /// the published slots, never invented ones.
        #[test]
        fn unpublished_slots_are_omitted(n in 1usize..96, extra in 1usize..96) {
            let t0 = ts("2026-11-12T00:00:00Z");
            let rates: Vec<Rate> = (0..n).map(|i| {
                let s = t0 + SignedDuration::from_mins(30 * i as i64);
                rate(s, Some(s + SignedDuration::from_mins(30)), 10.0)
            }).collect();
            let end = t0 + SignedDuration::from_mins(30 * (n + extra) as i64);
            prop_assert_eq!(slot_prices(&rates, t0, end).unwrap().len(), n);
        }
    }
}
