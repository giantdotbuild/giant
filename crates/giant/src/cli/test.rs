//! `giant test` - same machinery as `giant build`, but the selection
//! is restricted to targets with `test: true`.
//!
//! Everything else (patterns, tags, --affected, --color, --quiet,
//! NDJSON, the renderer) is identical to `giant build`. The two
//! subcommands exist as siblings for muscle memory and to keep
//! `giant build` from accidentally running tests.

use clap::Args;

use super::build;
use crate::selection;

#[derive(Args, Debug)]
pub struct TestArgs {
    /// Everything `giant build` accepts, applied to test targets.
    #[command(flatten)]
    pub build: build::BuildArgs,
}

pub async fn execute(args: TestArgs, global: &super::GlobalFlags) -> anyhow::Result<()> {
    build::execute_with_mode(args.build, global, selection::TestMode::Only).await
}
