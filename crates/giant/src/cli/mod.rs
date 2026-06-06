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
mod logs;
pub(crate) mod prep;
mod session;
mod test;
mod verify;
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
    /// Must appear before the subcommand - `--config` is intentionally
    /// not `global` so porcelains (giant-task, giant-tui, …) can define
    /// their own `--config` without colliding with this one through
    /// trailing args. `overrides_with` lets a later `--config` from a
    /// wrapper script's outer invocation be replaced by a user's
    /// explicit one without clap complaining about duplicates.
    #[arg(long, overrides_with = "config")]
    pub config: Option<std::path::PathBuf>,

    /// Force a fresh build (bypass cache).
    #[arg(long, global = true)]
    pub fresh: bool,

    /// Enforce declared inputs/outputs by running each eligible target through
    /// the `giant-sandbox` helper (Linux only; ADR-0030). Off by default.
    #[arg(long, global = true)]
    pub sandbox: bool,

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

    /// Audit hermeticity: build every target sandboxed with the cache
    /// bypassed, so any undeclared input/output/network use fails. This is
    /// `build --sandbox --fresh` over all targets (Linux only; ADR-0030).
    Verify(verify::VerifyArgs),

    /// List targets that would rebuild given a set of changed files.
    /// Doesn't actually run anything.
    Affected(affected::AffectedArgs),

    /// Show what feeds a target's cache key - the first thing to reach
    /// for when "why did this rebuild?" comes up.
    Explain(explain::ExplainArgs),

    /// Replay the captured stdout/stderr from the last cached
    /// invocation of a target - answer "what did the build say?"
    /// without busting the cache.
    Logs(logs::LogsArgs),

    /// List targets, or show a target's dep tree.
    Graph(graph::GraphArgs),

    /// Clear the local cache.
    Clean(clean::CleanArgs),

    /// Persistent engine over stdio. Loads config and builds the graph
    /// once, then reads JSON commands on stdin and emits NDJSON events
    /// on stdout. The protocol porcelains (the TUI in particular) drive
    /// against. Refuses to run with stdout on a TTY - pipe it. See
    /// TDD-0014.
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

    // Build the clap Command from the derived Cli, then register any
    // `giant-<name>` binaries we find on PATH as additional
    // subcommands. They appear in `Commands:` in the regular help with
    // a one-line `about` extracted from the porcelain's own --help.
    // The typed `Cli` enum has no variant for them, so we detect a
    // porcelain hit on the matches and dispatch before falling through
    // to `from_arg_matches`.
    let porcelains = detect_porcelains();
    let want_help = is_help_invocation();
    let mut cmd = Cli::command();
    for (name, path) in &porcelains {
        let about = if want_help {
            porcelain_about(path).unwrap_or_default()
        } else {
            String::new()
        };
        // clap's Command::new takes `impl Into<Str>` where Str only
        // converts from `&'static str` - we have to leak the name so
        // it satisfies the bound. The leak is once per porcelain per
        // invocation; the strings live for the rest of process life.
        let name_static: &'static str = Box::leak(name.clone().into_boxed_str());
        cmd = cmd.subcommand(
            clap::Command::new(name_static)
                .about(about)
                .disable_help_flag(true) // pass --help through to the porcelain
                .trailing_var_arg(true)
                .arg(
                    clap::Arg::new("args")
                        .num_args(0..)
                        .allow_hyphen_values(true)
                        .value_parser(clap::value_parser!(OsString)),
                ),
        );
    }
    let matches = cmd.get_matches();

    // Route porcelain hits before the typed decode: the derived `Cli`
    // enum has no variant for a dynamically-registered subcommand, so
    // `from_arg_matches` would error.
    if let Some((sub, sub_matches)) = matches.subcommand()
        && porcelains.contains_key(sub)
    {
        let mut argv: Vec<OsString> = vec![OsString::from(sub)];
        if let Some(extra) = sub_matches.get_many::<OsString>("args") {
            argv.extend(extra.cloned());
        }
        return external::dispatch(argv);
    }

    let cli = Cli::from_arg_matches(&matches).expect("clap derive ensures shape");

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&cli.log));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let global = GlobalFlags {
        config: cli.config.clone(),
        fresh: cli.fresh,
        sandbox: cli.sandbox,
    };

    match cli.command {
        Commands::Build(args) => build::execute(args, &global).await,
        Commands::Test(args) => test::execute(args, &global).await,
        Commands::Verify(args) => verify::execute(args, &global).await,
        Commands::Affected(args) => affected::execute(args, &global).await,
        Commands::Explain(args) => explain::execute(args, &global).await,
        Commands::Logs(args) => logs::execute(args, &global).await,
        Commands::Graph(args) => graph::execute(args, &global).await,
        Commands::Clean(args) => clean::execute(args, &global).await,
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
    pub sandbox: bool,
}

/// Resolve the `--sandbox` flag into a policy (ADR-0030, TDD-0025). Returns
/// `None` when the flag is off (run normally). When on, finds the
/// `giant-sandbox` helper on PATH and errors loudly if it is absent or the
/// host is not Linux - never silently degrades to an unsandboxed run.
pub(crate) fn resolve_sandbox(
    enabled: bool,
) -> anyhow::Result<Option<crate::executor::SandboxPolicy>> {
    if !enabled {
        return Ok(None);
    }
    if !cfg!(target_os = "linux") {
        anyhow::bail!("--sandbox is only supported on Linux");
    }
    let helper = external::find_on_path("giant-sandbox").ok_or_else(|| {
        anyhow::anyhow!("--sandbox needs the `giant-sandbox` helper on PATH, but it was not found")
    })?;
    // v1 toolchain (ADR-0030 §3): the Nix store plus the standard system roots
    // that hold interpreters, shared libraries, and PATH binaries, read-only +
    // executable, filtered to those that exist. This keeps `--sandbox` usable
    // outside a devenv shell (where PATH points at /run/current-system/sw or
    // /usr) while enforcement still bites on the *workspace* - only declared
    // inputs are readable there. The toolchain-declared model lands later.
    let toolchain = [
        "/nix/store",
        "/run/current-system/sw",
        "/usr",
        "/bin",
        "/lib",
        "/lib64",
        "/etc",
    ]
    .into_iter()
    .map(std::path::PathBuf::from)
    .filter(|p| p.exists())
    .collect();
    Ok(Some(crate::executor::SandboxPolicy { helper, toolchain }))
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

/// True iff the user is asking for help - explicitly via `--help`,
/// `-h`, or `help`, or implicitly by running `giant` with no
/// subcommand (clap auto-prints usage in that case). We only extract
/// porcelain about-lines (which involves spawning a process per
/// porcelain) when this returns true. Stops at `--` so a flag named
/// `help` passed through to a porcelain doesn't trigger the
/// extraction.
fn is_help_invocation() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // No args at all → clap renders help.
    if args.is_empty() {
        return true;
    }
    // No positional that could be a subcommand → clap still renders
    // help. Treat global flags only (--config, --fresh, --log) as
    // not-a-subcommand.
    let mut iter = args.iter().take_while(|a| a.as_str() != "--");
    let mut has_subcommand = false;
    while let Some(a) = iter.next() {
        if a == "--help" || a == "-h" || a == "help" {
            return true;
        }
        if a.starts_with("--config") || a.starts_with("--log") {
            // These take a value - skip the next arg if it's not
            // joined with `=`.
            if !a.contains('=') {
                iter.next();
            }
            continue;
        }
        if a == "--fresh" || a.starts_with('-') {
            continue;
        }
        has_subcommand = true;
        break;
    }
    !has_subcommand
}

/// Run `<porcelain> --help` and pull out the about text - the first
/// non-empty, non-`Usage:` line of stdout. Clap convention puts the
/// about right at the top. Returns `None` if the porcelain failed to
/// run, didn't produce parseable output, or just didn't have an about.
fn porcelain_about(path: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new(path)
        .arg("--help")
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let stdout = String::from_utf8(output.stdout).ok()?;
    stdout
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("Usage:"))
        .map(|l| l.to_string())
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
    std::fs::metadata(path)
        .map(|m| m.is_file())
        .unwrap_or(false)
}
