//! The sampler against the fixture host: what is read, what is filtered
//! out, and exact rates between two samples.

#![allow(clippy::unwrap_used, clippy::float_cmp)]

mod common;

use std::collections::BTreeMap;

use heimr_system::{Options, Sample, Sampler, SystemMetric};

fn options(h: &common::Host) -> Options {
    let mut o = Options::new(h.path("proc"), h.path("sys"));
    o.cgroup_path = h.path("cgroup");
    o.root_path = h.path("root");
    o.processes = vec!["exampled".into()];
    o.units = vec!["example.service".into()];
    o.top = 1;
    o
}

fn table(s: &Sampler, out: &[Sample]) -> BTreeMap<(String, &'static str), f64> {
    out.iter()
        .map(|x| ((s.device(x.device).to_string(), x.metric.name()), x.value))
        .collect()
}

fn bump(h: &common::Host, rel: &str, from: &str, to: &str) {
    let p = h.path(rel);
    let text = std::fs::read_to_string(&p).unwrap();
    assert!(text.contains(from), "{rel} lacks {from:?}");
    std::fs::write(p, text.replacen(from, to, 1)).unwrap();
}

#[test]
fn first_sample_reads_gauges_and_filters_devices() {
    let h = common::Host::new();
    let mut s = Sampler::new(options(&h));
    let mut out = Vec::new();
    assert_eq!(s.sample(0.0, &mut out), None);
    let t = table(&s, &out);
    let devices: std::collections::BTreeSet<_> = t.keys().map(|(d, _)| d.as_str()).collect();
    // Filesystem usage needs statvfs (unix); elsewhere it is left out.
    let filesystems: &[&str] = if cfg!(unix) {
        &["fs:boot", "fs:mnt.data_disk", "fs:root"]
    } else {
        &[]
    };
    let mut expected = filesystems.to_vec();
    expected.extend([
        "host",
        "hub",
        "net:eth0",
        "net:wlan0",
        "proc:exampled",
        "temp:cpu_thermal.CPU",
        "temp:cpu_thermal.temp2",
        "temp:drivetemp.temp1",
        "unit:example.service",
    ]);
    assert_eq!(
        devices.into_iter().collect::<Vec<_>>(),
        expected,
        "no rates yet (no cpu, disks or CPU ranking); bind, nfs and pseudo filesystems and lo left out"
    );
    let g = |d: &str, m: &str| t[&(d.to_owned(), m)];
    assert_eq!(g("host", "load.1"), 0.42);
    assert_eq!(g("host", "tasks.total"), 187.0);
    assert_eq!(g("host", "uptime"), 86400.5);
    assert_eq!(g("host", "memory.total"), 8_000_000.0 * 1024.0);
    assert_eq!(g("host", "memory.used"), 3_000_000.0 * 1024.0);
    assert_eq!(g("host", "memory.usage"), 37.5);
    assert_eq!(g("host", "swap.used"), 100_000.0 * 1024.0);
    assert_eq!(g("host", "pressure.io"), 1.25);
    assert_eq!(g("net:eth0", "net.errors"), 2.0);
    assert_eq!(g("net:eth0", "net.drops"), 6.0);
    assert_eq!(g("temp:cpu_thermal.CPU", "temperature"), 45.0);
    assert_eq!(g("temp:drivetemp.temp1", "temperature"), 38.85);
    assert_eq!(g("proc:exampled", "proc.count"), 2.0);
    assert_eq!(g("proc:exampled", "proc.threads"), 12.0);
    assert_eq!(g("proc:exampled", "proc.fds"), 4.0);
    assert_eq!(g("unit:example.service", "unit.memory"), 73_400_320.0);
    assert_eq!(g("unit:example.service", "unit.tasks"), 9.0);
    assert_eq!(g("hub", "proc.threads"), 12.0);
    assert_eq!(g("hub", "proc.fds"), 4.0);
    if cfg!(unix) {
        assert!(g("fs:root", "fs.size") > 0.0);
    } else {
        assert!(!t.keys().any(|(d, _)| d.starts_with("fs:")));
    }
}

#[test]
fn second_sample_gives_exact_rates() {
    let h = common::Host::new();
    let mut s = Sampler::new(options(&h));
    let mut out = Vec::new();
    s.sample(0.0, &mut out);
    // 10 s later: the total CPU did 400 more user ticks and 600 more idle
    // ticks (40 % busy); cpu0 was fully busy.
    bump(
        &h,
        "proc/stat",
        "cpu  120000 3000 40000 900000",
        "cpu  120400 3000 40000 900600",
    );
    bump(
        &h,
        "proc/stat",
        "cpu0 30000 750 10000 225000",
        "cpu0 31000 750 10000 225000",
    );
    // sda read 20 MiB (40960 sectors) and was busy 5 s.
    bump(
        &h,
        "proc/diskstats",
        "sda 10000 500 2000000 3000 20000 1000 4000000 9000 0 12000",
        "sda 10000 500 2040960 3000 20000 1000 4000000 9000 0 17000",
    );
    bump(&h, "proc/net/dev", "eth0: 90000000", "eth0: 91000000");
    bump(
        &h,
        "cgroup/system.slice/example.service/cpu.stat",
        "usage_usec 5000000",
        "usage_usec 7500000",
    );
    bump(
        &h,
        "proc/101/stat",
        "4194560 1000 0 0 0 3000 1000",
        "4194560 1000 0 0 0 3500 1000",
    );
    bump(
        &h,
        "proc/self/io",
        "write_bytes: 81920",
        "write_bytes: 122880",
    );
    out.clear();
    assert_eq!(s.sample(10.0, &mut out), None);
    let t = table(&s, &out);
    let g = |d: &str, m: &str| {
        *t.get(&(d.to_owned(), m))
            .unwrap_or_else(|| panic!("{d} {m}"))
    };
    assert_eq!(g("cpu", "cpu.usage"), 40.0);
    assert_eq!(g("cpu", "cpu.user"), 40.0);
    assert_eq!(g("cpu", "cpu.iowait"), 0.0);
    assert_eq!(g("cpu0", "cpu.usage"), 100.0);
    assert!(
        !t.contains_key(&("cpu1".to_owned(), "cpu.usage")),
        "no ticks at all: no reading, never NaN"
    );
    assert_eq!(g("disk:sda", "disk.read"), 40960.0 * 512.0 / 10.0);
    assert_eq!(g("disk:sda", "disk.busy"), 50.0);
    assert_eq!(g("net:eth0", "net.rx"), 100_000.0);
    assert_eq!(g("net:eth0", "net.tx"), 0.0);
    assert_eq!(g("unit:example.service", "unit.cpu"), 25.0);
    // 500 ticks over 10 s at USER_HZ (100 on Linux): half a core.
    assert_eq!(g("proc:exampled", "proc.cpu"), 50.0);
    assert_eq!(g("hub", "proc.write"), 4096.0);
    // Top 1 by CPU is now the busiest name; disks are whole disks only.
    assert!(t.contains_key(&("proc:exampled".to_owned(), "proc.cpu")));
    assert!(!t.keys().any(|(d, _)| d == "disk:sda1" || d == "disk:loop0"));
    assert!(t.contains_key(&("disk:sdb".to_owned(), "disk.read")));
    assert!(out.iter().all(|x| x.value.is_finite()));
    assert!(
        out.iter()
            .all(|x| x.metric != SystemMetric::ProcRead || x.value >= 0.0)
    );
}

#[test]
fn a_missing_proc_is_reported_not_fatal() {
    let h = common::Host::new();
    std::fs::remove_dir_all(h.path("proc")).unwrap();
    let mut s = Sampler::new(options(&h));
    let mut out = Vec::new();
    let problem = s.sample(0.0, &mut out).unwrap();
    assert!(problem.contains("stat"), "{problem}");
}

#[test]
fn explicit_mounts_and_interfaces() {
    let h = common::Host::new();
    let mut o = options(&h);
    o.mounts = vec!["/srv/bind".into()];
    o.interfaces = vec!["lo".into()];
    o.top = 0;
    o.processes.clear();
    o.temperatures = false;
    o.per_core = false;
    let mut s = Sampler::new(o);
    let mut out = Vec::new();
    s.sample(0.0, &mut out);
    s.sample(10.0, &mut out);
    let t = table(&s, &out);
    let devices: std::collections::BTreeSet<_> = t.keys().map(|(d, _)| d.as_str()).collect();
    assert!(devices.contains("net:lo"));
    assert_eq!(devices.contains("fs:srv.bind"), cfg!(unix));
    assert!(
        !devices.contains("fs:root") && !devices.contains("net:eth0") && !devices.contains("cpu0")
    );
    assert!(
        !devices
            .iter()
            .any(|d| d.starts_with("proc:") || d.starts_with("temp:"))
    );
}
