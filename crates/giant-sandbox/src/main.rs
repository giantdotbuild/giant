//! `giant-sandbox` - the per-target exec-wrapper porcelain (ADR-0030, TDD-0025).
//!
//! The engine resolves a target's bind set, writes it as a `SandboxSpec` JSON,
//! and prepends `giant-sandbox run --spec <file> --` to the target's argv. We
//! read the spec, apply the sandbox, and **spawn the build command ourselves**;
//! the actual exec moves here because the sandbox library installs its filters
//! as it forks the child. stdio is inherited straight through and the child's
//! exit code is propagated, so the engine sees one transparent child.
//!
//! This is a third porcelain shape (ADR-0030 §2): not a dispatched subcommand
//! like `giant gen`, not an event consumer like `giant tui`, but a per-target
//! exec wrapper in the spirit of `env` / `nice`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use clap::Parser;
use giant_schema::{SANDBOX_SPEC_SCHEMA, SandboxSpec};

#[cfg(target_os = "linux")]
mod sandbox;

/// Reserved exit code for a sandbox *setup* failure, distinct from the child's
/// own exit status so the engine can tell "could not sandbox" from "build
/// failed" (TDD-0025; matches the `env`/`docker` 125 convention).
const SETUP_FAILURE: u8 = 125;

#[derive(Parser)]
#[command(
    name = "giant-sandbox",
    version,
    about = "Sandbox exec-wrapper porcelain for Giant"
)]
enum Cli {
    /// Run a command inside the sandbox described by a `SandboxSpec`.
    Run {
        /// Path to the `SandboxSpec` JSON the engine wrote.
        #[arg(long, value_name = "FILE")]
        spec: PathBuf,

        /// The program and its arguments, after `--`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<OsString>,
    },
}

fn main() -> ExitCode {
    let Cli::Run { spec, command } = Cli::parse();
    match run(&spec, &command) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("giant-sandbox: {e:#}");
            ExitCode::from(SETUP_FAILURE)
        }
    }
}

/// Read and validate the spec, then hand off to the platform backend. Returns
/// the child's exit code on a normal run; any error here is a setup failure
/// (the caller maps it to [`SETUP_FAILURE`]).
fn run(spec_path: &Path, command: &[OsString]) -> anyhow::Result<u8> {
    let raw = std::fs::read(spec_path)
        .with_context(|| format!("reading spec {}", spec_path.display()))?;
    let spec: SandboxSpec = serde_json::from_slice(&raw).context("parsing SandboxSpec")?;
    anyhow::ensure!(
        spec.schema == SANDBOX_SPEC_SCHEMA,
        "unsupported SandboxSpec schema {} (this giant-sandbox speaks {SANDBOX_SPEC_SCHEMA})",
        spec.schema,
    );

    #[cfg(target_os = "linux")]
    {
        sandbox::run(&spec, command)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (&spec, command);
        anyhow::bail!("sandboxing is only supported on Linux")
    }
}
