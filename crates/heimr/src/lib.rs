//! heimr: a config-driven home brain.
//!
//! The library runs registered [adapter kinds](heimr_core::AdapterFactory)
//! as configured instances, each under its own supervisor, behind the
//! command safety layer ([`heimr_core::Actuator`]) with a durable audit
//! [journal](JsonlJournal), and feeds their records to [sinks](Sink), driven
//! by a typed, validated [`Config`]. The `heimr` binary registers the public
//! adapter kinds; a site binary registers its private ones too, with the
//! same three lines.
//!
//! ```no_run
//! use heimr::{App, Config};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let config = Config::load("heimr.toml")?;
//! let app = App::builder(config)
//!     .public_adapters()          // the demo adapter (more to come)
//!     // .adapter::<MyPrivateAdapter>()
//!     .build()
//!     .await?;
//! let planner = app.submitter("planner")?; // submit-only, bound to its actor
//! app.run().await?;               // until SIGINT/SIGTERM
//! # drop(planner);
//! # Ok(()) }
//! ```
//!
//! Instances are `[adapters.<instance>]` sections with `type = "<kind>"`;
//! several instances of one kind may run. Commands reach an instance only if
//! its section says `mode = "live"`; the default, shadow, audits commands
//! and delivers nothing.
//!
//! A failing instance is marked unhealthy and restarted with exponential
//! backoff and jitter; a fatal error, or repeated panics, keeps it down.
//! Only core failures stop the app. Each instance runs in a `tracing` span
//! named after it, and [`App::telemetry`] gives per-instance stats (records
//! emitted, commands delivered/shadowed/rejected/dropped, health, runs and
//! restarts) with no adapter code.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod app;
pub mod cli;
pub mod config;
pub mod demo;
pub mod journal;
pub mod sink;
pub mod telemetry;

pub use app::{ACTUATOR_SOURCE, APP_SOURCE, AdapterPlan, App, AppBuilder, Error, shutdown_signal};
pub use config::{Config, ConfigError};
pub use heimr_core as core;
pub use journal::{JournalOptions, JsonlJournal};
pub use sink::{LoggingSink, MemorySink, Sink};
pub use telemetry::Telemetry;
