//! Parsers for the `/proc` and `/sys` files the adapter reads.
//!
//! All parsers borrow from the text they are given and never allocate; they
//! return `None` (or skip a line) on anything unexpected and never panic,
//! whatever the input (property-tested).

/// Fields of a `cpu` line of `/proc/stat`, in clock ticks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    /// User time (including nice).
    pub user: u64,
    /// System time (including IRQ and soft IRQ).
    pub system: u64,
    /// Idle time.
    pub idle: u64,
    /// Waiting for I/O.
    pub iowait: u64,
    /// Stolen by the hypervisor.
    pub steal: u64,
}

impl CpuTimes {
    /// All ticks (guest time is already part of user time).
    pub fn total(&self) -> u64 {
        self.user
            .saturating_add(self.system)
            .saturating_add(self.idle)
            .saturating_add(self.iowait)
            .saturating_add(self.steal)
    }

    /// Ticks not idle and not waiting for I/O.
    pub fn busy(&self) -> u64 {
        self.user
            .saturating_add(self.system)
            .saturating_add(self.steal)
    }
}

fn nums<const N: usize>(fields: &mut std::str::SplitAsciiWhitespace<'_>) -> Option<[u64; N]> {
    let mut out = [0u64; N];
    for v in &mut out {
        *v = fields.next()?.parse().ok()?;
    }
    Some(out)
}

/// Call `f` for each `cpu` line of `/proc/stat`: `None` for the total,
/// `Some(n)` for core `n`. Lines that do not parse are skipped.
pub fn cpu_lines(text: &str, mut f: impl FnMut(Option<u32>, CpuTimes)) {
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("cpu") else {
            continue;
        };
        let mut fields = rest.split_ascii_whitespace();
        let core = if rest.starts_with(' ') {
            None
        } else {
            match fields.next().and_then(|n| n.parse::<u32>().ok()) {
                Some(n) => Some(n),
                None => continue,
            }
        };
        // user nice system idle iowait irq softirq steal
        let Some([user, nice, system, idle, iowait, irq, softirq]) = nums::<7>(&mut fields) else {
            continue;
        };
        let steal = fields.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        f(
            core,
            CpuTimes {
                user: user.saturating_add(nice),
                system: system.saturating_add(irq).saturating_add(softirq),
                idle,
                iowait,
                steal,
            },
        );
    }
}

/// `/proc/loadavg`: the three load averages, running and total tasks.
pub fn loadavg(text: &str) -> Option<([f64; 3], u64, u64)> {
    let mut f = text.split_ascii_whitespace();
    let mut load = [0.0; 3];
    for v in &mut load {
        *v = f.next()?.parse().ok().filter(|x: &f64| x.is_finite())?;
    }
    let (running, total) = f.next()?.split_once('/')?;
    Some((load, running.parse().ok()?, total.parse().ok()?))
}

/// `/proc/uptime`: seconds since boot.
pub fn uptime(text: &str) -> Option<f64> {
    text.split_ascii_whitespace()
        .next()?
        .parse()
        .ok()
        .filter(|x: &f64| x.is_finite() && *x >= 0.0)
}

/// What the adapter uses from `/proc/meminfo`, in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemInfo {
    /// `MemTotal`.
    pub total: u64,
    /// `MemAvailable` (falls back to free + buffers + cached on old kernels).
    pub available: u64,
    /// `SwapTotal`.
    pub swap_total: u64,
    /// `SwapFree`.
    pub swap_free: u64,
}

/// `/proc/meminfo`; `None` without `MemTotal`.
pub fn meminfo(text: &str) -> Option<MemInfo> {
    let (mut total, mut avail, mut free, mut buffers, mut cached) = (None, None, 0, 0, 0);
    let (mut swap_total, mut swap_free) = (0, 0);
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let mut f = rest.split_ascii_whitespace();
        let Some(v) = f.next().and_then(|v| v.parse::<u64>().ok()) else {
            continue;
        };
        let bytes = match f.next() {
            Some("kB") => v.saturating_mul(1024),
            None => v,
            Some(_) => continue,
        };
        match key {
            "MemTotal" => total = Some(bytes),
            "MemAvailable" => avail = Some(bytes),
            "MemFree" => free = bytes,
            "Buffers" => buffers = bytes,
            "Cached" => cached = bytes,
            "SwapTotal" => swap_total = bytes,
            "SwapFree" => swap_free = bytes,
            _ => {}
        }
    }
    let total = total?;
    let available = avail
        .unwrap_or_else(|| free.saturating_add(buffers).saturating_add(cached))
        .min(total);
    Some(MemInfo {
        total,
        available,
        swap_total,
        swap_free: swap_free.min(swap_total),
    })
}

/// `/proc/pressure/{cpu,memory,io}`: the `some avg10` percentage.
pub fn pressure(text: &str) -> Option<f64> {
    let line = text.lines().find(|l| l.starts_with("some "))?;
    line.split_ascii_whitespace()
        .find_map(|f| f.strip_prefix("avg10="))?
        .parse()
        .ok()
        .filter(|x: &f64| x.is_finite() && *x >= 0.0)
}

/// Counters of one interface in `/proc/net/dev`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetCounters {
    /// Bytes received.
    pub rx_bytes: u64,
    /// Receive errors.
    pub rx_errors: u64,
    /// Received packets dropped.
    pub rx_dropped: u64,
    /// Bytes sent.
    pub tx_bytes: u64,
    /// Send errors.
    pub tx_errors: u64,
    /// Sent packets dropped.
    pub tx_dropped: u64,
}

/// Call `f` for each interface in `/proc/net/dev`.
pub fn net_dev(text: &str, mut f: impl FnMut(&str, NetCounters)) {
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || name.contains('|') {
            continue;
        }
        let mut fields = rest.split_ascii_whitespace();
        let Some([rx_bytes, _rx_packets, rx_errors, rx_dropped, _, _, _, _]) =
            nums::<8>(&mut fields)
        else {
            continue;
        };
        let Some([tx_bytes, _tx_packets, tx_errors, tx_dropped]) = nums::<4>(&mut fields) else {
            continue;
        };
        f(
            name,
            NetCounters {
                rx_bytes,
                rx_errors,
                rx_dropped,
                tx_bytes,
                tx_errors,
                tx_dropped,
            },
        );
    }
}

/// Counters of one block device in `/proc/diskstats`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskCounters {
    /// 512-byte sectors read.
    pub sectors_read: u64,
    /// 512-byte sectors written.
    pub sectors_written: u64,
    /// Milliseconds spent doing I/O.
    pub io_ms: u64,
}

/// Call `f` for each device in `/proc/diskstats`.
pub fn diskstats(text: &str, mut f: impl FnMut(&str, DiskCounters)) {
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        let (Some(_major), Some(_minor), Some(name)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        // reads merged sectors ms writes merged sectors ms in_flight io_ms …
        let Some(
            [
                _r,
                _rm,
                sectors_read,
                _rms,
                _w,
                _wm,
                sectors_written,
                _wms,
                _inflight,
                io_ms,
            ],
        ) = nums::<10>(&mut fields)
        else {
            continue;
        };
        f(
            name,
            DiskCounters {
                sectors_read,
                sectors_written,
                io_ms,
            },
        );
    }
}

/// Call `f(device, mount point, filesystem type)` for each line of
/// `/proc/mounts`. Mount points with octal escapes (`\040` for a space) are
/// passed escaped; callers that need the real path unescape them.
pub fn mounts(text: &str, mut f: impl FnMut(&str, &str, &str)) {
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        if let (Some(dev), Some(mnt), Some(fs)) = (fields.next(), fields.next(), fields.next()) {
            f(dev, mnt, fs);
        }
    }
}

/// Undo the octal escapes of `/proc/mounts` (`\040` space, `\011` tab,
/// `\012` newline, `\134` backslash). Returns `None` if there are none.
pub fn unescape_mount(s: &str) -> Option<String> {
    if !s.contains('\\') {
        return None;
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = (b[i + 1] - b'0') as u32 * 64
                + (b[i + 2] - b'0') as u32 * 8
                + (b[i + 3] - b'0') as u32;
            out.push(u8::try_from(v).unwrap_or(b'?'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// What the adapter uses from `/proc/<pid>/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PidStat<'a> {
    /// Command name (up to 15 bytes; may contain spaces and parentheses).
    pub comm: &'a str,
    /// User plus system time, in clock ticks.
    pub cpu_ticks: u64,
    /// Threads.
    pub threads: u64,
    /// Resident set size, in pages.
    pub rss_pages: u64,
}

/// `/proc/<pid>/stat`.
pub fn pid_stat(text: &str) -> Option<PidStat<'_>> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = text.get(open + 1..close)?;
    let rest = text.get(close + 1..)?;
    // After the comm: state(3) ppid pgrp session tty tpgid flags minflt
    // cminflt majflt cmajflt utime(14) stime(15) cutime cstime priority nice
    // num_threads(20) itrealvalue starttime vsize rss(24)
    let mut f = rest.split_ascii_whitespace();
    let _state = f.next()?;
    let mut field = 3;
    let (mut utime, mut stime, mut threads) = (0u64, 0u64, 0u64);
    for v in f.by_ref() {
        field += 1;
        match field {
            14 => utime = v.parse().ok()?,
            15 => stime = v.parse().ok()?,
            20 => threads = v.parse().ok()?,
            24 => {
                // rss is signed in the kernel's format; negative means 0.
                let rss = v.parse::<i64>().ok()?.max(0).unsigned_abs();
                return Some(PidStat {
                    comm,
                    cpu_ticks: utime.saturating_add(stime),
                    threads,
                    rss_pages: rss,
                });
            }
            _ => {}
        }
    }
    None
}

/// `/proc/<pid>/io`: bytes read from and written to storage.
pub fn pid_io(text: &str) -> Option<(u64, u64)> {
    let (mut read, mut write) = (None, None);
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("read_bytes:") {
            read = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("write_bytes:") {
            write = v.trim().parse().ok();
        }
    }
    Some((read?, write?))
}

/// A `key value` line of a cgroup file such as `cpu.stat`.
pub fn keyed_u64(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let mut f = l.split_ascii_whitespace();
        (f.next()? == key).then(|| f.next()?.parse().ok())?
    })
}

/// A file holding one number (`memory.current`, `pids.current`, a hwmon
/// `temp*_input`).
pub fn single_i64(text: &str) -> Option<i64> {
    text.trim().parse().ok()
}

/// cgroup v2 `io.stat`: `rbytes` and `wbytes` summed over devices.
pub fn cgroup_io(text: &str) -> (u64, u64) {
    let (mut r, mut w) = (0u64, 0u64);
    for f in text.split_ascii_whitespace() {
        if let Some(v) = f
            .strip_prefix("rbytes=")
            .and_then(|v| v.parse::<u64>().ok())
        {
            r = r.saturating_add(v);
        } else if let Some(v) = f
            .strip_prefix("wbytes=")
            .and_then(|v| v.parse::<u64>().ok())
        {
            w = w.saturating_add(v);
        }
    }
    (r, w)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn stat_lines() {
        let text = "cpu  10 2 3 100 5 1 1 4 0 0\ncpu0 5 1 1 50 2 0 0 2 0 0\ncpu1 x\nintr 1 2 3\n";
        let mut seen = vec![];
        cpu_lines(text, |c, t| seen.push((c, t)));
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].0, None);
        assert_eq!(
            seen[0].1,
            CpuTimes {
                user: 12,
                system: 5,
                idle: 100,
                iowait: 5,
                steal: 4
            }
        );
        assert_eq!(seen[1].0, Some(0));
        assert_eq!(seen[0].1.total(), 126);
        assert_eq!(seen[0].1.busy(), 21);
    }

    #[test]
    fn pid_stat_with_awkward_comm() {
        let t = "42 (a (b) c) S 1 42 42 0 -1 4194560 100 0 0 0 7 3 0 0 20 0 4 0 1000 123456 250 18446744073709551615 0";
        let s = pid_stat(t).unwrap();
        assert_eq!(s.comm, "a (b) c");
        assert_eq!((s.cpu_ticks, s.threads, s.rss_pages), (10, 4, 250));
        assert!(pid_stat("42 (x) S 1 2").is_none());
    }

    #[test]
    fn small_files() {
        assert_eq!(
            loadavg("0.50 0.25 0.10 2/300 999\n"),
            Some(([0.5, 0.25, 0.1], 2, 300))
        );
        assert_eq!(uptime("123.45 678.9\n"), Some(123.45));
        assert_eq!(
            pressure("some avg10=1.50 avg60=0.00 avg300=0.00 total=1\nfull avg10=0.00"),
            Some(1.5)
        );
        assert_eq!(
            pid_io("rchar: 1\nread_bytes: 4096\nwrite_bytes: 8192\n"),
            Some((4096, 8192))
        );
        assert_eq!(
            keyed_u64("usage_usec 500\nuser_usec 300\n", "usage_usec"),
            Some(500)
        );
        assert_eq!(
            cgroup_io("8:0 rbytes=10 wbytes=20 rios=1\n8:16 rbytes=5 wbytes=1\n"),
            (15, 21)
        );
        assert_eq!(
            unescape_mount("/mnt/my\\040disk").as_deref(),
            Some("/mnt/my disk")
        );
        assert_eq!(unescape_mount("/plain"), None);
    }

    #[test]
    fn meminfo_falls_back_without_available() {
        let m = meminfo("MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 50 kB\nCached: 150 kB\n")
            .unwrap();
        assert_eq!(m.available, 300 * 1024);
        assert!(meminfo("MemFree: 1 kB\n").is_none());
    }

    proptest! {
        #[test]
        fn parsers_never_panic(s in "\\PC{0,400}") {
            cpu_lines(&s, |_, _| {});
            let _ = loadavg(&s);
            let _ = uptime(&s);
            let _ = meminfo(&s);
            let _ = pressure(&s);
            net_dev(&s, |_, _| {});
            diskstats(&s, |_, _| {});
            mounts(&s, |_, _, _| {});
            let _ = unescape_mount(&s);
            let _ = pid_stat(&s);
            let _ = pid_io(&s);
            let _ = keyed_u64(&s, "usage_usec");
            let _ = single_i64(&s);
            let _ = cgroup_io(&s);
        }

        #[test]
        fn structured_garbage_never_panics(
            words in proptest::collection::vec(prop_oneof![
                Just("cpu".to_owned()), Just("cpu0".to_owned()), Just("(".to_owned()), Just(")".to_owned()),
                Just(":".to_owned()), Just("\\".to_owned()), Just("\\0".to_owned()), Just("kB".to_owned()),
                Just("MemTotal:".to_owned()), Just("some".to_owned()), Just("avg10=".to_owned()),
                Just("18446744073709551615".to_owned()), Just("-1".to_owned()), Just("NaN".to_owned()),
                Just("\n".to_owned()), "[0-9]{1,25}",
            ], 0..80)
        ) {
            let s = words.join(" ");
            cpu_lines(&s, |_, t| { let _ = (t.total(), t.busy()); });
            let _ = meminfo(&s);
            let _ = pid_stat(&s);
            let _ = unescape_mount(&s);
            net_dev(&s, |_, _| {});
            diskstats(&s, |_, _| {});
            let _ = loadavg(&s);
            let _ = pressure(&s);
        }
    }
}
