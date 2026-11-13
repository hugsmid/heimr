//! The cost of one sample on this machine's live `/proc` and `/sys`: the
//! host-wide part alone, and with process scanning (top 5 plus the hub).
//!
//! ```bash
//! cargo bench -p heimr-system
//! ```

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use heimr_system::{Options, Sampler};

fn sampler(processes: bool) -> Sampler {
    let mut o = Options::new("/proc", "/sys");
    if !processes {
        o.top = 0;
        o.hub = false;
    }
    Sampler::new(o)
}

fn bench(c: &mut Criterion) {
    for (name, processes) in [("sample/host", false), ("sample/host+processes", true)] {
        let mut s = sampler(processes);
        let mut out = Vec::with_capacity(512);
        let mut t = 0.0;
        // Warm up rates, buffers and device ids.
        s.sample(t, &mut out);
        c.bench_function(name, |b| {
            b.iter(|| {
                t += 10.0;
                out.clear();
                black_box(s.sample(t, &mut out));
                black_box(out.len())
            });
        });
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
