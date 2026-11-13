//! Compiles and runs `templates/adapter` (its library and its tests) as
//! part of this crate's tests, so the template cannot rot. The template is
//! a standalone crate meant to be copied; here its sources are included
//! under the crate name it uses.

extern crate self as example_adapter;

#[path = "../../../templates/adapter/src/lib.rs"]
mod template;
pub use template::*;

#[path = "../../../templates/adapter/tests/conformance.rs"]
mod conformance;

#[path = "../../../templates/adapter/tests/behaviour.rs"]
mod behaviour;

/// The template states heimr's minimum Rust version, in its manifest and
/// its README, since a copy has no toolchain file.
#[test]
fn template_states_the_workspace_rust_version() {
    let field = |text: &str, table: &[&str]| -> String {
        let mut v = &toml::from_str::<toml::Value>(text).unwrap();
        for t in table {
            v = &v[*t];
        }
        v["rust-version"].as_str().unwrap().to_owned()
    };
    let workspace = field(
        include_str!("../../../Cargo.toml"),
        &["workspace", "package"],
    );
    let template = field(
        include_str!("../../../templates/adapter/Cargo.toml"),
        &["package"],
    );
    assert_eq!(template, workspace);
    let readme = include_str!("../../../templates/adapter/README.md");
    assert!(
        readme.contains(&format!("Rust {workspace} or newer")) && readme.contains("rustup"),
        "templates/adapter/README.md must state Rust {workspace} and how to get it"
    );
}

/// The template's manifest declares every crate its sources use.
#[test]
fn template_manifest_lists_its_dependencies() {
    let manifest = include_str!("../../../templates/adapter/Cargo.toml");
    for dep in [
        "heimr-core",
        "heimr-testkit",
        "jiff",
        "reqwest",
        "serde",
        "serde_json",
        "tokio",
    ] {
        assert!(
            manifest
                .lines()
                .any(|l| l.starts_with(&format!("{dep} = "))),
            "templates/adapter/Cargo.toml does not declare {dep}"
        );
    }
    assert!(
        manifest.contains("\n[workspace]\n"),
        "the template is its own workspace"
    );
}
