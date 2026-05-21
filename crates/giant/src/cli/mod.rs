//! CLI surface - parses args, dispatches to subcommand handlers.
//!
//! Built-in subcommands are matched first. Unknown subcommands fall
//! through to porcelain dispatch: `giant <name>` looks for `giant-<name>`
//! on PATH and execs it (git/cargo/kubectl model - see ADR-0010).

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

mod affected;
mod build;
mod clean;
mod completions;
pub(crate) mod dynamic;
mod explain;
mod external;
mod graph;
pub(crate) mod prep;
mod session;
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

    /// Persistent engine over stdio. Loads config once, runs
    /// discovery once, then reads JSON commands on stdin and emits
    /// NDJSON events on stdout. The protocol porcelains (the TUI in
    /// particular) drive against. Refuses to run with stdout on a
    /// TTY - pipe it. See TDD-0014.
    Session(session::SessionArgs),

    /// Generate a shell completion script for bash / zsh / fish /
    /// powershell / elvish / nushell. Pipe the output into the right
    /// place for your shell.
    Completions(completions::CompletionsArgs),

    /// Unknown subcommand → dispatch to `giant-<name>` on PATH if
    /// available, else error with a helpful hint (ADR-0010).
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

/// Entry point invoked from `main`.
pub async fn run() -> anyhow::Result<()> {
    // Dynamic completion: when invoked by the shell at TAB time, clap
    // sees the COMPLETE env var and emits suggestions on stdout, then
    // exits - without ever reaching the normal command dispatch.
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();

    // Build the clap Command from the derived Cli, then dynamically
    // append a list of detected porcelains to the help output. The
    // porcelains aren't real subcommands as far as clap is concerned;
    // they still get dispatched via `Commands::External`. The
    // after-help section is just so users see them in `--help`.
    let mut cmd = Cli::command();
    if let Some(blurb) = porcelain_help_section() {
        cmd = cmd.after_help(blurb);
    }
    let matches = cmd.get_matches();
    let cli = Cli::from_arg_matches(&matches).expect("clap derive ensures shape");

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
        Commands::Session(args) => session::execute(args, &global).await,
        Commands::Completions(args) => completions::execute(args),
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

/// Subcommand names that clap already knows about - porcelain
/// detection skips these so we don't end up listing `giant-clean` as
/// a porcelain alongside the built-in `clean`.
const BUILTIN_SUBCOMMANDS: &[&str] = &[
    "build",
    "test",
    "affected",
    "explain",
    "graph",
    "clean",
    "watch",
    "session",
    "completions",
];

/// Format a one-line-per-porcelain "after help" section, or `None` if
/// nothing was found on PATH. Output looks like:
///
/// ```text
/// Porcelains (extra subcommands provided by binaries on PATH):
///   task    /home/user/.cargo/bin/giant-task
///   tui     /usr/local/bin/giant-tui
/// ```
fn porcelain_help_section() -> Option<String> {
    let porcelains = detect_porcelains();
    if porcelains.is_empty() {
        return None;
    }
    let width = porcelains.keys().map(|n| n.len()).max().unwrap_or(0);
    let mut out = String::from(
        "Porcelains (extra subcommands provided by binaries on PATH):\n",
    );
    for (name, path) in &porcelains {
        out.push_str(&format!(
            "  {:<width$}  {}\n",
            name,
            path.display(),
            width = width
        ));
    }
    out.push_str("\nRun `giant <name> --help` for the porcelain's own help.");
    Some(out)
}

/// Walk PATH for `giant-<name>` executables that don't shadow a
/// built-in subcommand. Returns a sorted map of `name → path`; only
/// the first occurrence (earliest in PATH) wins, mirroring how the
/// shell resolves names.
fn detect_porcelains() -> BTreeMap<String, PathBuf> {
    use std::collections::HashSet;
    let builtins: HashSet<&'static str> = BUILTIN_SUBCOMMANDS.iter().copied().collect();
    let mut found: BTreeMap<String, PathBuf> = BTreeMap::new();
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            let Some(porc) = name.strip_prefix("giant-") else {
                continue;
            };
            if porc.is_empty() || builtins.contains(porc) {
                continue;
            }
            if found.contains_key(porc) {
                continue;
            }
            if is_executable(&entry.path()) {
                found.insert(porc.to_string(), entry.path());
            }
        }
    }
    found
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    // On Windows, PATHEXT lookup is the right answer; for now treat
    // any file matching the prefix as executable.
    std::fs::metadata(path).map(|m| m.is_file()).unwrap_or(false)
}
