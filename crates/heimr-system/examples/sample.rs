//! Print two samples of this machine, one second apart (rates need two).
//!
//! ```bash
//! cargo run -p heimr-system --example sample
//! ```

use heimr_system::{Options, Sampler};

fn main() {
    let mut sampler = Sampler::new(Options::new("/proc", "/sys"));
    let mut out = Vec::new();
    let start = std::time::Instant::now();
    if let Some(problem) = sampler.sample(0.0, &mut out) {
        eprintln!("first sample: {problem}");
    }
    std::thread::sleep(std::time::Duration::from_secs(1));
    out.clear();
    let t = start.elapsed().as_secs_f64();
    let begun = std::time::Instant::now();
    let problem = sampler.sample(t, &mut out);
    let took = begun.elapsed();
    for s in &out {
        println!(
            "{:<32} {:<18} {:>16.2} {}",
            sampler.device(s.device),
            s.metric.name(),
            s.value,
            s.metric.unit()
        );
    }
    if let Some(p) = problem {
        eprintln!("problem: {p}");
    }
    eprintln!("{} values in {took:?}", out.len());
}
