//! Giant CLI entry point.
//!
//! `main` does only enough to set up the tokio runtime, parse the
//! top-level CLI, and dispatch into `cli::run`. All meaningful work
//! is async and lives in the library.

use std::process::ExitCode;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    match giant::cli::run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // `SilentExit` lets a subcommand request a non-zero exit
            // without an error banner - for build failures where the
            // renderer's summary already names what went wrong.
            if e.downcast_ref::<giant::cli::SilentExit>().is_none() {
                eprintln!("{e:#}");
            }
            ExitCode::FAILURE
        }
    }
}
