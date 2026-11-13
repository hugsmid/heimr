//! Config validation against registered adapter kinds.

#![allow(clippy::unwrap_used)]

use heimr::core::{
    Adapter, AdapterContext, AdapterDescriptor, AdapterError, AdapterFactory, AdapterInstance,
    BoxFuture, Capabilities, CommandKind, Mode,
};
use heimr::{App, Config};

/// `AppBuilder::check` is async (an out-of-process kind may answer over
/// its protocol); these tests drive it to completion.
trait CheckNow {
    fn check_now(&self) -> Result<Vec<heimr::AdapterPlan>, heimr::Error>;
}

impl CheckNow for heimr::AppBuilder {
    fn check_now(&self) -> Result<Vec<heimr::AdapterPlan>, heimr::Error> {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(self.check())
    }
}

fn check(toml: &str) -> Result<Vec<heimr::AdapterPlan>, String> {
    let config = Config::from_toml_str(toml).map_err(|e| e.to_string())?;
    App::builder(config)
        .public_adapters()
        .check_now()
        .map_err(|e| e.to_string())
}

#[test]
fn example_config_is_valid() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/heimr.example.toml");
    let config = Config::load(path).unwrap();
    let plans = App::builder(config).public_adapters().check_now().unwrap();
    let names: Vec<_> = plans.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["demo", "demo-b"]);
    assert!(plans[0].enabled);
    assert!(!plans[1].enabled);
    assert!(
        plans.iter().all(|p| p.mode == Mode::Shadow),
        "the example must not enable live mode"
    );
    assert!(plans.iter().all(|p| p.descriptor.kind == "demo"));
}

#[test]
fn no_sections_means_no_instances() {
    assert!(check("").unwrap().is_empty());
}

#[test]
fn core_names_are_not_instance_names() {
    for name in ["actuator", "core", "heimr", "heimr-app"] {
        let err = check(&format!("[adapters.{name}]\ntype = \"demo\"")).unwrap_err();
        assert!(err.contains("reserved for the core"), "{name}: {err}");
    }
}

#[test]
fn instance_errors_name_the_key() {
    assert_eq!(
        check("[adapters.demo]\ntype = \"demo\"\nintervall = \"1m\"").unwrap_err(),
        "adapters.demo.intervall: unknown key"
    );
    assert_eq!(
        check("[adapters.x]\ntype = \"nope\"").unwrap_err(),
        "adapters.x.type: no adapter kind \"nope\" in this build (available: demo)"
    );
    assert_eq!(
        check("[adapters.demo]\ntype = \"demo\"\nzones = []").unwrap_err(),
        "adapters.demo.zones: must list 1 to 32 zones"
    );
    assert!(
        check("[adapters.demo]\ntype = \"demo\"\ninitial_target = 35.0")
            .unwrap_err()
            .contains("outside the absolute envelope")
    );
    assert!(
        check("[adapters.demo]\ntype = \"demo\"\ninterval = 5")
            .unwrap_err()
            .starts_with("adapters.demo: invalid configuration:")
    );
    // Disabled sections are still validated.
    assert!(check("[adapters.demo]\ntype = \"demo\"\nenabled = false\nbogus = 1").is_err());
    // Typos in the reserved keys can never mean live.
    assert!(check("[adapters.demo]\ntype = \"demo\"\nmode = \"life\"").is_err());
    assert!(check("[adapters.demo]\ntype = \"demo\"\nmdoe = \"live\"").is_err());
    assert!(check("[adapters.demo]\ntype = \"demo\"\nenabled = 1").is_err());
}

#[test]
fn several_instances_of_one_kind() {
    let plans = check(
        "[adapters.down]\ntype = \"demo\"\nzones = [\"a\"]\n[adapters.up]\ntype = \"demo\"\nmode = \"live\"\nzones = [\"a\"]",
    )
    .unwrap();
    assert_eq!(plans.len(), 2);
    assert_eq!((plans[0].mode, plans[1].mode), (Mode::Shadow, Mode::Live));
}

#[test]
fn live_requires_explicit_opt_in() {
    let plans = check("[adapters.demo]\ntype = \"demo\"").unwrap();
    assert_eq!(plans[0].mode, Mode::Shadow);
    let plans = check("[adapters.demo]\ntype = \"demo\"\nmode = \"live\"").unwrap();
    assert_eq!(plans[0].mode, Mode::Live);
}

#[derive(Debug, serde::Deserialize)]
struct NoConfig {}

struct Sensor;

impl Adapter for Sensor {
    type Config = NoConfig;
    fn descriptor() -> AdapterDescriptor {
        AdapterDescriptor::new("sensor", "read-only test sensor", Capabilities::read_only())
    }
    fn new(_: &NoConfig) -> Result<Self, AdapterError> {
        Ok(Self)
    }
    async fn run(self, ctx: AdapterContext<NoConfig>) -> Result<(), AdapterError> {
        ctx.shutdown().wait().await;
        Ok(())
    }
}

#[test]
fn read_only_kinds_cannot_be_live() {
    let config = Config::from_toml_str("[adapters.s]\ntype = \"sensor\"\nmode = \"live\"").unwrap();
    let err = App::builder(config)
        .adapter::<Sensor>()
        .check_now()
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "adapters.s.mode: this adapter is read-only and cannot be live"
    );
}

/// A kind whose descriptor is only known at run time (as an
/// external-process host would register one).
#[derive(Debug)]
struct RuntimeKind(AdapterDescriptor);

impl AdapterFactory for RuntimeKind {
    fn descriptor(&self) -> AdapterDescriptor {
        self.0.clone()
    }
    fn prepare(
        &self,
        settings: &serde_json::Value,
    ) -> BoxFuture<Result<Box<dyn AdapterInstance>, AdapterError>> {
        // A real host would ask the other process here.
        let result = if settings.as_object().is_some_and(|o| !o.is_empty()) {
            Err(AdapterError::config_key("x", "unknown key"))
        } else {
            Err(AdapterError::config("not runnable in this test"))
        };
        Box::pin(async move { result })
    }
}

#[test]
fn dynamic_kinds_register_and_validate() {
    let described = r#"{"kind": "remote-ventilation", "summary": "from another process",
        "capabilities": {"read": true, "write": ["fan_mode"], "fan_levels": {"min": 1, "max": 6}, "fan_presets": ["boost"]}}"#;
    let d: AdapterDescriptor = serde_json::from_str(described).unwrap();
    let config =
        Config::from_toml_str("[adapters.vent]\ntype = \"remote-ventilation\"\nx = 1").unwrap();
    let builder = App::builder(config).factory(RuntimeKind(d.clone()));
    assert_eq!(builder.kinds(), [d]);
    assert_eq!(
        builder.check_now().unwrap_err().to_string(),
        "adapters.vent.x: unknown key"
    );

    // An inconsistent descriptor is refused at registration.
    let bad = AdapterDescriptor::new("bad", "x", Capabilities::read_write([CommandKind::FanMode]));
    let err = App::builder(Config::default())
        .factory(RuntimeKind(bad))
        .check_now()
        .unwrap_err();
    assert!(err.to_string().contains("fan levels or presets"), "{err}");
    // Twice the same kind is refused.
    let err = App::builder(Config::default())
        .public_adapters()
        .public_adapters()
        .check_now()
        .unwrap_err();
    assert!(err.to_string().contains("registered twice"));
}
