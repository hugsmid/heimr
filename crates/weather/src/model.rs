//! Response shape and typed hourly rows.
//!
//! Open-Meteo answers column-oriented: `hourly.time[]` plus one array per
//! variable, with `hourly_units` naming each column's unit. [`HourlyResponse`]
//! validates that shape (every column present, same length as `time`, the
//! unit this crate asked for, strictly increasing times) and converts it to
//! row structs.

use std::collections::BTreeMap;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Hourly variables requested from Open-Meteo, in request order.
pub const HOURLY_VARIABLES: [&str; 8] = [
    "temperature_2m",
    "relative_humidity_2m",
    "wind_speed_10m",
    "wind_direction_10m",
    "precipitation",
    "cloud_cover",
    "shortwave_radiation",
    "surface_pressure",
];

/// Expected `hourly_units` value(s) per variable, given the unit parameters
/// this crate sends. The first entry is the canonical one.
const EXPECTED_UNITS: [&[&str]; 8] = [
    &["°C"],
    &["%"],
    &["m/s"],
    &["°"],
    &["mm"],
    &["%"],
    &["W/m²", "W/m2", "W/m^2"],
    &["hPa"],
];

/// `hourly_units.time` when `timeformat=unixtime`.
const TIME_UNIT: &str = "unixtime";

/// One hour of conditions at a location, from the archive or the historical
/// forecast API. Mirrors the `weather_obs` table.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// Start of the hour (UTC). Accumulated values (precipitation) and
    /// averages (shortwave radiation) cover the hour *before* this instant.
    pub time: Timestamp,
    /// Air temperature at 2 m, °C.
    pub temperature_c: Option<f64>,
    /// Relative humidity at 2 m, %.
    pub humidity_pct: Option<f64>,
    /// Wind speed at 10 m, m/s.
    pub wind_speed_ms: Option<f64>,
    /// Wind direction at 10 m, degrees (meteorological: where it blows from).
    pub wind_dir_deg: Option<f64>,
    /// Precipitation (rain, showers, snow), sum over the preceding hour, mm.
    pub precip_mm: Option<f64>,
    /// Total cloud cover, %.
    pub cloud_cover_pct: Option<f64>,
    /// Global horizontal shortwave radiation, mean over the preceding hour,
    /// W/m².
    pub shortwave_wm2: Option<f64>,
    /// Surface pressure (not reduced to sea level), hPa.
    pub pressure_hpa: Option<f64>,
}

/// One forecast hour. Mirrors the `weather_forecast` table.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ForecastPoint {
    /// When this forecast was obtained (see [`crate::IssuedAt`]). This is
    /// *not* the model run time, which Open-Meteo does not report per row.
    pub issued_at: Timestamp,
    /// The hour the values apply to (UTC), same convention as
    /// [`Observation::time`].
    pub valid_at: Timestamp,
    /// Air temperature at 2 m, °C.
    pub temperature_c: Option<f64>,
    /// Relative humidity at 2 m, %.
    pub humidity_pct: Option<f64>,
    /// Wind speed at 10 m, m/s.
    pub wind_speed_ms: Option<f64>,
    /// Wind direction at 10 m, degrees.
    pub wind_dir_deg: Option<f64>,
    /// Precipitation, sum over the preceding hour, mm.
    pub precip_mm: Option<f64>,
    /// Total cloud cover, %.
    pub cloud_cover_pct: Option<f64>,
    /// Shortwave radiation, mean over the preceding hour, W/m².
    pub shortwave_wm2: Option<f64>,
    /// Surface pressure, hPa.
    pub pressure_hpa: Option<f64>,
}

impl ForecastPoint {
    /// Re-tag an observation-shaped row as a forecast issued at `issued_at`.
    pub fn from_observation(issued_at: Timestamp, o: Observation) -> Self {
        Self {
            issued_at,
            valid_at: o.time,
            temperature_c: o.temperature_c,
            humidity_pct: o.humidity_pct,
            wind_speed_ms: o.wind_speed_ms,
            wind_dir_deg: o.wind_dir_deg,
            precip_mm: o.precip_mm,
            cloud_cover_pct: o.cloud_cover_pct,
            shortwave_wm2: o.shortwave_wm2,
            pressure_hpa: o.pressure_hpa,
        }
    }
}

/// The JSON body of a forecast, archive or historical-forecast response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HourlyResponse {
    /// Latitude of the grid cell used (may differ from the request).
    pub latitude: f64,
    /// Longitude of the grid cell used.
    pub longitude: f64,
    /// Elevation used for downscaling, metres.
    #[serde(default)]
    pub elevation: Option<f64>,
    /// Server time to generate the response.
    #[serde(default)]
    pub generationtime_ms: Option<f64>,
    /// Offset of the requested timezone; 0 for GMT.
    #[serde(default)]
    pub utc_offset_seconds: i64,
    /// Timezone name, e.g. `GMT`.
    #[serde(default)]
    pub timezone: Option<String>,
    /// Unit per hourly column (plus `time`).
    pub hourly_units: BTreeMap<String, String>,
    /// The columns.
    pub hourly: HourlyColumns,
}

/// Column-oriented hourly data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HourlyColumns {
    /// Unix seconds (UTC), one per row.
    pub time: Vec<i64>,
    /// Every other column, by variable name. Values may be `null`.
    #[serde(flatten)]
    pub columns: BTreeMap<String, Vec<Option<f64>>>,
}

impl HourlyResponse {
    /// Validate and convert to observation rows, in time order.
    pub fn observations(&self) -> Result<Vec<Observation>> {
        let times = self.time_axis()?;
        let cols = self.value_columns(times.len())?;
        Ok(times
            .iter()
            .enumerate()
            .map(|(i, &time)| {
                let v = |c: usize| cols[c][i];
                Observation {
                    time,
                    temperature_c: v(0),
                    humidity_pct: v(1),
                    wind_speed_ms: v(2),
                    wind_dir_deg: v(3),
                    precip_mm: v(4),
                    cloud_cover_pct: v(5),
                    shortwave_wm2: v(6),
                    pressure_hpa: v(7),
                }
            })
            .collect())
    }

    /// Validate and convert to forecast rows tagged with `issued_at`.
    /// Pure: the caller decides what `issued_at` is.
    pub fn forecast_points(&self, issued_at: Timestamp) -> Result<Vec<ForecastPoint>> {
        Ok(self
            .observations()?
            .into_iter()
            .map(|o| ForecastPoint::from_observation(issued_at, o))
            .collect())
    }

    fn time_axis(&self) -> Result<Vec<Timestamp>> {
        let unit = self.hourly_units.get("time").map_or("", String::as_str);
        if unit != TIME_UNIT {
            return Err(Error::UnitMismatch {
                column: "time".into(),
                expected: TIME_UNIT,
                actual: unit.into(),
            });
        }
        let mut out: Vec<Timestamp> = Vec::with_capacity(self.hourly.time.len());
        for &secs in &self.hourly.time {
            let t = Timestamp::from_second(secs)
                .map_err(|e| Error::TimeAxis(format!("{secs}: {e}")))?;
            if let Some(&prev) = out.last()
                && t <= prev
            {
                return Err(Error::TimeAxis(format!(
                    "not strictly increasing: {t} after {prev}"
                )));
            }
            out.push(t);
        }
        Ok(out)
    }

    fn value_columns(&self, len: usize) -> Result<Vec<&[Option<f64>]>> {
        HOURLY_VARIABLES
            .iter()
            .zip(EXPECTED_UNITS)
            .map(|(&name, units)| {
                let col = self
                    .hourly
                    .columns
                    .get(name)
                    .ok_or_else(|| Error::MissingColumn(name.into()))?;
                if col.len() != len {
                    return Err(Error::LengthMismatch {
                        column: name.into(),
                        expected: len,
                        actual: col.len(),
                    });
                }
                let unit = self.hourly_units.get(name).map_or("", String::as_str);
                if !units.contains(&unit) {
                    return Err(Error::UnitMismatch {
                        column: name.into(),
                        expected: units[0],
                        actual: unit.into(),
                    });
                }
                Ok(col.as_slice())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use proptest::prelude::*;

    fn units() -> BTreeMap<String, String> {
        std::iter::once(("time".to_owned(), TIME_UNIT.to_owned()))
            .chain(
                HOURLY_VARIABLES
                    .iter()
                    .zip(EXPECTED_UNITS)
                    .map(|(n, u)| ((*n).to_owned(), u[0].to_owned())),
            )
            .collect()
    }

    fn response(time: Vec<i64>, cols: Vec<Vec<Option<f64>>>) -> HourlyResponse {
        HourlyResponse {
            latitude: 50.73,
            longitude: -3.47,
            elevation: Some(45.0),
            generationtime_ms: Some(0.1),
            utc_offset_seconds: 0,
            timezone: Some("GMT".into()),
            hourly_units: units(),
            hourly: HourlyColumns {
                time,
                columns: HOURLY_VARIABLES
                    .iter()
                    .map(|n| (*n).to_owned())
                    .zip(cols)
                    .collect(),
            },
        }
    }

    const T0: i64 = 1_794_441_600; // 2026-11-12T00:00:00Z

    #[test]
    fn rejects_missing_column_wrong_unit_and_unordered_time() {
        let ok = response(vec![T0, T0 + 3600], vec![vec![Some(1.0), None]; 8]);
        assert_eq!(ok.observations().unwrap().len(), 2);

        let mut r = ok.clone();
        r.hourly.columns.remove("surface_pressure");
        assert!(
            matches!(r.observations(), Err(Error::MissingColumn(c)) if c == "surface_pressure")
        );

        let mut r = ok.clone();
        r.hourly_units
            .insert("wind_speed_10m".into(), "km/h".into());
        assert!(matches!(
            r.observations(),
            Err(Error::UnitMismatch { column, expected: "m/s", actual }) if column == "wind_speed_10m" && actual == "km/h"
        ));

        let mut r = ok.clone();
        r.hourly_units.insert("time".into(), "iso8601".into());
        assert!(
            matches!(r.observations(), Err(Error::UnitMismatch { column, .. }) if column == "time")
        );

        let mut r = ok.clone();
        r.hourly.time = vec![T0, T0];
        assert!(matches!(r.observations(), Err(Error::TimeAxis(_))));

        let mut r = ok;
        r.hourly.time = vec![T0, i64::MAX];
        assert!(matches!(r.observations(), Err(Error::TimeAxis(_))));
    }

    #[test]
    fn issued_at_is_exactly_what_the_caller_passes() {
        let r = response(vec![T0], vec![vec![Some(2.5)]; 8]);
        let issued: Timestamp = "2026-11-11T23:17:00Z".parse().unwrap();
        let p = r.forecast_points(issued).unwrap();
        assert_eq!(p[0].issued_at, issued);
        assert_eq!(p[0].valid_at.as_second(), T0);
    }

    fn column(n: usize) -> impl Strategy<Value = Vec<Option<f64>>> {
        proptest::collection::vec(proptest::option::of(-1000.0f64..1000.0), n)
    }

    /// `n` rows, 8 columns of length `n`, strictly increasing times.
    fn table() -> impl Strategy<Value = (Vec<i64>, Vec<Vec<Option<f64>>>)> {
        (0usize..60).prop_flat_map(|n| {
            (
                proptest::collection::vec(1i64..7200, n).prop_map(|steps| {
                    steps
                        .iter()
                        .scan(T0, |t, s| {
                            *t += s;
                            Some(*t)
                        })
                        .collect()
                }),
                proptest::collection::vec(column(n), 8),
            )
        })
    }

    proptest! {
        /// Conversion keeps every row, in order, with each cell (null or
        /// not) landing in the matching field.
        #[test]
        fn columns_to_rows_round_trip((time, cols) in table()) {
            let rows = response(time.clone(), cols.clone()).observations().unwrap();
            prop_assert_eq!(rows.len(), time.len());
            for (i, row) in rows.iter().enumerate() {
                prop_assert_eq!(row.time.as_second(), time[i]);
                let got = [
                    row.temperature_c, row.humidity_pct, row.wind_speed_ms, row.wind_dir_deg,
                    row.precip_mm, row.cloud_cover_pct, row.shortwave_wm2, row.pressure_hpa,
                ];
                for (c, v) in got.iter().enumerate() {
                    prop_assert_eq!(*v, cols[c][i]);
                }
            }
            prop_assert!(rows.windows(2).all(|w| w[0].time < w[1].time));
        }

        /// Any column whose length differs from `time` is rejected, naming it.
        #[test]
        fn length_mismatch_is_rejected(
            (time, mut cols) in table(),
            which in 0usize..8,
            extra in 1usize..3,
            shorten in any::<bool>(),
        ) {
            let col = &mut cols[which];
            if shorten && !col.is_empty() {
                col.truncate(col.len() - 1);
            } else {
                col.extend(std::iter::repeat_n(None, extra));
            }
            let got = col.len();
            match response(time.clone(), cols).observations() {
                Err(Error::LengthMismatch { column, expected, actual }) => {
                    prop_assert_eq!(column, HOURLY_VARIABLES[which]);
                    prop_assert_eq!(expected, time.len());
                    prop_assert_eq!(actual, got);
                }
                other => prop_assert!(false, "expected LengthMismatch, got {:?}", other),
            }
        }
    }
}
