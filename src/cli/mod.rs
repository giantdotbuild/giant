//! CLI surface - parses args, dispatches to subcommand handlers.

use clap::{Parser, Subcommand};

mod affected;
mod build;
mod explain;
pub(crate) mod prep;

#[derive(Parser, Debug)]
#[command(name = "giant", version, about = "Build orchestration with content-addressed caching")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Path to giant.yaml / giant.json. Defaults to walking up from cwd.
    #[arg(long, global = true)]
    pub config: Option<std::path::PathBuf>,

    /// Force a fresh build (bypass cache).
    #[arg(long, global = true)]
    pub fresh: bool,

    /// Log filter (RUST_LOG syntax).
    #[arg(long, global = true, default_value = "warn")]
    pub log: String,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Build targets.
    Build(build::BuildArgs),

    /// List targets that would rebuild given a set of changed files.
    /// Doesn't actually run anything.
    Affected(affected::AffectedArgs),

    /// Show what feeds a target's cache key - the first thing to reach
    /// for when "why did this rebuild?" comes up.
    Explain(explain::ExplainArgs),
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
        Commands::Affected(args) => affected::execute(args, &global).await,
        Commands::Explain(args) => explain::execute(args, &global).await,
    }
}

/// Subset of CLI args that subcommands need to consult.
#[derive(Debug, Clone)]
pub struct GlobalFlags {
    pub config: Option<std::path::PathBuf>,
    pub fresh: bool,
}
