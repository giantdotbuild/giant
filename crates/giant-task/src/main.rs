//! `giant-task` - the task-runner porcelain.
//!
//! Dispatched automatically via `giant task <name>` (the core's
//! external-subcommand shim sees the absent built-in and execs us).
//! Standalone invocation as `giant-task <name>` also works.
//!
//! Communicates with the engine via subprocess: builds run as
//! `giant build <deps...>`, the user sees the normal renderer.
//! Task `command:` runs in the workspace root (or task `cwd:`) via
//! `sh -c`, stdio inherited.

mod completions;
mod config;
mod deps;
mod render;
mod runner;
mod schema;
mod services;
mod workspace;

use clap::{CommandFactory, Parser};
use std::ffi::OsString;
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(
    name = "giant-task",
    about = "Task-runner porcelain for Giant",
    version,
    disable_help_subcommand = true,
    arg_required_else_help = false
)]
pub(crate) struct Cli {
    /// Task name to run. Omit (or use `list`) to print the available
    /// tasks.
    #[arg(add = clap_complete::ArgValueCompleter::new(completions::complete_task_names))]
    name: Option<String>,

    /// Path to `giant.yaml`. Defaults to walking up from cwd.
    #[arg(long, value_name = "PATH")]
    config: Option<std::path::PathBuf>,

    /// Set a task argument: `--arg key=value`. Repeatable.
    #[arg(long = "arg", value_name = "KEY=VALUE")]
    args: Vec<String>,

    /// Print the list of tasks and exit.
    #[arg(long)]
    list: bool,

    /// Stream the full `giant build` output for dep-phase targets
    /// (cache hits, per-target log lines). Default is the compact
    /// one-line summary; failures always show their captured output.
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Emit a shell completion script for `giant-task` and exit. Pipe
    /// the output into the right place for your shell.
    #[arg(long, value_name = "SHELL", value_enum)]
    completions: Option<completions::ShellChoice>,

    /// Pass-through args go after `--` and are appended to the task's
    /// command line (`sh -c '<command>' -- <passthrough...>`).
    #[arg(last = true)]
    passthrough: Vec<OsString>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    // Dynamic completion intercept - clap_complete sees the COMPLETE
    // env var, prints candidates, exits before we get to normal parse.
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();

    let cli = Cli::parse();
    match dispatch(cli).await {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("giant-task: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(cli: Cli) -> anyhow::Result<u8> {
    // Static completion emission takes precedence over every other
    // mode - no config load, no validation. Pipe into your shell.
    if let Some(shell) = cli.completions {
        completions::emit(shell);
        return Ok(0);
    }

    let cfg_path = match cli.config {
        // Canonicalise the user-supplied path so `--config giant.yaml`
        // (relative, no parent dir) still resolves to a real workspace
        // root. Falls back to the raw path if canonicalize fails (e.g.
        // the file doesn't exist; we want the load error, not a
        // confusing canonicalise error).
        Some(p) => p.canonicalize().unwrap_or(p),
        None => workspace::find_config(&std::env::current_dir()?)?,
    };
    let cfg = config::TaskConfig::load(&cfg_path)?;

    if cli.list || cli.name.as_deref() == Some("list") {
        render::list(&cfg);
        return Ok(0);
    }

    let name = match cli.name {
        Some(n) => n,
        None => {
            render::list(&cfg);
            return Ok(0);
        }
    };

    let workspace_root = cfg_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?
        .to_path_buf();

    runner::run(
        &cfg,
        &name,
        &cli.args,
        &cli.passthrough,
        &workspace_root,
        cli.verbose,
    )
    .await
}
