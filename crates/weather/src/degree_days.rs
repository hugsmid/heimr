//! Heating degree-days from hourly observations.
//!
//! Uses the integration method: each hourly temperature below the base
//! contributes `(base − T) / 24` degree-days. This is the method
//! recommended when hourly data is available; it differs slightly from the
//! daily max/min approximations behind some published figures.
//!
//! Rows are assumed to be hourly (as Open-Meteo returns them). Rows without
//! a temperature are skipped and counted in [`DegreeDays::missing_hours`],
//! so callers can decide whether coverage is good enough.

use std::collections::BTreeMap;

use jiff::civil::Date;
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};

use crate::model::Observation;

// Common base temperatures. The function takes any base; use the one that
// matches the figures you compare against.

/// Base temperature of 15.5 °C, a common convention for heating degree-days.
pub const BASE_15_5_C: f64 = 15.5;
/// Base temperature of 18 °C, a common convention for heating degree-days.
pub const BASE_18_C: f64 = 18.0;
/// Base temperature of 65 °F (about 18.3 °C), a common convention where
/// degree-days are counted in Fahrenheit; the value here is in °C.
pub const BASE_65_F: f64 = (65.0 - 32.0) * 5.0 / 9.0;

/// Heating degree-days over a set of hourly rows.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct DegreeDays {
    /// Heating degree-days (K·day). Never negative.
    pub value: f64,
    /// Hours that had a temperature.
    pub hours: usize,
    /// Hours skipped because the temperature was missing.
    pub missing_hours: usize,
}

impl DegreeDays {
    fn add(&mut self, temperature_c: Option<f64>, base_c: f64) {
        match temperature_c {
            Some(t) => {
                self.value += (base_c - t).max(0.0) / 24.0;
                self.hours += 1;
            }
            None => self.missing_hours += 1,
        }
    }
}

/// Heating degree-days over `obs` with base temperature `base_c`, in °C (see
/// [`BASE_15_5_C`], [`BASE_18_C`] and [`BASE_65_F`] for common choices).
///
/// ```
/// use weather::{heating_degree_days, Observation, BASE_15_5_C};
/// # let t0: jiff::Timestamp = "2026-01-10T00:00:00Z".parse().unwrap();
/// // A whole day at 5.5 °C is 10 degree-days.
/// let day: Vec<Observation> = (0..24)
///     .map(|h| Observation {
///         time: t0 + jiff::SignedDuration::from_hours(h),
///         temperature_c: Some(5.5),
///         humidity_pct: None, wind_speed_ms: None, wind_dir_deg: None,
///         precip_mm: None, cloud_cover_pct: None, shortwave_wm2: None,
///         pressure_hpa: None,
///     })
///     .collect();
/// let dd = heating_degree_days(&day, BASE_15_5_C);
/// assert!((dd.value - 10.0).abs() < 1e-9);
/// assert_eq!((dd.hours, dd.missing_hours), (24, 0));
/// ```
pub fn heating_degree_days(obs: &[Observation], base_c: f64) -> DegreeDays {
    let mut dd = DegreeDays::default();
    for o in obs {
        dd.add(o.temperature_c, base_c);
    }
    dd
}

/// Heating degree-days per calendar day in `tz` (e.g. `Europe/Berlin`), in
/// date order. A row belongs to the local date of its timestamp, so a
/// complete local day has 23, 24 or 25 hours.
pub fn daily_heating_degree_days(
    obs: &[Observation],
    base_c: f64,
    tz: &TimeZone,
) -> Vec<(Date, DegreeDays)> {
    let mut days: BTreeMap<Date, DegreeDays> = BTreeMap::new();
    for o in obs {
        let date = o.time.to_zoned(tz.clone()).date();
        days.entry(date).or_default().add(o.temperature_c, base_c);
    }
    days.into_iter().collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use jiff::{SignedDuration, Timestamp};
    use proptest::prelude::*;

    fn obs(start: &str, temps: &[Option<f64>]) -> Vec<Observation> {
        let t0: Timestamp = start.parse().unwrap();
        temps
            .iter()
            .enumerate()
            .map(|(h, &t)| Observation {
                time: t0 + SignedDuration::from_hours(h as i64),
                temperature_c: t,
                humidity_pct: None,
                wind_speed_ms: None,
                wind_dir_deg: None,
                precip_mm: None,
                cloud_cover_pct: None,
                shortwave_wm2: None,
                pressure_hpa: None,
            })
            .collect()
    }

    #[test]
    fn warm_hours_contribute_nothing_and_nulls_are_counted() {
        let mut temps = vec![Some(3.5); 12];
        temps.extend(vec![Some(20.0); 11]);
        temps.push(None);
        let dd = heating_degree_days(&obs("2026-01-10T00:00:00Z", &temps), BASE_15_5_C);
        // 12 h × 12 K / 24 = 6.
        assert!((dd.value - 6.0).abs() < 1e-9);
        assert_eq!((dd.hours, dd.missing_hours), (23, 1));
        assert_eq!(heating_degree_days(&[], 15.5), DegreeDays::default());
    }

    #[test]
    fn base_temperature_is_a_parameter() {
        let rows = obs("2026-01-10T00:00:00Z", &[Some(10.0); 24]);
        assert!((heating_degree_days(&rows, 15.5).value - 5.5).abs() < 1e-9);
        assert!((heating_degree_days(&rows, 18.0).value - 8.0).abs() < 1e-9);
        assert_eq!(heating_degree_days(&rows, 10.0).value, 0.0);
    }

    #[test]
    fn daily_split_follows_local_days_across_dst() {
        let tz = TimeZone::get("Europe/Berlin").unwrap();
        // 2026-10-25 (clocks go back) is 25 hours long there; local midnight
        // is 22:00 UTC the day before (CEST, UTC+2).
        let rows = obs("2026-10-24T22:00:00Z", &[Some(5.5); 25 + 24]);
        let days = daily_heating_degree_days(&rows, 15.5, &tz);
        let hours: Vec<_> = days
            .iter()
            .map(|(d, dd)| (d.to_string(), dd.hours))
            .collect();
        assert_eq!(
            hours,
            [("2026-10-25".to_owned(), 25), ("2026-10-26".to_owned(), 24)]
        );
        assert!((days[0].1.value - 25.0 * 10.0 / 24.0).abs() < 1e-9);
    }

    proptest! {
        /// Non-negative, monotone in the base temperature, additive over
        /// concatenation, and equal to the sum of the daily split.
        #[test]
        fn degree_day_laws(
            temps in proptest::collection::vec(proptest::option::of(-20.0f64..35.0), 0..200),
            base in 0.0f64..25.0,
            bump in 0.0f64..5.0,
            cut in any::<prop::sample::Index>(),
        ) {
            let rows = obs("2026-01-01T00:00:00Z", &temps);
            let dd = heating_degree_days(&rows, base);
            prop_assert!(dd.value >= 0.0);
            prop_assert_eq!(dd.hours + dd.missing_hours, rows.len());
            prop_assert!(heating_degree_days(&rows, base + bump).value >= dd.value - 1e-9);

            let k = if rows.is_empty() { 0 } else { cut.index(rows.len() + 1) };
            let (a, b) = rows.split_at(k);
            let sum = heating_degree_days(a, base).value + heating_degree_days(b, base).value;
            prop_assert!((sum - dd.value).abs() < 1e-9);

            let daily: f64 = daily_heating_degree_days(&rows, base, &TimeZone::UTC)
                .iter()
                .map(|(_, d)| d.value)
                .sum();
            prop_assert!((daily - dd.value).abs() < 1e-9);
        }
    }
}
