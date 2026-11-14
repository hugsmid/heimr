//! `demo`: a synthetic adapter for trying heimr without hardware.
//!
//! Each configured zone is a simulated room: every `interval` (on the
//! context clock) it emits a temperature reading, a humidity reading and the
//! zone's [`ClimateState`]. Temperature relaxes towards the zone's target
//! (or a cool ambient when off) with a 30-minute time constant; humidity
//! follows a gentle daily cycle. Small deterministic noise comes from `seed`.
//!
//! It accepts setpoint, resume-schedule and off commands, so shadow and live
//! behaviour can be seen end to end: in shadow mode (the default) the target
//! never changes, because the command never arrives. Overrides end the way
//! a real device's do: a timed [`Hold`] expires after its duration, and
//! [`Hold::NextScheduleChange`] at the next boundary of the simulated
//! schedule (every `schedule_block`). It reports health (`ok`; `degraded`
//! after a command for a zone it does not simulate) and counts `ticks` as a
//! custom metric.
//!
//! Series: `temperature` (°C) and `humidity` (%) per zone, gauges stamped
//! by the host clock, expected every `interval`.
//!
//! <!-- ANCHOR: toml -->
//! ```toml
//! [adapters.demo]
//! type = "demo"
//! mode = "shadow"        # or "live"
//! interval = "1m"
//! zones = ["zone-a", "zone-b"]
//! initial_target = 19.0
//! schedule_block = "3h"
//! seed = 1
//! ```
//! <!-- ANCHOR_END: toml -->

use std::collections::BTreeSet;

use heimr_core::{
    Adapter, AdapterContext, AdapterDescriptor, AdapterError, Capabilities, ClimateCommand,
    ClimateMode, ClimateState, Command, CommandEnvelope, CommandKind, DeviceId, EmitError, Hold,
    Metric, SeriesTemplate, Setpoint, Unit, ZoneId,
};
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;

// ANCHOR: config
/// The demo's instance settings.
#[derive(Debug, Clone, Deserialize)]
pub struct DemoConfig {
    /// Time between samples (1 s ..= 1 day).
    #[serde(default = "default_interval")]
    pub interval: SignedDuration,
    /// Simulated zones (1–32, unique).
    #[serde(default = "default_zones")]
    pub zones: Vec<ZoneId>,
    /// The simulated schedule's target, which zones start with and return
    /// to when an override ends.
    #[serde(default = "default_target")]
    pub initial_target: Setpoint,
    /// Length of a simulated schedule block (1 min ..= 1 day): overrides
    /// held "until the next schedule change" end at the next multiple of it.
    #[serde(default = "default_block")]
    pub schedule_block: SignedDuration,
    /// Noise seed.
    #[serde(default)]
    pub seed: u64,
}
// ANCHOR_END: config

fn default_interval() -> SignedDuration {
    SignedDuration::from_mins(1)
}

fn default_zones() -> Vec<ZoneId> {
    ZoneId::new("zone-a").into_iter().collect()
}

fn default_target() -> Setpoint {
    Setpoint::new(19.0).expect("19 °C lies inside the default band")
}

fn default_block() -> SignedDuration {
    SignedDuration::from_hours(3)
}

/// Ambient temperature a zone drifts to with heating off, °C.
const AMBIENT_C: f64 = 12.0;
/// How far below target a zone starts, °C.
const START_BELOW_C: f64 = 2.0;
/// Thermal time constant, minutes.
const TAU_MIN: f64 = 30.0;

/// The demo adapter. See the [module docs](self).
#[derive(Debug)]
pub struct Demo {
    zones: Vec<Zone>,
    rng: XorShift,
    interval: SignedDuration,
}

#[derive(Debug)]
struct Zone {
    id: ZoneId,
    device: DeviceId,
    mode: ClimateMode,
    target: Setpoint,
    /// When the current override ends (the device's own timer).
    override_until: Option<Timestamp>,
    temperature: f64,
    last: Option<Timestamp>,
}

// ANCHOR: descriptor
impl Adapter for Demo {
    type Config = DemoConfig;

    fn descriptor() -> AdapterDescriptor {
        AdapterDescriptor::new(
            "demo",
            "synthetic temperature/humidity sensor with simulated heating zones",
            Capabilities::read_write([
                CommandKind::ClimateTarget,
                CommandKind::ClimateSchedule,
                CommandKind::ClimateOff,
            ]),
        )
        // Sampled every `interval` (the default's here), stamped by the
        // host clock.
        .with_series(SeriesTemplate::gauge(
            Metric::TEMPERATURE,
            Unit::Celsius,
            default_interval(),
        ))
        .with_series(SeriesTemplate::gauge(
            Metric::HUMIDITY,
            Unit::Percent,
            default_interval(),
        ))
    }
    // ANCHOR_END: descriptor

    // ANCHOR: new
    fn new(config: &DemoConfig) -> Result<Self, AdapterError> {
        if config.interval < SignedDuration::from_secs(1)
            || config.interval > SignedDuration::from_hours(24)
        {
            return Err(AdapterError::config_key(
                "interval",
                "must be between 1s and 24h",
            ));
        }
        if config.zones.is_empty() || config.zones.len() > 32 {
            return Err(AdapterError::config_key("zones", "must list 1 to 32 zones"));
        }
        if config.zones.iter().collect::<BTreeSet<_>>().len() != config.zones.len() {
            return Err(AdapterError::config_key("zones", "must be unique"));
        }
        if config.schedule_block < SignedDuration::from_mins(1)
            || config.schedule_block > SignedDuration::from_hours(24)
        {
            return Err(AdapterError::config_key(
                "schedule_block",
                "must be between 1m and 24h",
            ));
        }
        let zones = config
            .zones
            .iter()
            .map(|id| Zone {
                id: id.clone(),
                device: DeviceId::from(id.clone()),
                mode: ClimateMode::Schedule,
                target: config.initial_target,
                override_until: None,
                temperature: config.initial_target.get() - START_BELOW_C,
                last: None,
            })
            .collect();
        Ok(Self {
            zones,
            rng: XorShift::new(config.seed),
            interval: config.interval,
        })
    }
    // ANCHOR_END: new

    /// The kind's templates, expected every configured `interval`.
    fn series(&self) -> Vec<SeriesTemplate> {
        Self::descriptor()
            .series
            .into_iter()
            .map(|t| t.with_expected_interval(self.interval))
            .collect()
    }

    // ANCHOR: run
    async fn run(mut self, mut ctx: AdapterContext<DemoConfig>) -> Result<(), AdapterError> {
        let interval = ctx.config().interval;
        let mut commands = ctx.take_commands();
        let mut next = ctx.now();
        ctx.health().ok().await?;
        loop {
            tokio::select! {
                biased;
                () = ctx.shutdown().wait() => return Ok(()),
                Some(cmd) = commands.recv() => self.apply(&ctx, cmd).await?,
                () = ctx.sleep_until(next) => {
                    let now = ctx.now();
                    self.tick(&ctx, now).await?;
                    ctx.metrics().counter("ticks", 1);
                    next = next.saturating_add(interval).unwrap_or(Timestamp::MAX);
                    if next <= now {
                        // Fell behind (suspend, clock step): skip missed ticks.
                        next = now.saturating_add(interval).unwrap_or(Timestamp::MAX);
                    }
                }
            }
        }
    }
    // ANCHOR_END: run
}

impl Demo {
    async fn tick(
        &mut self,
        ctx: &AdapterContext<DemoConfig>,
        now: Timestamp,
    ) -> Result<(), EmitError> {
        let day_fraction = (now.as_second().rem_euclid(86_400) as f64) / 86_400.0;
        let schedule_target = ctx.config().initial_target;
        for z in &mut self.zones {
            // The simulated device ends its own override, as a real one does.
            if z.override_until.is_some_and(|t| t <= now) {
                z.mode = ClimateMode::Schedule;
                z.target = schedule_target;
                z.override_until = None;
            }
            let dt_min = z
                .last
                .map_or(0.0, |t| now.duration_since(t).as_secs_f64().max(0.0) / 60.0);
            z.last = Some(now);
            let goal = if z.mode == ClimateMode::Off {
                AMBIENT_C
            } else {
                z.target.get()
            };
            z.temperature += (goal - z.temperature) * (1.0 - (-dt_min / TAU_MIN).exp());
            let temp = round2(z.temperature + self.rng.noise(0.05));
            let humidity = round2(
                (50.0 + 8.0 * (std::f64::consts::TAU * day_fraction).sin() + self.rng.noise(0.5))
                    .clamp(20.0, 80.0),
            );
            ctx.emit(ctx.reading(z.device.clone(), Metric::TEMPERATURE, temp, Unit::Celsius))
                .await?;
            ctx.emit(ctx.reading(z.device.clone(), Metric::HUMIDITY, humidity, Unit::Percent))
                .await?;
            ctx.emit(state(z, now, Some(temp))).await?;
        }
        Ok(())
    }

    /// When an override with `hold` given at `now` ends.
    fn override_end(
        &self,
        ctx: &AdapterContext<DemoConfig>,
        now: Timestamp,
        hold: Hold,
    ) -> Timestamp {
        let block = ctx.config().schedule_block;
        let next_block = || {
            let b = block.as_secs().max(60);
            let s = now.as_second();
            Timestamp::from_second(s - s.rem_euclid(b) + b).unwrap_or(Timestamp::MAX)
        };
        match hold {
            Hold::For(d) => now.saturating_add(d.get()).unwrap_or(Timestamp::MAX),
            // Unknown future hold kinds end at the next block: the shorter,
            // safer reading.
            _ => next_block(),
        }
    }

    // ANCHOR: apply
    async fn apply(
        &mut self,
        ctx: &AdapterContext<DemoConfig>,
        cmd: CommandEnvelope,
    ) -> Result<(), EmitError> {
        let now = ctx.now();
        let Command::Climate(c) = &cmd.command else {
            return Ok(()); // the actuator only sends declared kinds
        };
        let schedule_target = ctx.config().initial_target;
        let until = cmd.command.hold().map(|h| self.override_end(ctx, now, h));
        let Some(z) = self.zones.iter_mut().find(|z| &z.id == c.zone()) else {
            return ctx
                .health()
                .degraded(format!(
                    "command {} names unknown zone {}",
                    cmd.id,
                    c.zone()
                ))
                .await;
        };
        ctx.health().ok().await?;
        match c {
            ClimateCommand::SetTarget { target, .. } => {
                z.mode = ClimateMode::Manual;
                z.target = *target; // already checked against the instance's band
                z.override_until = until;
            }
            ClimateCommand::TurnOff { .. } => {
                z.mode = ClimateMode::Off;
                z.override_until = until;
            }
            // ResumeSchedule, and anything newer: back to the schedule.
            _ => {
                z.mode = ClimateMode::Schedule;
                z.target = schedule_target;
                z.override_until = None;
            }
        }
        ctx.emit(state(z, now, None)).await
    }
    // ANCHOR_END: apply
}

fn state(z: &Zone, now: Timestamp, measured: Option<f64>) -> ClimateState {
    let heating = z.mode != ClimateMode::Off && z.temperature < z.target.get() - 0.1;
    let mut s = ClimateState::new(z.id.clone(), now, z.mode).with_heating_pct(if heating {
        100.0
    } else {
        0.0
    });
    if z.mode != ClimateMode::Off {
        s = s.with_target_c(z.target.get());
    }
    if let Some(m) = measured {
        s = s.with_measured_c(m);
    }
    s
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// xorshift64*: tiny, deterministic, good enough for sensor noise.
#[derive(Debug, Clone)]
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        // Avoid the all-zero state.
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    /// Uniform in [-amplitude, amplitude].
    fn noise(&mut self, amplitude: f64) -> f64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        let unit = (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64;
        (unit * 2.0 - 1.0) * amplitude
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(toml: &str) -> DemoConfig {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn validates_config() {
        assert!(Demo::new(&cfg("")).is_ok());
        assert!(Demo::new(&cfg("interval = \"0s\"")).is_err());
        assert!(Demo::new(&cfg("zones = []")).is_err());
        assert!(Demo::new(&cfg("zones = [\"a\", \"a\"]")).is_err());
        assert!(Demo::new(&cfg("schedule_block = \"10s\"")).is_err());
        assert!(toml::from_str::<DemoConfig>("initial_target = 35.0").is_err());
        assert_eq!(default_target().get(), 19.0);
    }

    #[test]
    fn noise_is_bounded_and_deterministic() {
        let mut a = XorShift::new(7);
        let mut b = XorShift::new(7);
        for _ in 0..10_000 {
            let x = a.noise(0.5);
            assert!((-0.5..=0.5).contains(&x));
            assert_eq!(x, b.noise(0.5));
        }
    }
}
