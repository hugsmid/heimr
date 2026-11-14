//! Checks over emitted records: plain functions over [`Envelope`]s, so any
//! driver (or an author's own test) can use them.
//!
//! Each returns a list of problems, empty when the records pass.

use std::collections::BTreeMap;

use heimr_core::id::validate_name;
use heimr_core::{
    Basis, Currency, DeviceId, Envelope, Event, Metric, Quality, Record, SeriesRole,
    SeriesTemplate, Unit,
};
use jiff::{SignedDuration, Timestamp};

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
///
/// These are project-wide conventions, applied to the kind's series
/// templates (and so to every reading and forecast, which must match its
/// template): a template for `temperature` must say °C, whichever adapter
/// declares it. Units of other metrics come from the templates alone.
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
        // Money is never a plain number: a price per kWh, and a daily
        // standing charge, in the site currency.
        "unit_rate" => &[CurrencyPerKilowattHour],
        "standing_charge" => &[Currency],
        _ => return None,
    };
    Some(units)
}

fn label(env: &Envelope) -> String {
    match &env.record {
        Record::Reading(r) => format!("reading {}/{} at {}", r.device, r.metric, r.time),
        Record::Forecast(f) => format!(
            "forecast {}/{} for {}..{} issued at {}",
            f.device, f.metric, f.valid_from, f.valid_to, f.issued_at
        ),
        Record::Event(e) => format!("event {} at {}", e.kind, e.time),
        other => format!(
            "state {} at {}",
            env.state_key().map(|k| k.id).unwrap_or_default(),
            other.time()
        ),
    }
}

/// Every record names `instance` (as source, and as actor of events), uses
/// valid identifiers, is not of a core-reserved event kind, and is
/// timestamped within `earliest..=latest` (a forecast by its issue time). A
/// forecast's period is ordered (`valid_from <= valid_to`) and ends at most
/// `horizon` after its issue time. State values must be finite.
///
/// NaN and ±∞ in readings and forecasts are not problems: they are passed
/// through for the store to record as bad quality. [`non_finite`] lists
/// them.
pub fn well_formed(
    instance: &str,
    records: &[Envelope],
    earliest: Timestamp,
    latest: Timestamp,
    horizon: SignedDuration,
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
            }
            Record::Forecast(f) => {
                if DeviceId::new(f.device.as_str()).is_err() {
                    p.push(format!("{what}: invalid device id"));
                }
                if Metric::new(f.metric.as_str()).is_err() {
                    p.push(format!("{what}: invalid metric name"));
                }
                if f.valid_from > f.valid_to {
                    p.push(format!("{what}: valid_from is after valid_to"));
                }
                if f.valid_to.duration_since(f.issued_at) > horizon {
                    p.push(format!(
                        "{what}: ends more than the {horizon:#} forecast horizon after it was issued"
                    ));
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

/// Every reading and forecast of a versioned series (per its template)
/// carries `changed_at`, and none of a series that is not versioned does.
/// Records without a template are left to [`units`].
pub fn versioning(records: &[Envelope], series: &[SeriesTemplate]) -> Vec<String> {
    let mut p = Problems::default();
    for env in records {
        let (metric, forecast, changed_at) = match &env.record {
            Record::Reading(r) => (&r.metric, false, r.changed_at),
            Record::Forecast(f) => (&f.metric, true, f.changed_at),
            _ => continue,
        };
        let Some(t) = series
            .iter()
            .find(|t| t.for_forecasts() == forecast && t.metric == *metric)
        else {
            continue;
        };
        match (t.versioned, changed_at) {
            (true, None) => p.push(format!(
                "{}: the series is versioned, but it carries no changed_at",
                label(env)
            )),
            (false, Some(c)) => p.push(format!(
                "{}: changed_at {c} on a series that is not versioned",
                label(env)
            )),
            _ => {}
        }
    }
    p.finish()
}

/// Every reading of an interval series (and every forecast, by its
/// `valid_from`) is stamped at an interval start: aligned to the
/// template's length on UTC nanoseconds.
pub fn intervals(records: &[Envelope], series: &[SeriesTemplate]) -> Vec<String> {
    let mut p = Problems::default();
    for env in records {
        let (metric, forecast, t) = match &env.record {
            Record::Reading(r) => (&r.metric, false, r.time),
            Record::Forecast(f) => (&f.metric, true, f.valid_from),
            _ => continue,
        };
        let Some(s) = series
            .iter()
            .find(|s| s.for_forecasts() == forecast && s.metric == *metric)
        else {
            continue;
        };
        if let Some(d) = s.interval_length()
            && !s.is_aligned(t)
        {
            p.push(format!(
                "{}: {t} does not start an interval of {d:#}",
                label(env)
            ));
        }
    }
    p.finish()
}

/// Readings and forecasts whose value is NaN or ±∞. Not a problem: the
/// emitter passes them through and the store will record them as bad quality.
/// The list (first few, then a count) is for a note.
pub fn non_finite(records: &[Envelope]) -> Vec<String> {
    let mut p = Problems::default();
    for env in records {
        let value = match &env.record {
            Record::Reading(r) => r.value,
            Record::Forecast(f) => f.value,
            _ => continue,
        };
        if !value.is_finite() {
            p.push(format!("{}: value {value}", label(env)));
        }
    }
    p.finish()
}

/// Every series template of a well-known metric uses its documented unit
/// ([`documented_units`]), and money templates appear only if the kind
/// declares a `source_currency`. Every reading and forecast has a template
/// for its metric (as a reading or as a forecast) and uses its unit; a
/// published forecast's template is scheduled and a modelled one's a
/// forecast template; every
/// series (device, metric; readings and forecasts separately) keeps one
/// unit; and money units appear only with a declared currency.
pub fn units(
    records: &[Envelope],
    series: &[SeriesTemplate],
    source_currency: Option<Currency>,
) -> Vec<String> {
    let mut p = Problems::default();
    for t in series {
        let label = t.label();
        if let Some(allowed) = documented_units(&t.metric)
            && !allowed.contains(&t.unit)
        {
            p.push(format!(
                "template for {label}: unit {} but {} is documented in {}",
                t.unit,
                t.metric,
                allowed
                    .iter()
                    .map(|u| u.symbol())
                    .collect::<Vec<_>>()
                    .join(" or ")
            ));
        }
        if t.unit.is_money() && source_currency.is_none() {
            p.push(format!(
                "template for {label}: money unit {} from a kind that declares no source currency",
                t.unit
            ));
        }
    }
    let mut seen: BTreeMap<(&'static str, String, String), Unit> = BTreeMap::new();
    for env in records {
        let (class, device, metric, unit, time) = match &env.record {
            Record::Reading(r) => ("reading", &r.device, &r.metric, r.unit, r.time),
            Record::Forecast(f) => ("forecast", &f.device, &f.metric, f.unit, f.issued_at),
            _ => continue,
        };
        let forecast = class == "forecast";
        match series
            .iter()
            .find(|t| t.for_forecasts() == forecast && t.metric == *metric)
        {
            None => p.push(format!(
                "{class} {device}/{metric}: no series template for {class}s of {metric}"
            )),
            Some(t) if t.unit != unit => p.push(format!(
                "{class} {device}/{metric}: unit {unit} differs from its template's {}",
                t.unit
            )),
            Some(t) => {
                if let Record::Forecast(f) = &env.record {
                    let needs = match f.basis {
                        Basis::Published => SeriesRole::Scheduled,
                        _ => SeriesRole::Forecast,
                    };
                    if t.role != needs {
                        p.push(format!(
                            "forecast {device}/{metric}: a {} forecast needs a {} template, not a {} one",
                            f.basis.as_str(),
                            needs.as_str(),
                            t.role.as_str()
                        ));
                    }
                }
            }
        }
        if unit.is_money() && source_currency.is_none() {
            p.push(format!(
                "{device}/{metric}: money unit {unit} from a kind that declares no source currency"
            ));
        }
        let key = (class, device.to_string(), metric.to_string());
        match seen.get(&key) {
            Some(u) if *u != unit => p.push(format!(
                "{class} {device}/{metric}: unit changed from {u} to {unit} at {time}"
            )),
            Some(_) => {}
            None => {
                seen.insert(key, unit);
            }
        }
    }
    p.finish()
}

/// One fact as the store sees it: what must be identical on a re-send.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Fact {
    bits: u64,
    quality: Quality,
    changed_at: Option<u64>,
    /// A forecast's own time (its issue time).
    issued_at: Option<Timestamp>,
    /// A forecast's period end.
    valid_to: Option<Timestamp>,
    run: usize,
}

/// Records repeat and revise correctly, given each run of the instance in
/// order (a new run after every restart: [`Driver::runs`](crate::Driver::runs),
/// [`Harness::runs`](crate::Harness::runs)). A series is keyed by
/// (device, metric), readings and forecasts apart; a fact by its series and
/// `t` (a reading's time, a forecast's `valid_from`).
///
/// - **Within one run**, each new reading of a series that is not
///   versioned is newer than the last: no reordering.
/// - **Any repeat** of a series and `t` (across a restart, or within a
///   run, as after an error) is identical to the last fact there: the same
///   value bits, quality and `changed_at`, and for a forecast the same
///   issue time and period. A different value is a correction: on a versioned series
///   it needs a higher `changed_at`; on any other it is a failure.
/// - State timestamps never go back per zone or device.
///
/// Versioning comes from `series`: forecast records count as versioned
/// without a template, readings as not.
pub fn monotonic(runs: &[Vec<Envelope>], series: &[SeriesTemplate]) -> Vec<String> {
    let mut p = Problems::default();
    let mut facts: BTreeMap<(&'static str, String, String, Timestamp), Fact> = BTreeMap::new();
    let mut states: BTreeMap<String, Timestamp> = BTreeMap::new();
    for (run, records) in runs.iter().enumerate() {
        let mut latest: BTreeMap<(String, String), Timestamp> = BTreeMap::new();
        for env in records {
            let (class, device, metric, t, fact) = match &env.record {
                Record::Reading(r) => (
                    "reading",
                    &r.device,
                    &r.metric,
                    r.time,
                    Fact {
                        bits: r.value.to_bits(),
                        quality: r.quality,
                        changed_at: r.changed_at,
                        issued_at: None,
                        valid_to: None,
                        run,
                    },
                ),
                Record::Forecast(f) => (
                    "forecast",
                    &f.device,
                    &f.metric,
                    f.valid_from,
                    Fact {
                        bits: f.value.to_bits(),
                        quality: f.quality,
                        changed_at: f.changed_at,
                        issued_at: Some(f.issued_at),
                        valid_to: Some(f.valid_to),
                        run,
                    },
                ),
                Record::Event(_) => continue,
                other => {
                    let Some(key) = env.state_key() else { continue };
                    let t = other.time();
                    if let Some(prev) = states.get(&key.to_string())
                        && t < *prev
                    {
                        p.push(format!("state {key}: time {t} before {prev}"));
                    }
                    states.insert(key.to_string(), t);
                    continue;
                }
            };
            let forecast = class == "forecast";
            let versioned = series
                .iter()
                .find(|s| s.for_forecasts() == forecast && s.metric == *metric)
                .map_or(forecast, |s| s.versioned);
            let at = format!("{class} {device}/{metric} at {t}");
            let key = (class, device.to_string(), metric.to_string(), t);
            let seen = facts.get(&key).copied();
            // A new fact of a plain series must be newer than the last in
            // this run; a repeat is judged by the repeat rule below (a
            // re-send after an error is allowed if identical).
            if !versioned && seen.is_none() {
                let series_key = (device.to_string(), metric.to_string());
                if let Some(prev) = latest.get(&series_key)
                    && t <= *prev
                {
                    let how = if t == *prev {
                        "duplicate"
                    } else {
                        "out of order"
                    };
                    p.push(format!(
                        "{device}/{metric}: {how} timestamp {t} after {prev} in one run"
                    ));
                }
                latest.insert(series_key, t);
            }
            if let Some(prev) = seen {
                if let Some(problem) = repeat_problem(prev, fact, versioned) {
                    p.push(format!("{at}: {problem}"));
                }
                // A correction that does not outrank stays out of the way.
                let outranked =
                    versioned && prev.bits != fact.bits && fact.changed_at <= prev.changed_at;
                if !outranked {
                    facts.insert(key, fact);
                }
            } else {
                facts.insert(key, fact);
            }
        }
    }
    p.finish()
}

/// What is wrong with `new` as a repeat or revision of `old`, if anything.
fn repeat_problem(old: Fact, new: Fact, versioned: bool) -> Option<String> {
    let same_value = old.bits == new.bits;
    let mark = |c: Option<u64>| c.map_or_else(|| "none".to_owned(), |c| c.to_string());
    if versioned {
        if same_value {
            if new.changed_at != old.changed_at {
                return Some(format!(
                    "the same value re-sent with a different changed_at ({}, was {})",
                    mark(new.changed_at),
                    mark(old.changed_at)
                ));
            }
            if new.quality != old.quality {
                return Some(format!(
                    "quality changed on re-send ({:?}, was {:?}) without a new changed_at",
                    new.quality, old.quality
                ));
            }
            if new.valid_to != old.valid_to {
                return Some(format!(
                    "re-sent with another period (valid to {}, was {})",
                    new.valid_to.map(|t| t.to_string()).unwrap_or_default(),
                    old.valid_to.map(|t| t.to_string()).unwrap_or_default()
                ));
            }
            if new.issued_at != old.issued_at {
                return Some(format!(
                    "re-sent with its time re-stamped (issued at {}, was {})",
                    new.issued_at.map(|t| t.to_string()).unwrap_or_default(),
                    old.issued_at.map(|t| t.to_string()).unwrap_or_default()
                ));
            }
            None
        } else if new.changed_at <= old.changed_at {
            Some(format!(
                "a new value with changed_at {}, not above the {} it replaces",
                mark(new.changed_at),
                mark(old.changed_at)
            ))
        } else {
            None
        }
    } else if !same_value {
        Some(
            "the value changed on re-send, on a series that is not versioned (a re-stamped t, \
             or a correction that needs a versioned series)"
                .to_owned(),
        )
    } else if new.quality != old.quality {
        Some(format!(
            "quality changed on re-send ({:?}, was {:?})",
            new.quality, old.quality
        ))
    } else if new.changed_at != old.changed_at {
        Some("changed_at differs on re-send".to_owned())
    } else {
        None
    }
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
        for m in ["unit_rate", "standing_charge"] {
            let units = documented_units(&Metric::new(m).unwrap()).unwrap();
            assert!(units.iter().all(|u| u.is_money()), "{m}");
        }
    }
}
