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
//! Rates need two samples, so they start with the second one. Processes
//! are grouped by command name; the `top` names by CPU and by memory are
//! reported each sample, and so are the `processes` named in the config.
//!
//! <!-- ANCHOR: toml -->
//! ```toml
//! [adapters.host]
//! type = "system"
//! interval = "10s"                 # 1s ..= 1h
//! top = 5                          # top process names by CPU and by memory (0 = none)
//! processes = ["heimr", "sshd"]
//! units = ["heimr.service"]        # systemd units (cgroup v2)
//! # mounts = ["/", "/var"]         # default: every block-device filesystem
//! # interfaces = ["eth0"]          # default: all but loopback
//! # per_core = true
//! # temperatures = true
//! # hub = true
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

use heimr_core::{Adapter, AdapterContext, AdapterDescriptor, AdapterError, Capabilities, Metric};
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
    /// Top process names by CPU and by memory (0 ..= 50; default 5).
    #[serde(default = "default_top")]
    pub top: usize,
    /// systemd units on cgroup v2: a unit name (looked up in
    /// `system.slice`) or a path relative to the cgroup root.
    #[serde(default)]
    pub units: Vec<String>,
    /// The hub process's own stats.
    #[serde(default = "yes")]
    pub hub: bool,
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
        }
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
fn default_top() -> usize {
    5
}

/// The `system` adapter. See the [crate docs](crate).
#[derive(Debug)]
pub struct System {
    options: Options,
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

impl Adapter for System {
    type Config = Config;

    fn descriptor() -> AdapterDescriptor {
        AdapterDescriptor::new(
            "system",
            "the host's CPU, memory, disks, network, temperatures, processes and services",
            Capabilities::read_only(),
        )
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
        Ok(Self { options: o })
    }

    async fn run(self, ctx: AdapterContext<Config>) -> Result<(), AdapterError> {
        let metrics: Vec<Metric> = SystemMetric::ALL
            .iter()
            .map(|m| Sampler::metric(*m))
            .collect();
        let interval = ctx.config().interval;
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
            for x in &samples {
                if !x.value.is_finite() {
                    continue;
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
            match problem {
                None => ctx.health().ok().await?,
                Some(p) => ctx.health().degraded(p).await?,
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
