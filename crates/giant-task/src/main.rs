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
mod watch;
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
    // We own `--help`: `giant <task> --help` must reach our per-task help,
    // not clap's. clap's general help is printed by hand when there's no
    // task (see `dispatch`).
    disable_help_flag = true,
    arg_required_else_help = false
)]
pub(crate) struct Cli {
    /// The task name followed by everything that belongs to the task: its
    /// positional arguments, `--arg name=value` overrides, and `--help`.
    /// Flag-like values (e.g. `--release`) pass straight through to the
    /// task. Giant-task's own flags (`--watch`, `--config`, …) come
    /// BEFORE the task name.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        add = clap_complete::ArgValueCompleter::new(completions::complete_task_names)
    )]
    name_and_args: Vec<OsString>,

    /// Path to `giant.yaml`. Defaults to walking up from cwd.
    #[arg(long, value_name = "PATH")]
    config: Option<std::path::PathBuf>,

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

    /// Re-run the task on file changes. Watches the task's `inputs:`
    /// patterns (if declared) or the workspace root (excluding the
    /// cache and `.giant/` state) otherwise. Ctrl-C to exit.
    #[arg(long)]
    watch: bool,

    /// Quiet window in ms for the watcher (events that arrive this
    /// close together are coalesced into one rebuild). Default 100ms.
    #[arg(long, default_value_t = 100, requires = "watch")]
    quiet_ms: u64,

    /// Max delay in ms for the watcher: flush a batch this long after
    /// the FIRST event in it, even if events keep streaming. Default
    /// 500ms.
    #[arg(long, default_value_t = 500, requires = "watch")]
    max_delay_ms: u64,
}

/// The post-name tokens, split into the task's positional args, `--arg`
/// overrides, and a `--help` request.
#[derive(Default)]
struct TaskArgs {
    positionals: Vec<String>,
    arg_kvs: Vec<String>,
    want_help: bool,
}

/// Split the raw tokens after the task name. `--help`/`-h` requests the
/// task signature, `--arg name=value` sets a named arg, and everything
/// else - including flag-like values such as `--release`, which bind to
/// the task's `variadic` arg and reach the command as `$@` - is a
/// positional. (A literal `--` is consumed by the arg parser, so to
/// forward arbitrary args, declare a `variadic` arg.)
fn parse_task_args(rest: &[OsString]) -> anyhow::Result<TaskArgs> {
    let mut out = TaskArgs::default();
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        let s = tok.to_string_lossy();
        match s.as_ref() {
            "--help" | "-h" => out.want_help = true,
            "--arg" => {
                let v = it
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--arg needs a name=value"))?;
                out.arg_kvs.push(v.to_string_lossy().into_owned());
            }
            _ if let Some(kv) = s.strip_prefix("--arg=") => {
                out.arg_kvs.push(kv.to_string());
            }
            other => out.positionals.push(other.to_string()),
        }
    }
    Ok(out)
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

    // Split the task name off the front; the rest are the task's args.
    let (name, rest): (Option<String>, &[OsString]) = match cli.name_and_args.split_first() {
        Some((n, r)) => (Some(n.to_string_lossy().into_owned()), r),
        None => (None, &[]),
    };

    // `giant-task --help` / `-h` with no task → giant-task's own help.
    // Handled before config load so it works outside a workspace.
    if matches!(name.as_deref(), Some("--help") | Some("-h")) {
        Cli::command().print_help()?;
        println!();
        return Ok(0);
    }

    // Precedence: --config flag → GIANT_CONFIG env var → walk up from cwd
    // looking for giant.yaml / giant.json.
    let cfg_path = match cli
        .config
        .or_else(|| std::env::var_os("GIANT_CONFIG").map(std::path::PathBuf::from))
    {
        // Canonicalise so a relative path still resolves to a real
        // workspace root. Falls back to the raw path if canonicalize
        // fails (e.g. file doesn't exist; we want the load error, not
        // a confusing canonicalise error).
        Some(p) => p.canonicalize().unwrap_or(p),
        None => workspace::find_config(&std::env::current_dir()?)?,
    };
    let cfg = config::TaskConfig::load(&cfg_path)?;

    // `--list`, the `list` name, or no task name → print the task list.
    if cli.list || matches!(name.as_deref(), None | Some("list")) {
        render::list(&cfg);
        return Ok(0);
    }
    let name = name.expect("None handled above");

    let parsed = parse_task_args(rest)?;

    // Per-task help: `giant <task> --help` prints the task's signature.
    if parsed.want_help {
        let spec = cfg
            .tasks
            .get(&name)
            .ok_or_else(|| anyhow::anyhow!("no task named '{name}' - try `giant task --list`"))?;
        render::task_help(&name, spec);
        return Ok(0);
    }

    let workspace_root = cfg_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?
        .to_path_buf();

    if cli.watch {
        watch::loop_forever(
            &cfg,
            &name,
            &parsed.positionals,
            &parsed.arg_kvs,
            &workspace_root,
            cli.verbose,
            std::time::Duration::from_millis(cli.quiet_ms),
            std::time::Duration::from_millis(cli.max_delay_ms),
        )
        .await
    } else {
        runner::run(
            &cfg,
            &name,
            &parsed.positionals,
            &parsed.arg_kvs,
            &workspace_root,
            cli.verbose,
        )
        .await
    }
}
