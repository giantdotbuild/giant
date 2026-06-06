//! `giant verify` subcommand - the hermeticity audit (ADR-0030 §5).
//!
//! `verify` is `build` with two things forced on: the sandbox (so every target
//! runs under enforcement) and a fresh build (so the cache is bypassed and
//! *every* target actually runs, rather than only enforcing on misses). A
//! target that reads an undeclared file, depends on a scrubbed env var, or
//! reaches the network fails here - that is the audit. It covers every target,
//! tests included.

use clap::Args;

use super::build;
use crate::selection;

#[derive(Args, Debug)]
pub struct VerifyArgs {
    /// Same selection and output flags as `build` (`--watch` aside, which
    /// makes little sense for a one-shot audit but is harmless).
    #[command(flatten)]
    pub build: build::BuildArgs,
}

pub async fn execute(args: VerifyArgs, global: &super::GlobalFlags) -> anyhow::Result<()> {
    // Force sandbox enforcement and cache bypass regardless of the ambient
    // flags; everything else (selection, tags, jobs) flows from the args.
    let forced = super::GlobalFlags {
        fresh: true,
        sandbox: true,
        ..global.clone()
    };
    build::execute_with_mode(args.build, &forced, selection::TestMode::Include).await
}
