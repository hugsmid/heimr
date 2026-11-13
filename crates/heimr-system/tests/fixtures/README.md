# Fixtures (hand-built)

**These are not recordings.** `host/` is a synthetic host written by hand in the documented formats of the Linux files the adapter reads (see `proc(5)`, the kernel's `Documentation/admin-guide/iostats.rst`, `cgroup-v2.rst`, `sysfs-class-hwmon` and PSI docs). No file was captured from any machine; the names (`exampled`, `example.service`, `sda`, `eth0`, `cpu_thermal`) and numbers are made up.

| Path | Content |
|---|---|
| `host/proc/{stat,loadavg,uptime,meminfo}` | a four-core host, 8 GB memory, 2 GB swap |
| `host/proc/pressure/*` | PSI `some`/`full` lines |
| `host/proc/mounts` | block-device, pseudo, bind, escaped (`\040`) and network mounts |
| `host/proc/diskstats` | two disks, partitions and a loop device |
| `host/proc/net/dev` | loopback and two interfaces |
| `host/proc/<pid>/`, `host/proc/self/` | `stat` (one command name with a space and parentheses), `io`, `fd/` |
| `host/sys/block`, `host/sys/class/hwmon` | whole-disk markers and two temperature chips |
| `host/cgroup/system.slice/example.service` | cgroup v2 `cpu.stat`, `memory.current`, `pids.current`, `io.stat` |
| `host/root/` | mount-point directories for `statvfs` (the sizes come from whatever filesystem holds the test tree) |
