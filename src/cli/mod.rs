//! CLI surface - parses args, dispatches to subcommand handlers.
//!
//! Built-in subcommands are matched first. Unknown subcommands fall
//! through to porcelain dispatch: `giant <name>` looks for `giant-<name>`
//! on PATH and execs it (git/cargo/kubectl model - see ADR-0010).

use clap::{Parser, Subcommand};
use std::ffi::OsString;

mod affected;
mod build;
mod clean;
mod explain;
mod external;
mod graph;
pub(crate) mod prep;
mod test;
mod watch;

#[derive(Parser, Debug)]
#[command(
    name = "giant",
    version,
    about = "Build orchestration with content-addressed caching"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Path to giant.yaml / giant.json. Defaults to walking up from cwd.
    #[arg(long, global = true)]
    pub config: Option<std::path::PathBuf>,

    /// Force a fresh build (bypass cache).
    #[arg(long, global = true)]
    pub fresh: bool,

    /// Log filter (RUST_LOG syntax). Defaults to errors only - pass
    /// `--log warn` (or set `RUST_LOG=giant=warn`) when debugging.
    #[arg(long, global = true, default_value = "error")]
    pub log: String,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Build targets.
    Build(build::BuildArgs),

    /// Run test targets. Same flags as `build`, but the selection is
    /// restricted to targets with `test: true`.
    Test(test::TestArgs),

    /// List targets that would rebuild given a set of changed files.
    /// Doesn't actually run anything.
    Affected(affected::AffectedArgs),

    /// Show what feeds a target's cache key - the first thing to reach
    /// for when "why did this rebuild?" comes up.
    Explain(explain::ExplainArgs),

    /// List targets, or show a target's dep tree.
    Graph(graph::GraphArgs),

    /// Clear the local cache.
    Clean(clean::CleanArgs),

    /// Run an initial build, then continuously rebuild affected
    /// targets when files change. Ctrl-C to exit.
    Watch(watch::WatchArgs),

    /// Unknown subcommand → dispatch to `giant-<name>` on PATH if
    /// available, else error with a helpful hint (ADR-0010).
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

/// Entry point invoked from `main`.
pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&cli.log));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let global = GlobalFlags {
        config: cli.config.clone(),
        fresh: cli.fresh,
    };

    match cli.command {
        Commands::Build(args) => build::execute(args, &global).await,
        Commands::Test(args) => test::execute(args, &global).await,
        Commands::Affected(args) => affected::execute(args, &global).await,
        Commands::Explain(args) => explain::execute(args, &global).await,
        Commands::Graph(args) => graph::execute(args, &global).await,
        Commands::Clean(args) => clean::execute(args, &global).await,
        Commands::Watch(args) => watch::execute(args, &global).await,
        Commands::External(args) => external::dispatch(args),
    }
}

/// Subset of CLI args that subcommands need to consult.
#[derive(Debug, Clone)]
pub struct GlobalFlags {
    pub config: Option<std::path::PathBuf>,
    pub fresh: bool,
}

/// Returned by a subcommand to exit non-zero without `main` printing
/// an error banner. Used when the subcommand has already produced
/// user-facing failure output (e.g., the build summary).
#[derive(Debug)]
pub struct SilentExit;

impl std::fmt::Display for SilentExit {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Ok(())
    }
}

impl std::error::Error for SilentExit {}
