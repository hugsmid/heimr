//! Command-line entry point shared by the public `heimr` binary and site
//! binaries that add their own adapters.
//!
//! ```text
//! heimr --config <path>          run
//! heimr check-config <path>      validate and exit
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::app::{App, AppBuilder};
use crate::config::Config;

#[derive(Debug, Parser)]
#[command(version, about = "heimr: a config-driven home brain")]
struct Cli {
    /// Config file to run with.
    #[arg(long, short, env = "HEIMR_CONFIG", value_name = "PATH")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Validate a config file against the adapters in this build, then exit.
    CheckConfig {
        /// Config file.
        path: PathBuf,
    },
}

/// Parse arguments and run. `bin_name` appears in help and errors;
/// `register` adds adapters (e.g. `|b| b.public_adapters()`).
///
/// ```no_run
/// fn main() -> std::process::ExitCode {
///     heimr::cli::main("heimr", |b| b.public_adapters())
/// }
/// ```
pub fn main(bin_name: &'static str, register: impl FnOnce(AppBuilder) -> AppBuilder) -> ExitCode {
    let matches = Cli::command().name(bin_name).get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(c) => c,
        Err(e) => e.exit(),
    };
    let result = match (cli.command, cli.config) {
        (Some(Cmd::CheckConfig { path }), _) => check(&path, register),
        (None, Some(path)) => run(&path, register),
        (None, None) => Err(format!(
            "{bin_name}: --config <PATH> is required (or use check-config)"
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn chain(e: &dyn std::error::Error) -> String {
    let mut msg = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        let s_msg = s.to_string();
        if !msg.contains(&s_msg) {
            msg.push_str(": ");
            msg.push_str(&s_msg);
        }
        src = s.source();
    }
    msg
}

fn check(
    path: &std::path::Path,
    register: impl FnOnce(AppBuilder) -> AppBuilder,
) -> Result<(), String> {
    let config = Config::load(path).map_err(|e| format!("{}: {}", path.display(), chain(&e)))?;
    let builder = register(App::builder(config));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("starting runtime: {e}"))?;
    let plans = rt
        .block_on(builder.check())
        .map_err(|e| format!("{}: {}", path.display(), chain(&e)))?;
    println!("{}: OK", path.display());
    for p in plans {
        let writes: Vec<&str> = p.capabilities.write.iter().map(|k| k.as_str()).collect();
        let state = if p.enabled {
            format!("enabled, {}", p.mode.as_str())
        } else {
            "disabled".to_owned()
        };
        let access = if writes.is_empty() {
            "read-only".to_owned()
        } else {
            format!("writes {}", writes.join(", "))
        };
        println!(
            "  {:<16} type {}; {state}; {access}",
            p.name, p.descriptor.kind
        );
    }
    let kinds: Vec<String> = builder
        .kinds()
        .into_iter()
        .map(|d| format!("{} ({})", d.kind, d.summary))
        .collect();
    println!("adapter kinds in this build: {}", kinds.join("; "));
    Ok(())
}

fn run(
    path: &std::path::Path,
    register: impl FnOnce(AppBuilder) -> AppBuilder,
) -> Result<(), String> {
    let config = Config::load(path).map_err(|e| format!("{}: {}", path.display(), chain(&e)))?;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .or_else(|_| tracing_subscriber::EnvFilter::try_new(&config.log.filter))
        .map_err(|e| format!("log filter: {e}"))?;
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("starting runtime: {e}"))?;
    rt.block_on(async {
        let app = register(App::builder(config))
            .build()
            .await
            .map_err(|e| format!("{}: {}", path.display(), chain(&e)))?;
        for (name, mode) in app.adapters() {
            tracing::info!(adapter = %name, mode = mode.as_str(), "enabled");
        }
        app.run().await.map_err(|e| chain(&e))
    })
}
