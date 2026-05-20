//! `giant build` subcommand.

use clap::Args;

#[derive(Args, Debug)]
pub struct BuildArgs {
    /// Target patterns (glob over target IDs).
    pub patterns: Vec<String>,

    /// Number of parallel jobs (default: number of CPUs).
    #[arg(short = 'j', long)]
    pub jobs: Option<usize>,

    /// Show events as NDJSON on stdout instead of the tty renderer.
    #[arg(long, value_name = "FORMAT")]
    pub events: Option<EventsFormat>,

    /// Watch mode: build, then keep rebuilding on file changes.
    #[arg(short, long)]
    pub watch: bool,

    /// Only build targets affected since `<ref>` (git diff).
    #[arg(long)]
    pub affected: bool,

    #[arg(long)]
    pub base: Option<String>,

    #[arg(long, value_name = "PATH")]
    pub file: Vec<String>,

    #[arg(long)]
    pub tag: Vec<String>,

    #[arg(long)]
    pub no_tag: Vec<String>,

    #[arg(long)]
    pub include_tests: bool,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
pub enum EventsFormat {
    Ndjson,
}

pub async fn execute(_args: BuildArgs, _global: &super::GlobalFlags) -> anyhow::Result<()> {
    // Phase 1 skeleton: not wired up yet. See roadmap.md Phase 1.
    anyhow::bail!("`giant build` not yet implemented - TDDs 0001-0012 are the spec; code lands in Phase 1")
}
