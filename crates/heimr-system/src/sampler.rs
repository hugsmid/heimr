//! One sample of the host: reads `/proc`, `/sys` and cgroup files and turns
//! them into `(device, metric, value)` triples.
//!
//! The sampler keeps its read buffers, path buffers, previous counters and
//! interned device ids between samples, so a steady-state sample allocates
//! nothing for the host-wide part. Rates (CPU %, bytes per second) need two
//! samples; the first sample emits only gauges.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};

use heimr_core::{DeviceId, Metric, Unit};

use crate::parse::{self, CpuTimes};

macro_rules! metrics {
    ($($v:ident => $name:literal, $unit:ident, $doc:literal;)*) => {
        /// A metric the adapter emits. Each variant's docs give the name
        /// it is emitted as and its unit.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum SystemMetric {
            $(
                #[doc = concat!("`", $name, "` (", stringify!($unit), "): ", $doc)]
                $v,
            )*
        }

        impl SystemMetric {
            /// Every metric, in declaration order.
            pub const ALL: &'static [SystemMetric] = &[$(SystemMetric::$v,)*];

            /// The metric's name, as emitted.
            pub fn name(self) -> &'static str {
                match self { $(SystemMetric::$v => $name,)* }
            }

            /// The metric's unit.
            pub fn unit(self) -> Unit {
                match self { $(SystemMetric::$v => Unit::$unit,)* }
            }
        }
    };
}

metrics! {
    CpuUsage => "cpu.usage", Percent, "Share of CPU time not idle, for all CPUs (`cpu`) or one core (`cpuN`).";
    CpuUser => "cpu.user", Percent, "Share of CPU time in user space.";
    CpuSystem => "cpu.system", Percent, "Share of CPU time in the kernel.";
    CpuIowait => "cpu.iowait", Percent, "Share of CPU time idle while waiting for I/O.";
    CpuSteal => "cpu.steal", Percent, "Share of CPU time taken by the hypervisor for other guests.";
    Load1 => "load.1", Dimensionless, "Load average over 1 minute.";
    Load5 => "load.5", Dimensionless, "Load average over 5 minutes.";
    Load15 => "load.15", Dimensionless, "Load average over 15 minutes.";
    TasksRunning => "tasks.running", Dimensionless, "Runnable tasks.";
    TasksTotal => "tasks.total", Dimensionless, "All tasks.";
    Uptime => "uptime", Second, "Time since boot.";
    MemTotal => "memory.total", Byte, "Total usable memory.";
    MemAvailable => "memory.available", Byte, "Memory available for new work without swapping.";
    MemUsed => "memory.used", Byte, "Memory in use (total minus available).";
    MemUsage => "memory.usage", Percent, "Memory in use as a share of the total.";
    SwapTotal => "swap.total", Byte, "Total swap space.";
    SwapUsed => "swap.used", Byte, "Swap space in use.";
    PressureCpu => "pressure.cpu", Percent, "Share of time some task was stalled waiting for CPU (10 s average).";
    PressureMemory => "pressure.memory", Percent, "Share of time some task was stalled waiting for memory (10 s average).";
    PressureIo => "pressure.io", Percent, "Share of time some task was stalled waiting for I/O (10 s average).";
    FsSize => "fs.size", Byte, "Filesystem size.";
    FsUsed => "fs.used", Byte, "Filesystem space in use.";
    FsAvailable => "fs.available", Byte, "Filesystem space available to unprivileged users.";
    FsUsage => "fs.usage", Percent, "Filesystem space in use as a share of its size.";
    FsInodes => "fs.inodes.usage", Percent, "Inodes in use as a share of all inodes.";
    DiskRead => "disk.read", BytePerSecond, "Bytes read from a disk per second.";
    DiskWrite => "disk.write", BytePerSecond, "Bytes written to a disk per second.";
    DiskBusy => "disk.busy", Percent, "Share of time a disk was busy with I/O.";
    NetRx => "net.rx", BytePerSecond, "Bytes received on an interface per second.";
    NetTx => "net.tx", BytePerSecond, "Bytes sent on an interface per second.";
    NetErrors => "net.errors", Dimensionless, "Receive and transmit errors on an interface since boot.";
    NetDrops => "net.drops", Dimensionless, "Received and transmitted packets dropped on an interface since boot.";
    Temperature => "temperature", Celsius, "A temperature sensor's reading.";
    ProcCpu => "proc.cpu", Percent, "CPU used by a process group, as a share of one core.";
    ProcRss => "proc.rss", Byte, "Resident memory of a process group.";
    ProcThreads => "proc.threads", Dimensionless, "Threads in a process group.";
    ProcCount => "proc.count", Dimensionless, "Processes in a process group.";
    ProcFds => "proc.fds", Dimensionless, "Open file descriptors of a process group.";
    ProcRead => "proc.read", BytePerSecond, "Bytes a process group read from storage per second.";
    ProcWrite => "proc.write", BytePerSecond, "Bytes a process group wrote to storage per second.";
    UnitCpu => "unit.cpu", Percent, "CPU used by a service, as a share of one core.";
    UnitMemory => "unit.memory", Byte, "Memory charged to a service.";
    UnitTasks => "unit.tasks", Dimensionless, "Tasks in a service.";
    UnitRead => "unit.read", BytePerSecond, "Bytes a service read from storage per second.";
    UnitWrite => "unit.write", BytePerSecond, "Bytes a service wrote to storage per second.";
}

/// One value of one sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// Index into [`Sampler::device`].
    pub device: u32,
    /// What.
    pub metric: SystemMetric,
    /// Value in the metric's unit.
    pub value: f64,
}

/// What to sample. Built from the adapter's config.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Options {
    /// Where procfs is mounted (`/proc`).
    pub proc_path: PathBuf,
    /// Where sysfs is mounted (`/sys`).
    pub sys_path: PathBuf,
    /// Where the cgroup v2 hierarchy is mounted (`/sys/fs/cgroup`).
    pub cgroup_path: PathBuf,
    /// Prefix for mount points from the mount table (`/`; e.g. `/host` when
    /// the host's root is mounted there).
    pub root_path: PathBuf,
    /// Per-core CPU usage.
    pub per_core: bool,
    /// Temperatures from hwmon (or thermal zones).
    pub temperatures: bool,
    /// Only these mount points (empty: every block-device filesystem).
    pub mounts: Vec<String>,
    /// Only these interfaces (empty: all but loopback).
    pub interfaces: Vec<String>,
    /// Processes to report by command name, always.
    pub processes: Vec<String>,
    /// Also the top N processes (by command name) by CPU and by memory.
    pub top: usize,
    /// systemd units (cgroup v2), by name (`system.slice/<unit>`) or path
    /// relative to the cgroup root.
    pub units: Vec<String>,
    /// The hub process's own stats (`<proc>/self`).
    pub hub: bool,
}

impl Options {
    /// Defaults for the given procfs and sysfs paths.
    pub fn new(proc_path: impl Into<PathBuf>, sys_path: impl Into<PathBuf>) -> Self {
        let sys_path = sys_path.into();
        Self {
            proc_path: proc_path.into(),
            cgroup_path: sys_path.join("fs/cgroup"),
            sys_path,
            root_path: PathBuf::from("/"),
            per_core: true,
            temperatures: true,
            mounts: Vec::new(),
            interfaces: Vec::new(),
            processes: Vec::new(),
            top: 5,
            units: Vec::new(),
            hub: true,
        }
    }
}

#[derive(Debug, Default)]
struct Agg {
    cpu: f64,
    rss: f64,
    threads: f64,
    count: f64,
    fds: f64,
    read: f64,
    write: f64,
    io: bool,
    named: bool,
    device: Option<u32>,
    epoch: u64,
}

/// One command name's totals, copied out for ranking.
#[derive(Debug, Clone, Copy)]
struct Ranked {
    device: u32,
    cpu: f64,
    rss: f64,
    threads: f64,
    count: f64,
    fds: f64,
    read: f64,
    write: f64,
    io: bool,
    named: bool,
}

#[derive(Debug, Clone, Copy)]
struct PidPrev {
    ticks: u64,
    epoch: u64,
}

#[derive(Debug)]
struct Temp {
    device: u32,
    input: PathBuf,
}

/// Samples the host. See the [module docs](self).
#[derive(Debug)]
pub struct Sampler {
    o: Options,
    stat: PathBuf,
    loadavg: PathBuf,
    uptime: PathBuf,
    meminfo: PathBuf,
    pressure: [PathBuf; 3],
    mounts: PathBuf,
    diskstats: PathBuf,
    net_dev: PathBuf,
    buf: Vec<u8>,
    path: String,
    key: String,
    devices: Vec<DeviceId>,
    by_key: HashMap<String, u32>,
    prev: HashMap<(u32, SystemMetric), (f64, f64)>,
    prev_cpu: Vec<Option<CpuTimes>>,
    whole_disk: HashMap<String, bool>,
    temps: Vec<Temp>,
    temps_age: u32,
    pids: HashMap<u32, PidPrev>,
    comms: HashMap<String, Agg>,
    ranked: Vec<Ranked>,
    selected: Vec<u32>,
    last_scan: Option<f64>,
    fs_seen: HashMap<String, u64>,
    units: Vec<(String, PathBuf)>,
    epoch: u64,
    ticks_per_second: f64,
    page_size: f64,
    problem: Option<String>,
}

fn read_into<'b>(buf: &'b mut Vec<u8>, path: &Path) -> std::io::Result<Cow<'b, str>> {
    buf.clear();
    std::fs::File::open(path)?.read_to_end(buf)?;
    Ok(String::from_utf8_lossy(buf))
}

/// A device id from a prefix and a raw name: disallowed characters become
/// `_`, `/` becomes `.`, at most 64 bytes.
fn sanitise(out: &mut String, prefix: &str, raw: &str) {
    out.clear();
    out.push_str(prefix);
    let raw = raw.trim_matches('/');
    if raw.is_empty() && prefix.ends_with(':') {
        out.push_str("root");
    }
    for c in raw.chars() {
        if out.len() >= heimr_core::id::MAX_ID_LEN {
            break;
        }
        out.push(match c {
            '/' => '.',
            c if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-') => c,
            _ => '_',
        });
    }
}

fn pct(part: f64, whole: f64) -> f64 {
    if whole > 0.0 {
        (part / whole * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    }
}

#[cfg(unix)]
fn clock_ticks_and_page_size() -> (f64, f64) {
    (
        rustix::param::clock_ticks_per_second() as f64,
        rustix::param::page_size() as f64,
    )
}

#[cfg(not(unix))]
fn clock_ticks_and_page_size() -> (f64, f64) {
    (100.0, 4096.0)
}

/// Filesystem size, used, available bytes and inode usage.
#[cfg(unix)]
fn statvfs(path: &Path) -> Option<(f64, f64, f64, Option<f64>)> {
    let s = rustix::fs::statvfs(path).ok()?;
    let frsize = s.f_frsize as f64;
    let size = s.f_blocks as f64 * frsize;
    let free = s.f_bfree as f64 * frsize;
    let avail = s.f_bavail as f64 * frsize;
    let inodes = (s.f_files > 0).then(|| {
        pct(
            (s.f_files - s.f_ffree.min(s.f_files)) as f64,
            s.f_files as f64,
        )
    });
    Some((size, (size - free).max(0.0), avail, inodes))
}

#[cfg(not(unix))]
fn statvfs(_: &Path) -> Option<(f64, f64, f64, Option<f64>)> {
    None
}

/// Count the entries of a directory (open file descriptors).
fn count_entries(path: &Path) -> Option<f64> {
    Some(std::fs::read_dir(path).ok()?.count() as f64)
}

impl Sampler {
    /// A sampler for `options`.
    pub fn new(o: Options) -> Self {
        let p = |rel: &str| o.proc_path.join(rel);
        let (ticks_per_second, page_size) = clock_ticks_and_page_size();
        let units = o
            .units
            .iter()
            .map(|u| {
                let dir = if u.contains('/') {
                    o.cgroup_path.join(u.trim_start_matches('/'))
                } else {
                    o.cgroup_path.join("system.slice").join(u)
                };
                (u.rsplit('/').next().unwrap_or(u).to_owned(), dir)
            })
            .collect();
        Self {
            stat: p("stat"),
            loadavg: p("loadavg"),
            uptime: p("uptime"),
            meminfo: p("meminfo"),
            pressure: [p("pressure/cpu"), p("pressure/memory"), p("pressure/io")],
            mounts: p("mounts"),
            diskstats: p("diskstats"),
            net_dev: p("net/dev"),
            o,
            buf: Vec::with_capacity(16 * 1024),
            path: String::with_capacity(256),
            key: String::with_capacity(80),
            devices: Vec::new(),
            by_key: HashMap::new(),
            prev: HashMap::new(),
            prev_cpu: Vec::new(),
            whole_disk: HashMap::new(),
            temps: Vec::new(),
            temps_age: u32::MAX,
            pids: HashMap::new(),
            comms: HashMap::new(),
            ranked: Vec::new(),
            selected: Vec::new(),
            last_scan: None,
            fs_seen: HashMap::new(),
            units,
            epoch: 0,
            ticks_per_second: ticks_per_second.max(1.0),
            page_size,
            problem: None,
        }
    }

    /// The device id at `index` (from [`Sample::device`]).
    pub fn device(&self, index: u32) -> &DeviceId {
        &self.devices[index as usize]
    }

    /// The emitted metric name for `m` (built once per sampler).
    pub fn metric(m: SystemMetric) -> Metric {
        Metric::new(m.name()).unwrap_or(Metric::TEMPERATURE)
    }

    /// Intern `prefix` + sanitised `raw` as a device id.
    fn dev(&mut self, prefix: &str, raw: &str) -> u32 {
        sanitise(&mut self.key, prefix, raw);
        if let Some(i) = self.by_key.get(self.key.as_str()) {
            return *i;
        }
        // Sanitised ids are always valid; fall back to the prefix alone.
        let id = DeviceId::new(self.key.as_str())
            .or_else(|_| DeviceId::new(prefix.trim_end_matches(':')))
            .unwrap_or_else(|_| default_device());
        let i = u32::try_from(self.devices.len()).unwrap_or(u32::MAX);
        self.devices.push(id);
        self.by_key.insert(self.key.clone(), i);
        i
    }

    fn problem(&mut self, p: String) {
        if self.problem.is_none() {
            self.problem = Some(p);
        }
    }

    /// The per-second rate of a cumulative counter, or `None` on its first
    /// sample or after a reset.
    fn rate(&mut self, device: u32, m: SystemMetric, counter: f64, now: f64) -> Option<f64> {
        let (c0, t0) = self.prev.insert((device, m), (counter, now))?;
        let dt = now - t0;
        (dt > 0.0 && counter >= c0).then(|| (counter - c0) / dt)
    }

    /// Take one sample at monotonic time `now` (seconds), appending to
    /// `out`. Returns the first problem met (a file missing or not
    /// parseable); everything readable is still sampled.
    pub fn sample(&mut self, now: f64, out: &mut Vec<Sample>) -> Option<String> {
        self.problem = None;
        self.epoch = self.epoch.wrapping_add(1);
        let mut buf = std::mem::take(&mut self.buf);
        self.cpu(&mut buf, out);
        self.host(&mut buf, out);
        self.filesystems(&mut buf, out);
        self.disks(&mut buf, now, out);
        self.network(&mut buf, now, out);
        if self.o.temperatures {
            self.temperatures(&mut buf, out);
        }
        if self.o.top > 0 || !self.o.processes.is_empty() {
            self.processes(&mut buf, now, out);
        }
        for i in 0..self.units.len() {
            self.unit(&mut buf, i, now, out);
        }
        if self.o.hub {
            self.hub(&mut buf, now, out);
        }
        self.buf = buf;
        self.problem.take()
    }

    fn cpu(&mut self, buf: &mut Vec<u8>, out: &mut Vec<Sample>) {
        let text = match read_into(buf, &self.stat) {
            Ok(t) => t,
            Err(e) => return self.problem(format!("cannot read {}: {e}", self.stat.display())),
        };
        let mut found = false;
        parse::cpu_lines(&text, |core, t| {
            found |= core.is_none();
            if core.is_some() && !self.o.per_core {
                return;
            }
            let slot = core.map_or(0, |c| c as usize + 1);
            if slot > 4096 {
                return;
            }
            if self.prev_cpu.len() <= slot {
                self.prev_cpu.resize(slot + 1, None);
            }
            let Some(p) = self.prev_cpu[slot].replace(t) else {
                return;
            };
            let total = t.total().saturating_sub(p.total()) as f64;
            if total <= 0.0 {
                return;
            }
            let d = |a: u64, b: u64| a.saturating_sub(b) as f64;
            let device = match core {
                None => self.dev("cpu", ""),
                Some(c) => {
                    let mut n = std::mem::take(&mut self.path);
                    n.clear();
                    let _ = write!(n, "{c}");
                    let i = self.dev("cpu", &n);
                    self.path = n;
                    i
                }
            };
            let busy = pct(d(t.busy(), p.busy()), total);
            out.push(Sample {
                device,
                metric: SystemMetric::CpuUsage,
                value: busy,
            });
            if core.is_none() {
                for (m, a, b) in [
                    (SystemMetric::CpuUser, t.user, p.user),
                    (SystemMetric::CpuSystem, t.system, p.system),
                    (SystemMetric::CpuIowait, t.iowait, p.iowait),
                    (SystemMetric::CpuSteal, t.steal, p.steal),
                ] {
                    out.push(Sample {
                        device,
                        metric: m,
                        value: pct(d(a, b), total),
                    });
                }
            }
        });
        if !found {
            self.problem(format!("{}: no total cpu line", self.stat.display()));
        }
    }

    fn host(&mut self, buf: &mut Vec<u8>, out: &mut Vec<Sample>) {
        let host = self.dev("host", "");
        let push = |out: &mut Vec<Sample>, metric, value| {
            out.push(Sample {
                device: host,
                metric,
                value,
            })
        };
        match read_into(buf, &self.loadavg)
            .ok()
            .as_deref()
            .and_then(parse::loadavg)
        {
            Some((l, running, total)) => {
                push(out, SystemMetric::Load1, l[0]);
                push(out, SystemMetric::Load5, l[1]);
                push(out, SystemMetric::Load15, l[2]);
                push(out, SystemMetric::TasksRunning, running as f64);
                push(out, SystemMetric::TasksTotal, total as f64);
            }
            None => self.problem(format!("cannot read or parse {}", self.loadavg.display())),
        }
        if let Some(u) = read_into(buf, &self.uptime)
            .ok()
            .as_deref()
            .and_then(parse::uptime)
        {
            push(out, SystemMetric::Uptime, u);
        }
        match read_into(buf, &self.meminfo)
            .ok()
            .as_deref()
            .and_then(parse::meminfo)
        {
            Some(m) => {
                let used = m.total.saturating_sub(m.available) as f64;
                push(out, SystemMetric::MemTotal, m.total as f64);
                push(out, SystemMetric::MemAvailable, m.available as f64);
                push(out, SystemMetric::MemUsed, used);
                push(out, SystemMetric::MemUsage, pct(used, m.total as f64));
                push(out, SystemMetric::SwapTotal, m.swap_total as f64);
                push(
                    out,
                    SystemMetric::SwapUsed,
                    m.swap_total.saturating_sub(m.swap_free) as f64,
                );
            }
            None => self.problem(format!("cannot read or parse {}", self.meminfo.display())),
        }
        for (i, m) in [
            SystemMetric::PressureCpu,
            SystemMetric::PressureMemory,
            SystemMetric::PressureIo,
        ]
        .into_iter()
        .enumerate()
        {
            // Pressure stall information is optional (CONFIG_PSI).
            if let Some(v) = read_into(buf, &self.pressure[i])
                .ok()
                .as_deref()
                .and_then(parse::pressure)
            {
                push(out, m, v);
            }
        }
    }

    fn filesystems(&mut self, buf: &mut Vec<u8>, out: &mut Vec<Sample>) {
        let text = match read_into(buf, &self.mounts) {
            Ok(t) => t,
            Err(e) => return self.problem(format!("cannot read {}: {e}", self.mounts.display())),
        };
        let epoch = self.epoch;
        parse::mounts(&text, |dev, mnt, _fs| {
            let unescaped = parse::unescape_mount(mnt);
            let mnt = unescaped.as_deref().unwrap_or(mnt);
            if self.o.mounts.is_empty() {
                // Block-device filesystems only (no network filesystems,
                // whose statfs can hang); each device once.
                if !dev.starts_with("/dev/") {
                    return;
                }
                match self.fs_seen.get_mut(dev) {
                    Some(g) if *g == epoch => return,
                    Some(g) => *g = epoch,
                    None => {
                        self.fs_seen.insert(dev.to_owned(), epoch);
                    }
                }
            } else if !self.o.mounts.iter().any(|w| w == mnt) {
                return;
            }
            let mut path = std::mem::take(&mut self.path);
            path.clear();
            let root = self.o.root_path.to_string_lossy();
            path.push_str(root.trim_end_matches('/'));
            path.push('/');
            path.push_str(mnt.trim_start_matches('/'));
            let stat = statvfs(Path::new(&path));
            self.path = path;
            let Some((size, used, avail, inodes)) = stat.filter(|s| s.0 > 0.0) else {
                return;
            };
            let device = self.dev("fs:", mnt);
            out.push(Sample {
                device,
                metric: SystemMetric::FsSize,
                value: size,
            });
            out.push(Sample {
                device,
                metric: SystemMetric::FsUsed,
                value: used,
            });
            out.push(Sample {
                device,
                metric: SystemMetric::FsAvailable,
                value: avail,
            });
            out.push(Sample {
                device,
                metric: SystemMetric::FsUsage,
                value: pct(used, used + avail),
            });
            if let Some(i) = inodes {
                out.push(Sample {
                    device,
                    metric: SystemMetric::FsInodes,
                    value: i,
                });
            }
        });
    }

    fn disks(&mut self, buf: &mut Vec<u8>, now: f64, out: &mut Vec<Sample>) {
        let text = match read_into(buf, &self.diskstats) {
            Ok(t) => t,
            Err(e) => {
                return self.problem(format!("cannot read {}: {e}", self.diskstats.display()));
            }
        };
        parse::diskstats(&text, |name, c| {
            if name.starts_with("loop") || name.starts_with("ram") {
                return;
            }
            // Whole disks only: partitions have no /sys/block entry.
            let whole = match self.whole_disk.get(name) {
                Some(w) => *w,
                None => {
                    let w =
                        !name.contains('/') && self.o.sys_path.join("block").join(name).exists();
                    self.whole_disk.insert(name.to_owned(), w);
                    w
                }
            };
            if !whole {
                return;
            }
            let device = self.dev("disk:", name);
            if let Some(r) = self.rate(
                device,
                SystemMetric::DiskRead,
                c.sectors_read as f64 * 512.0,
                now,
            ) {
                out.push(Sample {
                    device,
                    metric: SystemMetric::DiskRead,
                    value: r,
                });
            }
            if let Some(w) = self.rate(
                device,
                SystemMetric::DiskWrite,
                c.sectors_written as f64 * 512.0,
                now,
            ) {
                out.push(Sample {
                    device,
                    metric: SystemMetric::DiskWrite,
                    value: w,
                });
            }
            if let Some(ms) = self.rate(device, SystemMetric::DiskBusy, c.io_ms as f64, now) {
                out.push(Sample {
                    device,
                    metric: SystemMetric::DiskBusy,
                    value: (ms / 10.0).clamp(0.0, 100.0),
                });
            }
        });
    }

    fn network(&mut self, buf: &mut Vec<u8>, now: f64, out: &mut Vec<Sample>) {
        let text = match read_into(buf, &self.net_dev) {
            Ok(t) => t,
            Err(e) => return self.problem(format!("cannot read {}: {e}", self.net_dev.display())),
        };
        parse::net_dev(&text, |name, c| {
            let wanted = if self.o.interfaces.is_empty() {
                name != "lo"
            } else {
                self.o.interfaces.iter().any(|i| i == name)
            };
            if !wanted {
                return;
            }
            let device = self.dev("net:", name);
            if let Some(r) = self.rate(device, SystemMetric::NetRx, c.rx_bytes as f64, now) {
                out.push(Sample {
                    device,
                    metric: SystemMetric::NetRx,
                    value: r,
                });
            }
            if let Some(t) = self.rate(device, SystemMetric::NetTx, c.tx_bytes as f64, now) {
                out.push(Sample {
                    device,
                    metric: SystemMetric::NetTx,
                    value: t,
                });
            }
            let errors = c.rx_errors.saturating_add(c.tx_errors) as f64;
            let drops = c.rx_dropped.saturating_add(c.tx_dropped) as f64;
            out.push(Sample {
                device,
                metric: SystemMetric::NetErrors,
                value: errors,
            });
            out.push(Sample {
                device,
                metric: SystemMetric::NetDrops,
                value: drops,
            });
        });
    }

    /// Find temperature inputs (every 60 samples, as sensors rarely change).
    fn discover_temperatures(&mut self, buf: &mut Vec<u8>) {
        self.temps.clear();
        let hwmon = self.o.sys_path.join("class/hwmon");
        let mut chips: Vec<PathBuf> = std::fs::read_dir(&hwmon)
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        chips.sort();
        for chip in chips {
            let name = read_into(buf, &chip.join("name"))
                .map(|t| t.trim().to_owned())
                .unwrap_or_else(|_| {
                    chip.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                });
            let mut inputs: Vec<PathBuf> = std::fs::read_dir(&chip)
                .map(|d| {
                    d.flatten()
                        .map(|e| e.path())
                        .filter(|p| {
                            p.file_name()
                                .and_then(|n| n.to_str())
                                .is_some_and(|n| n.starts_with("temp") && n.ends_with("_input"))
                        })
                        .collect()
                })
                .unwrap_or_default();
            inputs.sort();
            for input in inputs {
                let file = input.file_name().and_then(|n| n.to_str()).unwrap_or("temp");
                let base = file.trim_end_matches("_input").to_owned();
                let label = read_into(buf, &chip.join(format!("{base}_label")))
                    .map(|t| t.trim().to_owned())
                    .ok()
                    .filter(|l| !l.is_empty())
                    .unwrap_or(base);
                let device = self.dev("temp:", &format!("{name}.{label}"));
                self.temps.push(Temp { device, input });
            }
        }
        if !self.temps.is_empty() {
            return;
        }
        let thermal = self.o.sys_path.join("class/thermal");
        let mut zones: Vec<PathBuf> = std::fs::read_dir(&thermal)
            .map(|d| {
                d.flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("thermal_zone"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        zones.sort();
        for zone in zones {
            let kind = read_into(buf, &zone.join("type"))
                .map(|t| t.trim().to_owned())
                .unwrap_or_else(|_| "zone".to_owned());
            let n = zone
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .trim_start_matches("thermal_zone")
                .to_owned();
            let device = self.dev("temp:", &format!("{kind}.{n}"));
            self.temps.push(Temp {
                device,
                input: zone.join("temp"),
            });
        }
    }

    fn temperatures(&mut self, buf: &mut Vec<u8>, out: &mut Vec<Sample>) {
        if self.temps_age >= 60 {
            self.discover_temperatures(buf);
            self.temps_age = 0;
        }
        self.temps_age += 1;
        for t in &self.temps {
            let Some(milli) = read_into(buf, &t.input)
                .ok()
                .as_deref()
                .and_then(parse::single_i64)
            else {
                continue;
            };
            let c = milli as f64 / 1000.0;
            if (-100.0..=250.0).contains(&c) {
                out.push(Sample {
                    device: t.device,
                    metric: SystemMetric::Temperature,
                    value: c,
                });
            }
        }
    }

    fn processes(&mut self, buf: &mut Vec<u8>, now: f64, out: &mut Vec<Sample>) {
        let Ok(dir) = std::fs::read_dir(&self.o.proc_path) else {
            return;
        };
        let epoch = self.epoch;
        let dt = self.last_scan.map(|t| now - t).filter(|d| *d > 0.0);
        self.last_scan = Some(now);
        for a in self.comms.values_mut() {
            *a = Agg {
                device: a.device,
                named: a.named,
                epoch: a.epoch,
                ..Agg::default()
            };
        }
        let proc = self.o.proc_path.to_string_lossy().into_owned();
        for entry in dir.flatten() {
            let name = entry.file_name();
            let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let mut path = std::mem::take(&mut self.path);
            path.clear();
            let _ = write!(path, "{proc}/{pid}/stat");
            let read = read_into(buf, Path::new(&path));
            self.path = path;
            let Ok(text) = read else { continue };
            let Some(st) = parse::pid_stat(&text) else {
                continue;
            };
            let prev = self.pids.insert(
                pid,
                PidPrev {
                    ticks: st.cpu_ticks,
                    epoch,
                },
            );
            let cpu = match (prev, dt) {
                (Some(p), Some(dt)) if st.cpu_ticks >= p.ticks => {
                    (st.cpu_ticks - p.ticks) as f64 / self.ticks_per_second / dt * 100.0
                }
                _ => 0.0,
            };
            let named = self.o.processes.iter().any(|n| n == st.comm);
            let a = match self.comms.get_mut(st.comm) {
                Some(a) => a,
                None => self.comms.entry(st.comm.to_owned()).or_default(),
            };
            a.cpu += cpu;
            a.rss += st.rss_pages as f64 * self.page_size;
            a.threads += st.threads as f64;
            a.count += 1.0;
            a.named = named;
            a.epoch = epoch;
            if named {
                let mut path = std::mem::take(&mut self.path);
                path.clear();
                let _ = write!(path, "{proc}/{pid}/fd");
                a.fds += count_entries(Path::new(&path)).unwrap_or(0.0);
                path.clear();
                let _ = write!(path, "{proc}/{pid}/io");
                if let Some((r, w)) = read_into(buf, Path::new(&path))
                    .ok()
                    .as_deref()
                    .and_then(parse::pid_io)
                {
                    a.read += r as f64;
                    a.write += w as f64;
                    a.io = true;
                }
                self.path = path;
            }
        }
        self.pids.retain(|_, p| p.epoch == epoch);
        self.comms.retain(|_, a| a.epoch == epoch);
        let mut comms = std::mem::take(&mut self.comms);
        self.ranked.clear();
        for (name, a) in comms.iter_mut() {
            let device = match a.device {
                Some(d) => d,
                None => {
                    let d = self.dev("proc:", name);
                    a.device = Some(d);
                    d
                }
            };
            self.ranked.push(Ranked {
                device,
                cpu: a.cpu,
                rss: a.rss,
                threads: a.threads,
                count: a.count,
                fds: a.fds,
                read: a.read,
                write: a.write,
                io: a.io,
                named: a.named,
            });
        }
        self.comms = comms;
        self.selected.clear();
        let top = self.o.top.min(self.ranked.len());
        if top > 0 {
            // By CPU only among processes that used some (none on the first
            // scan, which has no rates yet).
            self.ranked
                .sort_unstable_by(|a, b| b.cpu.total_cmp(&a.cpu).then(a.device.cmp(&b.device)));
            self.selected.extend(
                self.ranked[..top]
                    .iter()
                    .filter(|r| r.cpu > 0.0)
                    .map(|r| r.device),
            );
            self.ranked
                .sort_unstable_by(|a, b| b.rss.total_cmp(&a.rss).then(a.device.cmp(&b.device)));
            for r in &self.ranked[..top] {
                if !self.selected.contains(&r.device) {
                    self.selected.push(r.device);
                }
            }
        }
        for i in 0..self.ranked.len() {
            let r = self.ranked[i];
            if !(r.named || self.selected.contains(&r.device)) {
                continue;
            }
            let d = r.device;
            out.push(Sample {
                device: d,
                metric: SystemMetric::ProcCpu,
                value: r.cpu,
            });
            out.push(Sample {
                device: d,
                metric: SystemMetric::ProcRss,
                value: r.rss,
            });
            out.push(Sample {
                device: d,
                metric: SystemMetric::ProcThreads,
                value: r.threads,
            });
            out.push(Sample {
                device: d,
                metric: SystemMetric::ProcCount,
                value: r.count,
            });
            if r.named {
                out.push(Sample {
                    device: d,
                    metric: SystemMetric::ProcFds,
                    value: r.fds,
                });
                if r.io {
                    if let Some(v) = self.rate(d, SystemMetric::ProcRead, r.read, now) {
                        out.push(Sample {
                            device: d,
                            metric: SystemMetric::ProcRead,
                            value: v,
                        });
                    }
                    if let Some(v) = self.rate(d, SystemMetric::ProcWrite, r.write, now) {
                        out.push(Sample {
                            device: d,
                            metric: SystemMetric::ProcWrite,
                            value: v,
                        });
                    }
                }
            }
        }
    }

    fn unit(&mut self, buf: &mut Vec<u8>, i: usize, now: f64, out: &mut Vec<Sample>) {
        let dir = self.units[i].1.clone();
        let name = self.units[i].0.clone();
        if !dir.is_dir() {
            return; // not running, or not cgroup v2
        }
        let device = self.dev("unit:", &name);
        if let Some(usec) = read_into(buf, &dir.join("cpu.stat"))
            .ok()
            .as_deref()
            .and_then(|t| parse::keyed_u64(t, "usage_usec"))
            && let Some(r) = self.rate(device, SystemMetric::UnitCpu, usec as f64, now)
        {
            // Microseconds of CPU per second, as % of one core.
            out.push(Sample {
                device,
                metric: SystemMetric::UnitCpu,
                value: r / 1e4,
            });
        }
        if let Some(m) = read_into(buf, &dir.join("memory.current"))
            .ok()
            .as_deref()
            .and_then(parse::single_i64)
        {
            out.push(Sample {
                device,
                metric: SystemMetric::UnitMemory,
                value: m.max(0) as f64,
            });
        }
        if let Some(n) = read_into(buf, &dir.join("pids.current"))
            .ok()
            .as_deref()
            .and_then(parse::single_i64)
        {
            out.push(Sample {
                device,
                metric: SystemMetric::UnitTasks,
                value: n.max(0) as f64,
            });
        }
        if let Ok(t) = read_into(buf, &dir.join("io.stat")) {
            let (r, w) = parse::cgroup_io(&t);
            if let Some(v) = self.rate(device, SystemMetric::UnitRead, r as f64, now) {
                out.push(Sample {
                    device,
                    metric: SystemMetric::UnitRead,
                    value: v,
                });
            }
            if let Some(v) = self.rate(device, SystemMetric::UnitWrite, w as f64, now) {
                out.push(Sample {
                    device,
                    metric: SystemMetric::UnitWrite,
                    value: v,
                });
            }
        }
    }

    fn hub(&mut self, buf: &mut Vec<u8>, now: f64, out: &mut Vec<Sample>) {
        let device = self.dev("hub", "");
        let me = self.o.proc_path.join("self");
        let Some((ticks, threads, rss)) = read_into(buf, &me.join("stat"))
            .ok()
            .as_deref()
            .and_then(parse::pid_stat)
            .map(|s| (s.cpu_ticks, s.threads, s.rss_pages))
        else {
            return;
        };
        if let Some(r) = self.rate(device, SystemMetric::ProcCpu, ticks as f64, now) {
            out.push(Sample {
                device,
                metric: SystemMetric::ProcCpu,
                value: r / self.ticks_per_second * 100.0,
            });
        }
        out.push(Sample {
            device,
            metric: SystemMetric::ProcRss,
            value: rss as f64 * self.page_size,
        });
        out.push(Sample {
            device,
            metric: SystemMetric::ProcThreads,
            value: threads as f64,
        });
        if let Some(n) = count_entries(&me.join("fd")) {
            out.push(Sample {
                device,
                metric: SystemMetric::ProcFds,
                value: n,
            });
        }
        if let Some((r, w)) = read_into(buf, &me.join("io"))
            .ok()
            .as_deref()
            .and_then(parse::pid_io)
        {
            if let Some(v) = self.rate(device, SystemMetric::ProcRead, r as f64, now) {
                out.push(Sample {
                    device,
                    metric: SystemMetric::ProcRead,
                    value: v,
                });
            }
            if let Some(v) = self.rate(device, SystemMetric::ProcWrite, w as f64, now) {
                out.push(Sample {
                    device,
                    metric: SystemMetric::ProcWrite,
                    value: v,
                });
            }
        }
    }
}

fn default_device() -> DeviceId {
    DeviceId::new("host").unwrap_or_else(|_| unreachable!("a valid literal id"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitising_device_ids() {
        let mut s = String::new();
        sanitise(&mut s, "fs:", "/");
        assert_eq!(s, "fs:root");
        sanitise(&mut s, "fs:", "/var/lib/my disk");
        assert_eq!(s, "fs:var.lib.my_disk");
        sanitise(&mut s, "unit:", "getty@tty1");
        assert_eq!(s, "unit:getty_tty1");
        sanitise(&mut s, "proc:", &"x".repeat(200));
        assert_eq!(s.len(), heimr_core::id::MAX_ID_LEN);
        assert!(DeviceId::new(s.as_str()).is_ok());
    }

    #[test]
    fn metric_names_are_valid() {
        for m in SystemMetric::ALL {
            assert!(Metric::new(m.name()).is_ok(), "{}", m.name());
        }
    }
}
