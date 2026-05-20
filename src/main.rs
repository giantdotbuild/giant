//! Giant CLI entry point.
//!
//! `main` does only enough to set up the tokio runtime, parse the
//! top-level CLI, and dispatch into `cli::run`. All meaningful work
//! is async and lives in the library (ADR-0009).

use std::process::ExitCode;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    match giant::cli::run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e:#}");
            ExitCode::FAILURE
        }
    }
}
