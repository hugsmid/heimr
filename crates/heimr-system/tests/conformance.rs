//! The heimr conformance suite against the fixture host.

mod common;

use heimr_system::System;
use heimr_testkit::Suite;
use serde_json::json;

const READ_ONLY: &str = "read-only adapter: no commands";
const NO_COUNT: &str = "the fixture file tree does not count reads or writes";

#[test]
fn conformance() {
    Suite::with_upstream::<System, _, _, _>(|| async { common::Host::new() })
        .invalid("interval", json!({ "interval": "500ms" }))
        .invalid("top", json!({ "top": 51 }))
        .invalid("proc_path", json!({ "proc_path": "relative/proc" }))
        .invalid("units", json!({ "units": ["../escape.service"] }))
        .expect_skip("commands.shadow", READ_ONLY)
        .expect_skip("commands.live", READ_ONLY)
        .expect_skip("config.no_io_in_new", NO_COUNT)
        .expect_skip("commands.no_unsolicited_writes", NO_COUNT)
        .expect_skip("fault.no_hot_loop", NO_COUNT)
        .expect_skip(
            "fault.rejected_is_fatal",
            "a file tree has no credentials to reject",
        )
        .run()
        .assert_passed();
}
