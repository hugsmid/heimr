//! Checks over emitted records: plain functions over [`Envelope`]s, so any
//! driver (or an author's own test) can use them.
//!
//! Each returns a list of problems, empty when the records pass.

use std::collections::BTreeMap;

use heimr_core::id::validate_name;
use heimr_core::{DeviceId, Envelope, Event, Metric, Record, Unit};
use jiff::Timestamp;

/// At most this many problems are listed per check; the rest are counted.
const MAX_LISTED: usize = 8;

/// Collects problems, listing the first few.
#[derive(Debug, Default)]
pub(crate) struct Problems {
    listed: Vec<String>,
    more: usize,
}

impl Problems {
    pub(crate) fn push(&mut self, p: String) {
        if self.listed.len() < MAX_LISTED {
            self.listed.push(p);
        } else {
            self.more += 1;
        }
    }

    pub(crate) fn finish(mut self) -> Vec<String> {
        if self.more > 0 {
            self.listed.push(format!("… and {} more", self.more));
        }
        self.listed
    }
}

/// The unit(s) heimr documents for a well-known metric, or `None` for a
/// metric of the adapter's own.
pub fn documented_units(metric: &Metric) -> Option<&'static [Unit]> {
    use Unit::*;
    let units: &'static [Unit] = match metric.as_str() {
        "temperature" => &[Celsius],
        "humidity" | "filter_life" | "heating_demand" | "battery" => &[Percent],
        "pm25" | "pm10" => &[MicrogramsPerCubicMetre],
        "co2" => &[PartsPerMillion],
        "voc" => &[PartsPerBillion, Dimensionless],
        "fan_level" => &[Dimensionless],
        "air_flow" => &[CubicMetrePerHour],
        "power" => &[Watt],
        "energy" => &[WattHour, KilowattHour],
        _ => return None,
    };
    Some(units)
}

fn label(env: &Envelope) -> String {
    match &env.record {
        Record::Reading(r) => format!("reading {}/{} at {}", r.device, r.metric, r.time),
        Record::Event(e) => format!("event {} at {}", e.kind, e.time),
        other => format!(
            "state {} at {}",
            env.state_key().map(|k| k.id).unwrap_or_default(),
            other.time()
        ),
    }
}

/// Every record names `instance` (as source, and as actor of events), uses
/// valid identifiers and a finite value, is not of a core-reserved event
/// kind, and is timestamped within `earliest..=latest`.
pub fn well_formed(
    instance: &str,
    records: &[Envelope],
    earliest: Timestamp,
    latest: Timestamp,
) -> Vec<String> {
    let mut p = Problems::default();
    if validate_name(instance).is_err() {
        p.push(format!(
            "instance name {instance:?} breaks the naming rules"
        ));
    }
    for env in records {
        let what = label(env);
        if env.source != instance {
            p.push(format!(
                "{what}: source {:?} is not the instance",
                env.source
            ));
        }
        let t = env.record.time();
        if t < earliest || t > latest {
            p.push(format!(
                "{what}: timestamp outside {earliest} ..= {latest} (allowed back-fill and clock skew)"
            ));
        }
        match &env.record {
            Record::Reading(r) => {
                if r.source != instance {
                    p.push(format!("{what}: reading source {:?}", r.source));
                }
                if DeviceId::new(r.device.as_str()).is_err() {
                    p.push(format!("{what}: invalid device id"));
                }
                if Metric::new(r.metric.as_str()).is_err() {
                    p.push(format!("{what}: invalid metric name"));
                }
                if !r.value.is_finite() {
                    p.push(format!("{what}: value {} is not finite", r.value));
                }
            }
            Record::Event(e) => {
                if e.actor != instance {
                    p.push(format!("{what}: actor {:?} is not the instance", e.actor));
                }
                if DeviceId::new(e.kind.as_str()).is_err() {
                    p.push(format!("{what}: invalid event kind"));
                }
                if Event::is_reserved_kind(&e.kind) && e.kind != Event::ADAPTER_HEALTH {
                    p.push(format!("{what}: kind is reserved for the core"));
                }
            }
            Record::Climate(s) => {
                for (name, v) in [
                    ("target_c", s.target_c),
                    ("measured_c", s.measured_c),
                    ("heating_pct", s.heating_pct),
                ] {
                    if v.is_some_and(|v| !v.is_finite()) {
                        p.push(format!("{what}: {name} is not finite"));
                    }
                }
            }
            _ => {}
        }
    }
    p.finish()
}

/// Well-known metrics use their documented unit, and every series
/// (device, metric) keeps one unit.
pub fn units(records: &[Envelope]) -> Vec<String> {
    let mut p = Problems::default();
    let mut seen: BTreeMap<(String, String), Unit> = BTreeMap::new();
    for env in records {
        let Record::Reading(r) = &env.record else {
            continue;
        };
        if let Some(allowed) = documented_units(&r.metric)
            && !allowed.contains(&r.unit)
        {
            p.push(format!(
                "{}/{}: unit {} but {} is documented in {}",
                r.device,
                r.metric,
                r.unit,
                r.metric,
                allowed
                    .iter()
                    .map(|u| u.symbol())
                    .collect::<Vec<_>>()
                    .join(" or ")
            ));
        }
        let key = (r.device.to_string(), r.metric.to_string());
        match seen.get(&key) {
            Some(u) if *u != r.unit => p.push(format!(
                "{}/{}: unit changed from {} to {} at {}",
                r.device, r.metric, u, r.unit, r.time
            )),
            Some(_) => {}
            None => {
                seen.insert(key, r.unit);
            }
        }
    }
    p.finish()
}

/// Reading timestamps strictly increase per series (device, metric): no
/// duplicates (also across restarts and reconnects) and no reordering.
/// State timestamps never go backwards per zone or device.
pub fn monotonic(records: &[Envelope]) -> Vec<String> {
    let mut p = Problems::default();
    let mut readings: BTreeMap<(String, String), Timestamp> = BTreeMap::new();
    let mut states: BTreeMap<String, Timestamp> = BTreeMap::new();
    for env in records {
        match &env.record {
            Record::Reading(r) => {
                let key = (r.device.to_string(), r.metric.to_string());
                if let Some(prev) = readings.get(&key)
                    && r.time <= *prev
                {
                    let how = if r.time == *prev {
                        "duplicate"
                    } else {
                        "out of order"
                    };
                    p.push(format!(
                        "{}/{}: {how} timestamp {} after {}",
                        r.device, r.metric, r.time, prev
                    ));
                }
                readings.insert(key, r.time);
            }
            Record::Event(_) => {}
            other => {
                let Some(key) = env.state_key() else { continue };
                let t = other.time();
                if let Some(prev) = states.get(&key.to_string())
                    && t < *prev
                {
                    p.push(format!("state {key}: time {t} before {prev}"));
                }
                states.insert(key.to_string(), t);
            }
        }
    }
    p.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_units_cover_the_core_constants() {
        for m in [
            Metric::TEMPERATURE,
            Metric::HUMIDITY,
            Metric::PM25,
            Metric::PM10,
            Metric::CO2,
            Metric::VOC,
            Metric::FILTER_LIFE,
            Metric::FAN_LEVEL,
            Metric::AIR_FLOW,
            Metric::HEATING_DEMAND,
            Metric::POWER,
            Metric::ENERGY,
            Metric::BATTERY,
        ] {
            assert!(documented_units(&m).is_some(), "{m}");
        }
        assert!(documented_units(&Metric::new("my_metric").unwrap()).is_none());
    }
}
