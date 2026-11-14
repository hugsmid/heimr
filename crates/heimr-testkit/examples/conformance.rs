//! Run the conformance suite against the `demo` adapter and print the
//! report.
//!
//! ```bash
//! cargo run -p heimr-testkit --example conformance
//! ```

use heimr::demo::Demo;
use heimr_testkit::Suite;
use serde_json::json;

/// The demo's data is synthetic: there is no upstream to count traffic,
/// fail or interrupt.
const POLLED: &str = "polled: no record-on-change series";
const NO_UPSTREAM: &str = "synthetic data: no upstream to count traffic or simulate faults";

fn main() -> std::process::ExitCode {
    // `run` prints the report.
    let report = Suite::new::<Demo>(json!({ "interval": "10s", "zones": ["zone-a"] }))
        .invalid("interval", json!({ "interval": "0s" }))
        .expect_skip("config.no_io_in_new", NO_UPSTREAM)
        .expect_skip("commands.no_unsolicited_writes", NO_UPSTREAM)
        .expect_skip("fault.transient_recovers", NO_UPSTREAM)
        .expect_skip("fault.no_hot_loop", NO_UPSTREAM)
        .expect_skip("fault.rejected_is_fatal", NO_UPSTREAM)
        .expect_skip("fault.malformed_no_panic", NO_UPSTREAM)
        .expect_note("records.monotonic", NO_UPSTREAM)
        .expect_note("records.keep_alive", POLLED)
        .expect_note("commands.shadow", NO_UPSTREAM)
        .run();
    if report.passed() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}
