//! `system`: a read-only [heimr](https://github.com/hugsmid/heimr) adapter
//! that records the host's own health, so the hub and the services around
//! it can be watched and optimised like any other data.
//!
//! Every `interval` it emits readings for:
//!
//! | Device | Metrics |
//! |---|---|
//! | `cpu`, `cpu0`… | `cpu.usage` (%), and for `cpu` also `cpu.user`, `cpu.system`, `cpu.iowait`, `cpu.steal` |
//! | `host` | `load.1`/`.5`/`.15`, `tasks.running`, `tasks.total`, `uptime` (s), `memory.total`/`.available`/`.used` (B), `memory.usage` (%), `swap.total`/`.used` (B), `pressure.cpu`/`.memory`/`.io` (% of time stalled, 10 s average, where the kernel has PSI) |
//! | `fs:<mount>` | `fs.size`/`.used`/`.available` (B), `fs.usage`, `fs.inodes.usage` (%) |
//! | `disk:<device>` | `disk.read`/`.write` (B/s), `disk.busy` (%) |
//! | `net:<interface>` | `net.rx`/`.tx` (B/s), `net.errors`, `net.drops` (totals since boot) |
//! | `temp:<chip>.<sensor>` | `temperature` (°C) |
//! | `proc:<name>` | `proc.cpu` (% of one core), `proc.rss` (B), `proc.threads`, `proc.count`; for named processes also `proc.fds` and `proc.read`/`.write` (B/s, where readable) |
//! | `unit:<name>` | `unit.cpu` (% of one core), `unit.memory` (B), `unit.tasks`, `unit.read`/`.write` (B/s) — systemd units on cgroup v2 |
//! | `hub` | the hub process itself: `proc.cpu`, `proc.rss`, `proc.threads`, `proc.fds`, `proc.read`/`.write` |
//!
//! Every series is stamped by the host clock when sampled and expected
//! every `interval` ([`series_template`]); `uptime`, `net.errors` and
//! `net.drops` are counters, the rest gauges. All are diagnostics
//! (retention class `Diagnostic`).
//!
//! Host metrics multiply quickly (per core, disk, interface, process and
//! unit), so series are counted and capped: `series` lists the ones to
//! send (default all), and `max_series` (default 1 000) is a ceiling. A
//! new series beyond it is not sent, and health is degraded saying how
//! many were held back. The count is the custom gauge `series`. The `top`
//! processes are off by default: their names churn, and each new name is
//! a new series that would fill the ceiling; name the processes to watch
//! in `processes` instead, or opt in with `top`.
//!
//! Rates need two samples, so they start with the second one. Processes
//! are grouped by command name; the `top` names by CPU and by memory (if
//! opted in) are
//! reported each sample, and so are the `processes` named in the config.
//!
//! <!-- ANCHOR: toml -->
//! ```toml
//! [adapters.host]
//! type = "system"
//! interval = "10s"                 # 1s ..= 1h
//! top = 5                          # top process names by CPU and by memory (default 0: none)
//! processes = ["heimr", "sshd"]
//! units = ["heimr.service"]        # systemd units (cgroup v2)
//! # mounts = ["/", "/var"]         # default: every block-device filesystem
//! # interfaces = ["eth0"]          # default: all but loopback
//! # per_core = true
//! # temperatures = true
//! # hub = true
//! # series = ["cpu.usage", "memory.*", "host/load.1"]   # default: all
//! # max_series = 1000
//! # proc_path = "/proc"            # e.g. "/host/proc" in a container
//! # sys_path = "/sys"
//! # cgroup_path = "/sys/fs/cgroup"
//! # root_path = "/"                # prefix for mount points
//! ```
//! <!-- ANCHOR_END: toml -->
//!
//! # Cost
//! The adapter reads `/proc`, `/sys` and cgroup files directly with small
//! allocation-free parsers, on a blocking thread so the async runtime is
//! never held up. Buffers, previous counters and device ids are kept
//! between samples. `cargo bench -p heimr-system` measures one sample on
//! your host.
//!
//! # Platforms
//! Linux. Elsewhere it compiles and runs, and the default settings are
//! valid, but it finds no `/proc`: it reports health `degraded` and emits
//! nothing.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod parse;
pub mod sampler;

use std::path::PathBuf;

use heimr_core::{
    Adapter, AdapterContext, AdapterDescriptor, AdapterError, Capabilities, Metric, Retention,
    SeriesTemplate,
};
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;

pub use sampler::{Options, Sample, Sampler, SystemMetric};

/// Instance settings. Every key is optional; [`Config::default`] is the
/// settings of an instance section with only `type = "system"`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct Config {
    /// Time between samples (1 s ..= 1 h; default 10 s).
    #[serde(default = "default_interval")]
    pub interval: SignedDuration,
    /// Where procfs is mounted.
    #[serde(default = "default_proc")]
    pub proc_path: PathBuf,
    /// Where sysfs is mounted.
    #[serde(default = "default_sys")]
    pub sys_path: PathBuf,
    /// Where the cgroup v2 hierarchy is mounted (default
    /// `<sys_path>/fs/cgroup`).
    #[serde(default)]
    pub cgroup_path: Option<PathBuf>,
    /// Prefix for mount points from the mount table.
    #[serde(default = "default_root")]
    pub root_path: PathBuf,
    /// Per-core CPU usage.
    #[serde(default = "yes")]
    pub per_core: bool,
    /// Temperatures.
    #[serde(default = "yes")]
    pub temperatures: bool,
    /// Only these mount points.
    #[serde(default)]
    pub mounts: Vec<String>,
    /// Only these network interfaces.
    #[serde(default)]
    pub interfaces: Vec<String>,
    /// Processes always reported, by command name.
    #[serde(default)]
    pub processes: Vec<String>,
    /// Top process names by CPU and by memory (0 ..= 50; default 0, none).
    /// Opt-in: the names change, and each new one is a new `proc:` series
    /// counted against `max_series`.
    #[serde(default = "default_top")]
    pub top: usize,
    /// systemd units on cgroup v2: a unit name (looked up in
    /// `system.slice`) or a path relative to the cgroup root.
    #[serde(default)]
    pub units: Vec<String>,
    /// The hub process's own stats.
    #[serde(default = "yes")]
    pub hub: bool,
    /// Only these series (default: all). An entry is a metric name
    /// (`cpu.usage`), a metric prefix ending in `.*` (`proc.*`), or one
    /// device's metric (`host/load.1`).
    #[serde(default)]
    pub series: Vec<String>,
    /// The most series (device and metric pairs) the instance sends
    /// (1 ..= 100 000; default 1 000). New series beyond it are not sent,
    /// and health is degraded saying how many were held back.
    #[serde(default = "default_max_series")]
    pub max_series: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interval: default_interval(),
            proc_path: default_proc(),
            sys_path: default_sys(),
            cgroup_path: None,
            root_path: default_root(),
            per_core: true,
            temperatures: true,
            mounts: Vec::new(),
            interfaces: Vec::new(),
            processes: Vec::new(),
            top: default_top(),
            units: Vec::new(),
            hub: true,
            series: Vec::new(),
            max_series: default_max_series(),
        }
    }
}

fn default_max_series() -> usize {
    1000
}

/// The highest `max_series`.
pub const MAX_SERIES: usize = 100_000;

/// One `series` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Allow {
    Metric(&'static str),
    Prefix(String),
    Exact(String, &'static str),
}

/// Which series an instance sends: the allow-list, then the ceiling.
/// Counts every series it has let through.
#[derive(Debug, Clone)]
pub struct SeriesGate {
    allow: Vec<Allow>,
    max: usize,
    seen: std::collections::HashSet<(u32, SystemMetric)>,
}

/// What [`SeriesGate::admit`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Admit {
    /// Send it.
    Yes,
    /// Not in the allow-list.
    NotListed,
    /// A new series beyond the ceiling.
    OverCeiling,
}

impl SeriesGate {
    /// A gate for the `series` allow-list (empty: all) and the
    /// `max_series` ceiling. Errors name the offending entry.
    pub fn new(series: &[String], max: usize) -> Result<Self, String> {
        let known = |name: &str| {
            SystemMetric::ALL
                .iter()
                .find(|m| m.name() == name)
                .map(|m| m.name())
        };
        let mut allow = Vec::with_capacity(series.len());
        for e in series {
            let entry = if let Some(prefix) = e.strip_suffix(".*") {
                let p = format!("{prefix}.");
                if !SystemMetric::ALL.iter().any(|m| m.name().starts_with(&p)) {
                    return Err(format!("{e:?} matches no metric"));
                }
                Allow::Prefix(p)
            } else if let Some((device, metric)) = e.rsplit_once('/') {
                let m = known(metric).ok_or_else(|| format!("{e:?}: unknown metric {metric:?}"))?;
                if device.is_empty() {
                    return Err(format!("{e:?}: empty device"));
                }
                Allow::Exact(device.to_owned(), m)
            } else {
                Allow::Metric(known(e).ok_or_else(|| format!("unknown metric {e:?}"))?)
            };
            allow.push(entry);
        }
        Ok(Self {
            allow,
            max,
            seen: std::collections::HashSet::new(),
        })
    }

    /// Whether to send `metric` of device `device` (interned as `index`).
    pub fn admit(&mut self, index: u32, device: &str, metric: SystemMetric) -> Admit {
        let name = metric.name();
        let listed = self.allow.is_empty()
            || self.allow.iter().any(|a| match a {
                Allow::Metric(m) => *m == name,
                Allow::Prefix(p) => name.starts_with(p.as_str()),
                Allow::Exact(d, m) => d == device && *m == name,
            });
        if !listed {
            return Admit::NotListed;
        }
        if self.seen.contains(&(index, metric)) {
            return Admit::Yes;
        }
        if self.seen.len() >= self.max {
            return Admit::OverCeiling;
        }
        self.seen.insert((index, metric));
        Admit::Yes
    }

    /// How many series have been let through.
    pub fn count(&self) -> usize {
        self.seen.len()
    }
}

fn default_interval() -> SignedDuration {
    SignedDuration::from_secs(10)
}
fn default_proc() -> PathBuf {
    PathBuf::from("/proc")
}
fn default_sys() -> PathBuf {
    PathBuf::from("/sys")
}
fn default_root() -> PathBuf {
    PathBuf::from("/")
}
fn yes() -> bool {
    true
}
/// No top-N processes by default: their names change from sample to
/// sample, and every new name is a new series that fills `max_series`.
fn default_top() -> usize {
    0
}

/// The `system` adapter. See the [crate docs](crate).
#[derive(Debug)]
pub struct System {
    options: Options,
    interval: SignedDuration,
}

const MAX_LIST: usize = 64;

fn check_list(key: &str, list: &[String], path_like: bool) -> Result<(), AdapterError> {
    if list.len() > MAX_LIST {
        return Err(AdapterError::config_key(
            key,
            format!("at most {MAX_LIST} entries"),
        ));
    }
    for v in list {
        if v.trim().is_empty() || v.len() > 255 || v.contains('\0') {
            return Err(AdapterError::config_key(
                key,
                format!("invalid entry {v:?}"),
            ));
        }
        if path_like && v.split('/').any(|c| c == "..") {
            return Err(AdapterError::config_key(
                key,
                format!("{v:?} must not contain .."),
            ));
        }
    }
    Ok(())
}

/// Paths must start at a root. On unix that is the same as absolute; on
/// Windows `/proc` has a root but no drive prefix, so `is_absolute` would
/// make the defaults a configuration error instead of the documented
/// degraded instance that emits nothing.
fn check_path(key: &str, p: &std::path::Path) -> Result<(), AdapterError> {
    if p.has_root() {
        Ok(())
    } else {
        Err(AdapterError::config_key(key, "must be an absolute path"))
    }
}

/// The series template of one metric: sampled by the host clock every
/// `interval` (the default configuration's, 10 s); totals since boot are
/// counters (they restart from zero at boot), everything else a gauge.
pub fn series_template(m: SystemMetric) -> SeriesTemplate {
    let metric = Sampler::metric(m);
    let interval = default_interval();
    let t = match m {
        SystemMetric::Uptime | SystemMetric::NetErrors | SystemMetric::NetDrops => {
            SeriesTemplate::counter(metric, m.unit(), interval, None)
        }
        _ => SeriesTemplate::gauge(metric, m.unit(), interval),
    };
    // Host health is diagnostics: kept for a shorter time.
    t.with_retention(Retention::Diagnostic)
}

impl Adapter for System {
    type Config = Config;

    fn descriptor() -> AdapterDescriptor {
        let d = AdapterDescriptor::new(
            "system",
            "the host's CPU, memory, disks, network, temperatures, processes and services",
            Capabilities::read_only(),
        );
        SystemMetric::ALL
            .iter()
            .fold(d, |d, m| d.with_series(series_template(*m)))
    }

    fn new(c: &Config) -> Result<Self, AdapterError> {
        if c.interval < SignedDuration::from_secs(1) || c.interval > SignedDuration::from_hours(1) {
            return Err(AdapterError::config_key(
                "interval",
                "must be between 1s and 1h",
            ));
        }
        if c.top > 50 {
            return Err(AdapterError::config_key("top", "must be at most 50"));
        }
        check_path("proc_path", &c.proc_path)?;
        check_path("sys_path", &c.sys_path)?;
        check_path("root_path", &c.root_path)?;
        if let Some(p) = &c.cgroup_path {
            check_path("cgroup_path", p)?;
        }
        check_list("mounts", &c.mounts, true)?;
        check_list("interfaces", &c.interfaces, false)?;
        check_list("processes", &c.processes, false)?;
        check_list("units", &c.units, true)?;
        check_list("series", &c.series, false)?;
        if !(1..=MAX_SERIES).contains(&c.max_series) {
            return Err(AdapterError::config_key(
                "max_series",
                format!("must be between 1 and {MAX_SERIES}"),
            ));
        }
        SeriesGate::new(&c.series, c.max_series)
            .map_err(|e| AdapterError::config_key("series", e))?;
        let mut o = Options::new(&c.proc_path, &c.sys_path);
        if let Some(p) = &c.cgroup_path {
            o.cgroup_path = p.clone();
        }
        o.root_path = c.root_path.clone();
        o.per_core = c.per_core;
        o.temperatures = c.temperatures;
        o.mounts = c.mounts.clone();
        o.interfaces = c.interfaces.clone();
        o.processes = c.processes.clone();
        o.top = c.top;
        o.units = c.units.clone();
        o.hub = c.hub;
        Ok(Self {
            options: o,
            interval: c.interval,
        })
    }

    /// The kind's templates, expected every configured `interval`.
    fn series(&self) -> Vec<SeriesTemplate> {
        Self::descriptor()
            .series
            .into_iter()
            .map(|t| t.with_expected_interval(self.interval))
            .collect()
    }

    async fn run(self, ctx: AdapterContext<Config>) -> Result<(), AdapterError> {
        let metrics: Vec<Metric> = SystemMetric::ALL
            .iter()
            .map(|m| Sampler::metric(*m))
            .collect();
        let interval = ctx.config().interval;
        let mut gate = SeriesGate::new(&ctx.config().series, ctx.config().max_series)
            .map_err(|e| AdapterError::config_key("series", e))?;
        let mut sampler = Some(Box::new(Sampler::new(self.options)));
        let mut out: Vec<Sample> = Vec::with_capacity(512);
        let mut next = ctx.now();
        loop {
            tokio::select! {
                () = ctx.shutdown().wait() => return Ok(()),
                () = ctx.sleep_until(next) => {}
            }
            let now = ctx.now();
            let mono = ctx.clock().monotonic().as_secs_f64();
            let Some(mut s) = sampler.take() else {
                return Err(AdapterError::contract("sampler lost"));
            };
            let mut buf = std::mem::take(&mut out);
            // File reads can stall (a busy disk, a slow sysfs driver):
            // keep them off the async runtime.
            let task = tokio::task::spawn_blocking(move || {
                let problem = s.sample(mono, &mut buf);
                (s, buf, problem)
            });
            let joined = tokio::select! {
                () = ctx.shutdown().wait() => return Ok(()),
                j = task => j,
            };
            let (s, mut samples, problem) = joined.map_err(|e| {
                AdapterError::transient(if e.is_panic() {
                    "the sampler panicked"
                } else {
                    "the sampler was cancelled"
                })
            })?;
            // Every value is sent as sampled: a non-finite one (none is
            // expected) is recorded by the store as bad quality.
            let mut held = 0usize;
            for x in &samples {
                match gate.admit(x.device, s.device(x.device).as_str(), x.metric) {
                    Admit::Yes => {}
                    Admit::NotListed => continue,
                    _ => {
                        held += 1;
                        continue;
                    }
                }
                let r = ctx
                    .reading(
                        s.device(x.device).clone(),
                        metrics[x.metric as usize].clone(),
                        x.value,
                        x.metric.unit(),
                    )
                    .with_time(now);
                ctx.emit(r).await?;
            }
            ctx.metrics().gauge("series", gate.count() as f64);
            let over = (held > 0).then(|| {
                format!(
                    "{held} more series would exceed max_series ({}); not sent",
                    ctx.config().max_series
                )
            });
            match (problem, over) {
                (None, None) => ctx.health().ok().await?,
                (Some(p), None) | (None, Some(p)) => ctx.health().degraded(p).await?,
                (Some(p), Some(o)) => ctx.health().degraded(format!("{p}; {o}")).await?,
            }
            samples.clear();
            out = samples;
            sampler = Some(s);
            next = next.saturating_add(interval).unwrap_or(Timestamp::MAX);
            let now = ctx.now();
            if next <= now {
                // Fell behind (suspend, clock step): skip missed samples.
                next = now.saturating_add(interval).unwrap_or(Timestamp::MAX);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn the_gate_applies_the_allow_list_then_the_ceiling() {
        let list = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let mut g = SeriesGate::new(&list(&["cpu.usage", "memory.*", "host/load.1"]), 3).unwrap();
        use SystemMetric as M;
        assert_eq!(g.admit(0, "cpu", M::CpuUsage), Admit::Yes);
        assert_eq!(g.admit(0, "cpu", M::CpuUser), Admit::NotListed);
        assert_eq!(g.admit(1, "host", M::Load1), Admit::Yes);
        assert_eq!(
            g.admit(2, "box", M::Load1),
            Admit::NotListed,
            "another device"
        );
        assert_eq!(g.admit(1, "host", M::MemTotal), Admit::Yes);
        assert_eq!(g.count(), 3);
        // At the ceiling: known series still pass, new ones do not.
        assert_eq!(g.admit(1, "host", M::MemUsed), Admit::OverCeiling);
        assert_eq!(g.admit(0, "cpu", M::CpuUsage), Admit::Yes);
        assert_eq!(g.count(), 3);
        // An empty list allows every metric.
        let mut all = SeriesGate::new(&[], 10).unwrap();
        assert_eq!(all.admit(7, "proc:x", M::ProcFds), Admit::Yes);
        for (bad, why) in [
            ("cpu.nope", "unknown metric"),
            ("nothing.*", "matches no metric"),
            ("/load.1", "empty device"),
            ("host/load.9", "unknown metric"),
        ] {
            let e = SeriesGate::new(&list(&[bad]), 10).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
    }

    #[test]
    fn series_settings_are_checked() {
        let with = |v: serde_json::Value| {
            let mut c = serde_json::json!({});
            for (k, x) in v.as_object().unwrap() {
                c[k] = x.clone();
            }
            System::new(&serde_json::from_value(c).unwrap())
        };
        assert!(with(serde_json::json!({ "series": ["proc.*"], "max_series": 5 })).is_ok());
        for bad in [
            serde_json::json!({ "series": ["load"] }),
            serde_json::json!({ "max_series": 0 }),
            serde_json::json!({ "max_series": 100_001 }),
        ] {
            match with(bad.clone()) {
                Err(AdapterError::Config { key: Some(k), .. }) => {
                    assert_eq!(k, bad.as_object().unwrap().keys().next().unwrap().as_str());
                }
                other => panic!("{bad}: {other:?}"),
            }
        }
    }

    #[test]
    fn instance_series_follow_the_configured_interval() {
        let c: Config = serde_json::from_value(serde_json::json!({ "interval": "60s" })).unwrap();
        let s = System::new(&c).unwrap().series();
        let d = System::descriptor();
        assert_eq!(s.len(), d.series.len());
        assert!(
            s.iter()
                .all(|t| t.expected_interval == SignedDuration::from_mins(1))
        );
        assert!(
            s.iter()
                .all(|t| t.staleness_limit() == SignedDuration::from_mins(3))
        );
        assert!(
            d.series
                .iter()
                .all(|t| t.expected_interval == SignedDuration::from_secs(10))
        );
        d.check_instance_series(&s).unwrap();
    }

    #[test]
    fn top_processes_are_opt_in() {
        assert_eq!(Config::default().top, 0);
        let c: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(c.top, 0);
    }

    #[test]
    fn every_template_is_diagnostic() {
        let d = System::descriptor();
        assert_eq!(d.series.len(), SystemMetric::ALL.len());
        assert!(
            d.series
                .iter()
                .all(|t| t.retention == Retention::Diagnostic)
        );
    }

    #[test]
    fn defaults_are_valid_on_every_platform() {
        let parsed: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(parsed, Config::default());
        assert!(System::new(&Config::default()).is_ok());
    }

    #[test]
    fn relative_paths_are_refused() {
        for p in ["relative/proc", "proc", "./proc", ""] {
            assert!(check_path("proc_path", Path::new(p)).is_err(), "{p:?}");
        }
        assert!(check_path("proc_path", Path::new("/proc")).is_ok());
    }
}
