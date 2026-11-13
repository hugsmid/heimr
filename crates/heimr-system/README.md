# heimr-system

A read-only heimr adapter (`type = "system"`) that records the machine's own health as ordinary readings, so dashboards, alerts and the assistant can treat it like any other data: CPU (total and per core), load, memory and swap, pressure stall information, filesystem usage, disk and network throughput, temperatures, selected and top processes, systemd units (cgroup v2) and the hub process itself.

```toml
[adapters.host]
type = "system"
interval = "10s"
top = 5                          # top process names by CPU and by memory
processes = ["heimr", "sshd"]
units = ["heimr.service"]
```

Every key and every emitted metric is listed in the crate docs. Run `cargo run -p heimr-system --example sample` to see what this machine reports.

## Design

- **Direct reads, no metrics crate.** The adapter reads the few `/proc`, `/sys` and cgroup files it needs with small parsers that borrow from a reused buffer. General-purpose crates refresh much more than is used here and allocate per sample; these files are stable kernel interfaces. The only dependency beyond `heimr-core`, `jiff`, `serde` and `tokio` is `rustix` (safe bindings) for `statvfs`, the clock-tick rate and the page size.
- **No per-sample heap churn in the host-wide part.** Read buffers, path buffers, previous counters and interned device ids live across samples. Process scanning allocates one directory entry per process (the standard library's directory iterator) and a name only when a new command name appears.
- **Never blocks the runtime.** Each sample runs on a blocking thread; shutdown is honoured while a sample is in flight.
- **Safe by default.** Only block-device filesystems are queried unless `mounts` is set, so a hung network mount cannot stall sampling. Missing or unparseable files make health `degraded`; the adapter keeps running.

## Cost

Measure one sample on your own host with `cargo bench -p heimr-system` (criterion): it times the host-wide part (CPU, load, memory, PSI, filesystems, disks, network, temperatures) and a full sample with process scanning and the hub process.

## Platforms

Linux. It builds elsewhere and accepts the default settings, but finds no `/proc`, reports health `degraded` and emits nothing. Filesystem usage needs `statvfs` (unix). Units need cgroup v2.

## Testing

Parsers are tested on hand-built fixtures (`tests/fixtures/README.md`) and with `proptest` (no input makes them panic); rates are checked exactly between two fixture states; the adapter passes the `heimr-testkit` conformance suite against the fixture host, including an unreadable `/proc` and garbage file contents.

## Licence

Licensed under either of [Apache-2.0](../../LICENSE-APACHE) or [MIT](../../LICENSE-MIT) at your option.
