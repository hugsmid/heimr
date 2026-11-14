# heimr-system

A read-only heimr adapter (`type = "system"`) that records the machine's own health as ordinary readings, so dashboards, alerts and the assistant can treat it like any other data: CPU (total and per core), load, memory and swap, pressure stall information, filesystem usage, disk and network throughput, temperatures, selected and top processes, systemd units (cgroup v2) and the hub process itself.

```toml
[adapters.host]
type = "system"
interval = "10s"
top = 5                          # top process names by CPU and by memory (opt-in; default 0)
processes = ["heimr", "sshd"]
units = ["heimr.service"]
# series = ["cpu.usage", "memory.*", "host/load.1"]   # default: all
# max_series = 1000              # ceiling on series sent
```

Every key and every emitted metric is listed in the crate docs. Run `cargo run -p heimr-system --example sample` to see what this machine reports.

## Design

- **Direct reads, no metrics crate.** The adapter reads the few `/proc`, `/sys` and cgroup files it needs with small parsers that borrow from a reused buffer. General-purpose crates refresh much more than is used here and allocate per sample; these files are stable kernel interfaces. The only dependency beyond `heimr-core`, `jiff`, `serde` and `tokio` is `rustix` (safe bindings) for `statvfs`, the clock-tick rate and the page size.
- **No per-sample heap churn in the host-wide part.** Read buffers, path buffers, previous counters and interned device ids live across samples. Process scanning allocates one directory entry per process (the standard library's directory iterator) and a name only when a new command name appears.
- **Never blocks the runtime.** Each sample runs on a blocking thread; shutdown is honoured while a sample is in flight.
- **Series counted and capped.** Host metrics multiply (per core, disk, interface, process and unit). `series` limits what is sent to listed metrics, metric prefixes (`proc.*`) or single device metrics (`host/load.1`); `max_series` (default 1 000) caps the number of series, holding new ones back with health `degraded`. The count is the custom gauge `series`. Top-N processes (`top`) are off by default: their names churn from sample to sample and each new name is a new series that would fill the ceiling, so watch named `processes` instead or opt in. Every series template has the retention class `Diagnostic` (*provisional*: it may change when the store is added; see the adapter guide).
- **Safe by default.** Only block-device filesystems are queried unless `mounts` is set, so a hung network mount cannot stall sampling. Missing or unparseable files make health `degraded`; the adapter keeps running.

## Cost

Measure one sample on your own host with `cargo bench -p heimr-system` (criterion): it times the host-wide part (CPU, load, memory, PSI, filesystems, disks, network, temperatures) and a full sample with process scanning and the hub process.

## Platforms

Linux. It builds elsewhere and accepts the default settings, but finds no `/proc`, reports health `degraded` and emits nothing. Filesystem usage needs `statvfs` (unix). Units need cgroup v2.

## Testing

Parsers are tested on hand-built fixtures (`tests/fixtures/README.md`) and with `proptest` (no input makes them panic); rates are checked exactly between two fixture states; the allow-list and the ceiling are run on the test kit's harness; the adapter passes the `heimr-testkit` conformance suite against the fixture host, including an unreadable `/proc` and garbage file contents.

## Licence

Licensed under either of [Apache-2.0](../../LICENSE-APACHE) or [MIT](../../LICENSE-MIT) at your option.
