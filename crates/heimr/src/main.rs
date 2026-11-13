//! The public `heimr` binary: the app with the adapters that ship with it.
#![forbid(unsafe_code)]

// ANCHOR: main
fn main() -> std::process::ExitCode {
    heimr::cli::main("heimr", |b| b.public_adapters())
}
// ANCHOR_END: main
