//! Test kit for [heimr](https://github.com/hugsmid/heimr) adapters.
//!
//! - **Conformance** ([`Suite`]): a fixed catalogue of named scenarios
//!   ([`catalogue`]) checking what the adapter contract promises —
//!   configuration handling, lifecycle and shutdown, shadow and live
//!   command handling through the core's actuator, record well-formedness,
//!   stats, and behaviour under upstream faults. Each scenario reports pass,
//!   pass with a note (part of it could not be observed with the fixture),
//!   fail (with the reason), skip or expected failure; skips, notes and
//!   accepted failures must be declared by id
//!   ([`Suite::expect_skip`], [`Suite::expect_note`],
//!   [`Suite::expect_failure`]) or [`Report::assert_passed`] fails. A
//!   conformance test is one call:
//!
//!   ```no_run
//!   # use heimr_testkit::Suite;
//!   # use serde_json::json;
//!   Suite::new::<heimr::demo::Demo>(json!({ "interval": "10s" }))
//!       .invalid("interval", json!({ "interval": "0s" }))
//!       // Synthetic data: nothing to fault.
//!       .expect_skip("fault.no_hot_loop", "no upstream")
//!       // …one declaration per skip, note or accepted failure.
//!       .run()
//!       .assert_passed();
//!   ```
//!
//!   Scenarios only use the protocol-neutral [`Driver`] trait. The
//!   in-process driver ([`InProcess`]) runs the adapter inside the real app
//!   (supervisor, actuator, stats) on a fake clock; a runner for adapters in
//!   other processes can implement [`Driver`] and call [`run_scenarios`]
//!   with the same catalogue.
//! - **Harness** ([`Harness`]): run one adapter directly with a real
//!   context on a [`ManualClock`](heimr_core::ManualClock), for unit tests
//!   of its behaviour; records land in memory, commands go through a real
//!   actuator with an in-memory audit log.
//! - **Record/replay** ([`replay`], feature `replay`, on by default): serve
//!   a JSON cassette on a local mock server, simulate outages, rejected
//!   credentials and garbage, and save recordings only after redaction and
//!   a scan for identifying values.
//! - **Rendering** ([`render`]): stable text for snapshot tests.
//! - **Checks** ([`checks`]): the record checks as plain functions.
//!
//! Time: the core's clock seam is all an adapter needs; the kit uses
//! [`TokioClock`](heimr_core::TokioClock) under tokio's paused time
//! ([`ClockMode::Paused`]) or a stepped [`ManualClock`](heimr_core::ManualClock)
//! ([`ClockMode::Manual`]) for adapters doing real I/O.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod checks;
pub mod driver;
pub mod harness;
pub mod inprocess;
pub mod render;
#[cfg(feature = "replay")]
pub mod replay;
pub mod scenario;
pub mod suite;
pub mod upstream;
mod util;

pub use driver::{Driver, Failure, Fault, PrepareFailure, Traffic};
pub use harness::Harness;
pub use inprocess::{ClockMode, DriverOptions, INSTANCE, InProcess, t0};
pub use scenario::{
    Declaration, Expectations, Expected, Report, Scenario, ScenarioContext, ScenarioResult, Status,
    Verdict, catalogue,
};
pub use suite::{ONLY_ENV, Suite, run_scenarios, test_site};
pub use upstream::{StaticUpstream, Upstream};
