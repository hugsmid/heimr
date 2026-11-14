//! `Report::assert_passed` is strict: every skip, note and failure must be
//! declared on the suite, every declaration must happen, and a typo in a
//! scenario id fails the run.

#![allow(clippy::unwrap_used)]

use std::panic::{AssertUnwindSafe, catch_unwind};

use heimr_core::{Adapter, AdapterContext, AdapterDescriptor, AdapterError, Capabilities};
use heimr_testkit::{ONLY_ENV, Report, Status, Suite};
use serde_json::json;

/// No settings at all.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NoKeys {}

/// Reports health and does nothing else.
struct HealthOnly;

impl Adapter for HealthOnly {
    type Config = NoKeys;
    fn descriptor() -> AdapterDescriptor {
        AdapterDescriptor::new(
            "health-only",
            "reports health, nothing else",
            Capabilities::read_only(),
        )
    }
    fn new(_: &NoKeys) -> Result<Self, AdapterError> {
        Ok(Self)
    }
    async fn run(self, ctx: AdapterContext<NoKeys>) -> Result<(), AdapterError> {
        ctx.health().ok().await?;
        ctx.shutdown().wait().await;
        Ok(())
    }
}

fn health_only() -> Suite {
    Suite::new::<HealthOnly>(json!({})).emits(false)
}

/// Every scenario a health-only adapter with fixed settings skips, and the
/// ones it passes with a note.
const SKIPS: &[&str] = &[
    "config.invalid_values",
    "config.no_io_in_new",
    "run.emits",
    "commands.shadow",
    "commands.no_unsolicited_writes",
    "commands.live",
    "fault.transient_recovers",
    "fault.no_hot_loop",
    "fault.rejected_is_fatal",
    "fault.malformed_no_panic",
];
const NOTES: &[&str] = &["records.monotonic", "records.keep_alive"];

fn declared() -> Suite {
    let mut s = health_only()
        .expect_note(NOTES[0], "no upstream to interrupt")
        .expect_note(NOTES[1], "no record-on-change series");
    for id in SKIPS {
        s = s.expect_skip(id, "nothing to configure, command or fault");
    }
    s
}

/// The panic message of `assert_passed`, or `None` if it passed.
fn assert_message(r: &Report) -> Option<String> {
    catch_unwind(AssertUnwindSafe(|| r.assert_passed()))
        .err()
        .map(|p| {
            p.downcast_ref::<String>()
                .cloned()
                .unwrap_or_else(|| "non-string panic".into())
        })
}

#[test]
fn undeclared_skips_and_notes_fail_and_are_named() {
    let r = health_only().run();
    let msg = assert_message(&r).expect("undeclared skips must fail");
    for id in SKIPS {
        assert!(
            msg.contains(&format!("- {id}: undeclared skip: ")),
            "{id} missing from\n{msg}"
        );
    }
    for id in NOTES {
        assert!(msg.contains(&format!("- {id}: undeclared note: ")), "{msg}");
    }
    assert_eq!(r.problems().len(), SKIPS.len() + NOTES.len(), "{msg}");
}

#[test]
fn declarations_make_it_pass() {
    let r = declared().run();
    assert_eq!(assert_message(&r), None, "{r}");
    assert_eq!(r.count(Status::Skipped), SKIPS.len());
    assert_eq!(r.count(Status::PassedWithNote), NOTES.len());
    assert_eq!(r.declared(), SKIPS.len() + NOTES.len());
    let summary = r.to_string();
    let last = summary.lines().last().unwrap();
    assert!(
        last.contains("10 skipped") && last.contains("12 as declared, 0 problems"),
        "{last}"
    );
}

#[test]
fn a_declared_skip_that_passes_fails() {
    let r = declared()
        .expect_skip("run.stays_up", "wrongly declared")
        .run();
    let msg = assert_message(&r).expect("a stale skip must fail");
    assert!(
        msg.contains(
            "- run.stays_up: declared skipped (wrongly declared) but passed: remove the declaration"
        ),
        "{msg}"
    );
}

#[test]
fn a_declared_failure_that_is_skipped_fails() {
    let r = declared()
        .expect_failure("fault.no_hot_loop", "retries too fast")
        .run();
    let msg = assert_message(&r).expect("a skipped expected failure must fail");
    assert!(
        msg.contains("- fault.no_hot_loop: declared to fail (retries too fast) but was skipped: "),
        "{msg}"
    );
}

#[test]
fn a_declaration_for_an_unknown_id_fails() {
    let r = declared().expect_failure("commands.lvie", "typo").run();
    let msg = assert_message(&r).expect("an unknown id must fail");
    assert!(
        msg.contains("- declaration for unknown scenario id \"commands.lvie\""),
        "{msg}"
    );
}

#[test]
fn conflicting_declarations_fail() {
    let r = declared()
        .expect_failure("commands.live", "contradiction")
        .run();
    let msg = assert_message(&r).expect("conflicting declarations must fail");
    assert!(
        msg.contains("- commands.live: declared skipped and declared to fail"),
        "{msg}"
    );
}

/// Runs only as a child of the tests below, with `ONLY_ENV` set.
#[test]
#[ignore = "run by only_* tests in a child process"]
fn only_child() {
    let Ok(only) = std::env::var(ONLY_ENV) else {
        return;
    };
    let r = declared().run();
    if only == "descriptor.valid" {
        assert_eq!(r.count(Status::Passed), 1, "{r}");
        assert_eq!(r.count(Status::Filtered), r.results.len() - 1, "{r}");
        assert_eq!(r.get("commands.live").unwrap().problem, None, "{r}");
    }
    r.assert_passed();
}

/// Run `only_child` with `ONLY_ENV` set and, if `ci` is given, `CI` set
/// to it; otherwise without `CI`, whatever the parent has (CI services set
/// it, and the filter is refused under it).
fn run_child_with(only: &str, ci: Option<&str>) -> (bool, String) {
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "only_child", "--ignored", "--nocapture"])
        .env(ONLY_ENV, only)
        .env_remove("CI");
    if let Some(ci) = ci {
        cmd.env("CI", ci);
    }
    let out = cmd.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn run_child(only: &str) -> (bool, String) {
    run_child_with(only, None)
}

/// Under CI the filter is refused and every scenario runs.
#[test]
fn only_is_refused_under_ci() {
    let (ok, text) = run_child_with("descriptor.valid", Some("true"));
    assert!(!ok, "{text}");
    assert!(
        text.contains("HEIMR_TESTKIT_ONLY is set in CI (\"descriptor.valid\"); every scenario was run: unset it"),
        "{text}"
    );
}

/// The filter tests themselves pass when the parent runs under CI.
#[test]
fn the_filter_tests_pass_under_ci() {
    for test in [
        "only_with_an_unknown_id_fails",
        "only_filters_without_enforcing_other_declarations",
    ] {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test])
            .env("CI", "true")
            .env_remove(ONLY_ENV)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{test} under CI=true:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn only_with_an_unknown_id_fails() {
    let (ok, text) = run_child("commands.lvie");
    assert!(!ok, "{text}");
    assert!(
        text.contains("- HEIMR_TESTKIT_ONLY names unknown scenario id \"commands.lvie\""),
        "{text}"
    );
}

#[test]
fn only_filters_without_enforcing_other_declarations() {
    let (ok, text) = run_child("descriptor.valid");
    assert!(ok, "{text}");
}
