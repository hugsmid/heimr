//! Stable, line-per-record text for snapshot tests (`insta` or plain
//! string comparison).
//!
//! ```
//! use heimr_testkit::render::render;
//! assert_eq!(render(&[]), "");
//! ```
//!
//! Each line is `time kind source/subject details`, for example
//!
//! ```text
//! 2030-01-01T00:00:00Z reading under-test sensor-1 temperature 20.5 °C
//! 2030-01-01T00:00:00Z forecast under-test tariff unit_rate 0.184 ¤/kWh for 2030-01-02T00:00:00Z..2030-01-02T00:30:00Z published
//! 2030-01-01T00:00:00Z event under-test adapter_health {"previous":"unknown","reason":null,"status":"ok"}
//! ```
//!
//! A forecast's leading time is its issue time.
//!
//! Values use Rust's shortest round-trip float formatting; event payloads
//! are JSON with sorted keys. Quality is shown only when not `good`.

use std::fmt::Write;

use heimr_core::{Envelope, Quality, Record};
use jiff::Timestamp;

/// Options for [`Render::render`].
#[derive(Debug, Clone, Default)]
pub struct Render {
    relative_to: Option<Timestamp>,
    decimals: Option<usize>,
    skip_health: bool,
}

impl Render {
    /// Default options: absolute times, full precision, health included.
    pub fn new() -> Self {
        Self::default()
    }

    /// Show times as offsets from `t0` (`+1m30s`).
    #[must_use]
    pub fn relative_to(mut self, t0: Timestamp) -> Self {
        self.relative_to = Some(t0);
        self
    }

    /// Round values to `n` decimals.
    #[must_use]
    pub fn decimals(mut self, n: usize) -> Self {
        self.decimals = Some(n);
        self
    }

    /// Leave out `adapter_health` events.
    #[must_use]
    pub fn skip_health(mut self) -> Self {
        self.skip_health = true;
        self
    }

    fn time(&self, t: Timestamp) -> String {
        match self.relative_to {
            Some(t0) => format!("+{:#}", t.duration_since(t0)),
            None => t.to_string(),
        }
    }

    fn value(&self, v: f64) -> String {
        match self.decimals {
            Some(n) => format!("{v:.n$}"),
            None => format!("{v}"),
        }
    }

    fn opt(&self, name: &str, v: Option<f64>) -> String {
        v.map(|v| format!(" {name}={}", self.value(v)))
            .unwrap_or_default()
    }

    /// One line per record, in the given order.
    pub fn render(&self, records: &[Envelope]) -> String {
        let mut out = String::new();
        for env in records {
            let t = self.time(env.record.time());
            let src = &env.source;
            let line = match &env.record {
                Record::Reading(r) => {
                    let q = quality(r.quality);
                    format!(
                        "{t} reading {src} {} {} {} {}{q}",
                        r.device,
                        r.metric,
                        self.value(r.value),
                        r.unit
                    )
                }
                Record::Forecast(f) => {
                    let basis = serde_json::to_string(&f.basis).unwrap_or_default();
                    format!(
                        "{t} forecast {src} {} {} {} {} for {}..{} {}{}",
                        f.device,
                        f.metric,
                        self.value(f.value),
                        f.unit,
                        self.time(f.valid_from),
                        self.time(f.valid_to),
                        basis.trim_matches('"'),
                        quality(f.quality)
                    )
                }
                Record::Event(e) => {
                    if self.skip_health && e.kind == heimr_core::Event::ADAPTER_HEALTH {
                        continue;
                    }
                    format!("{t} event {src} {} {}", e.kind, e.payload)
                }
                Record::Climate(s) => format!(
                    "{t} climate {src} {} {:?}{}{}{}",
                    s.zone,
                    s.mode,
                    self.opt("target", s.target_c),
                    self.opt("measured", s.measured_c),
                    self.opt("heating", s.heating_pct)
                ),
                Record::Fan(s) => format!("{t} fan {src} {} {:?}", s.device, s.mode),
                Record::Switch(s) => format!("{t} switch {src} {} on={}", s.device, s.on),
                other => format!("{t} record {src} {other:?}"),
            };
            let _ = writeln!(out, "{line}");
        }
        out
    }
}

/// ` (estimated)` and the like; nothing for good quality.
fn quality(q: Quality) -> String {
    if q == Quality::Good {
        String::new()
    } else {
        format!(
            " ({})",
            serde_json::to_string(&q)
                .unwrap_or_default()
                .trim_matches('"')
        )
    }
}

/// [`Render::new`]`.render(records)`.
pub fn render(records: &[Envelope]) -> String {
    Render::new().render(records)
}
