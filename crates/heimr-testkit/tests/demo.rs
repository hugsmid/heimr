//! The kit applied to the `demo` adapter that ships with heimr.

use heimr::demo::Demo;
use heimr_testkit::Suite;
use serde_json::json;

/// The demo's data is synthetic: there is no upstream to count traffic,
/// fail or interrupt.
const NO_UPSTREAM: &str = "synthetic data: no upstream to count traffic or simulate faults";

#[test]
fn demo_conforms() {
    Suite::new::<Demo>(json!({ "interval": "10s", "zones": ["zone-a", "zone-b"] }))
        .invalid("interval", json!({ "interval": "0s" }))
        .invalid("zones", json!({ "zones": [] }))
        .invalid("zones", json!({ "zones": ["zone-a", "zone-a"] }))
        .invalid("schedule_block", json!({ "schedule_block": "10s" }))
        .invalid_any(json!({ "initial_target": 35.0 }))
        .expect_skip("config.no_io_in_new", NO_UPSTREAM)
        .expect_skip("commands.no_unsolicited_writes", NO_UPSTREAM)
        .expect_skip("fault.transient_recovers", NO_UPSTREAM)
        .expect_skip("fault.no_hot_loop", NO_UPSTREAM)
        .expect_skip("fault.rejected_is_fatal", NO_UPSTREAM)
        .expect_skip("fault.malformed_no_panic", NO_UPSTREAM)
        .expect_note("records.monotonic", NO_UPSTREAM)
        .expect_note("commands.shadow", NO_UPSTREAM)
        .run()
        .assert_passed();
}
