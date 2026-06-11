//! CLI surface - parses args, dispatches to subcommand handlers.
//!
//! Built-in subcommands are matched first. Unknown subcommands fall
//! through to porcelain dispatch: `giant <name>` looks for `giant-<name>`
//! on PATH and execs it (the git/cargo/kubectl model).

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

mod completions;
mod external;
pub mod prep;
pub mod session;
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

    /// Log filter (RUST_LOG syntax). Defaults to errors only - pass
    /// `--log warn` (or set `RUST_LOG=giant=warn`) when debugging.
    ///
    /// Not `global` for the same reason as `--config`: build/test/verify and the
    /// other porcelains own their flags now, so a global here would
    /// swallow a porcelain's `--fresh` / `--sandbox` before dispatch.
    #[arg(long, default_value = "error")]
    pub log: String,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Persistent engine over stdio. Loads config and builds the graph
    /// once, then reads JSON commands on stdin and emits NDJSON events
    /// on stdout. The protocol porcelains (the TUI in particular) drive
    /// against. Refuses to run with stdout on a TTY - pipe it.
    Session(session::SessionArgs),

    /// Generate a shell completion script for bash / zsh / fish /
    /// powershell / elvish / nushell. Pipe the output into the right
    /// place for your shell.
    Completions(completions::CompletionsArgs),

    /// Unknown subcommand → dispatch to `giant-<name>` on PATH if
    /// available, else error with a helpful hint.
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
    // About-lines only show in help, and scraping each means spawning
    // `<porcelain> --help`; do it in one bounded concurrent batch so help stays
    // fast no matter how many porcelains are installed.
    let abouts = if is_help_invocation() {
        porcelain_abouts(&porcelains)
    } else {
        BTreeMap::new()
    };
    let mut cmd = Cli::command();
    for name in porcelains.keys() {
        let about = abouts.get(name).cloned().unwrap_or_default();
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
    // Stderr, always: `giant session` owns stdout for the NDJSON event
    // stream, and a log line on stdout would corrupt it.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let global = GlobalFlags {
        config: cli.config.clone(),
    };

    match cli.command {
        Commands::Session(args) => session::execute(args, &global).await,
        Commands::Completions(args) => completions::execute(args),
        Commands::External(args) => external::dispatch(args),
    }
}

/// Subset of CLI args the remaining built-in subcommands consult.
#[derive(Debug, Clone)]
pub struct GlobalFlags {
    pub config: Option<std::path::PathBuf>,
}

/// Resolve the `--sandbox` flag into a policy. Returns
/// `None` when the flag is off (run normally). When on, finds the
/// `giant-sandbox` helper on PATH and errors loudly if it is absent or the
/// host is not Linux - never silently degrades to an unsandboxed run.
pub fn resolve_sandbox(
    enabled: bool,
    cfg: &crate::config::SandboxConfig,
    workspace_root: &std::path::Path,
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

    // Generic FHS roots (read-only + executable). Anything scheme-specific - a
    // Nix `/nix/store`, an asdf `~/.asdf`, a vendored `bin/` - is added by the
    // workspace's `sandbox.roots`. Core assumes no toolchain manager.
    // Filtered to those present; enforcement still bites on the workspace.
    const DEFAULT_ROOTS: &[&str] = &["/usr", "/bin", "/lib", "/lib64", "/etc"];
    let toolchain = DEFAULT_ROOTS
        .iter()
        .map(|s| (*s).to_string())
        .chain(cfg.roots.iter().cloned())
        .map(|s| resolve_sb_path(&s, workspace_root))
        .filter(|p| p.exists())
        .collect();

    // The standard pseudo-devices, writable. birdcage has no mount namespace,
    // so there is no synthetic /dev; without these, `> /dev/null`, `/dev/urandom`,
    // etc. - which almost every real command touches - fail. Universal on Linux,
    // not scheme-specific. Granted read-write (they are harmless sinks/sources).
    const DEFAULT_DEV: &[&str] = &[
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/random",
        "/dev/urandom",
        "/dev/tty",
    ];

    // Extra writable paths (e.g. a build cache outside the workspace) plus the
    // pseudo-devices. Must already exist - a sandbox rule needs a real path -
    // so skip missing ones.
    let rw = DEFAULT_DEV
        .iter()
        .map(|s| (*s).to_string())
        .chain(cfg.rw.iter().cloned())
        .map(|s| resolve_sb_path(&s, workspace_root))
        .filter(|p| p.exists())
        .collect();

    // Generic env allowlist. Scheme-specific families (e.g. `NIX_*`) come from
    // `sandbox.env`. A trailing `*` is a prefix; birdcage grants exact names,
    // so we expand against the ambient environment here.
    const DEFAULT_ENV: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "SHELL",
        "TERM",
        "TZ",
        "TMPDIR",
        "LANG",
        "LC_*",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "PKG_CONFIG_PATH",
        "LD_LIBRARY_PATH",
        "GIANT_*",
    ];
    let env = expand_env_patterns(
        DEFAULT_ENV
            .iter()
            .map(|s| (*s).to_string())
            .chain(cfg.env.iter().cloned()),
    );

    Ok(Some(crate::executor::SandboxPolicy {
        helper,
        toolchain,
        rw,
        env,
    }))
}

/// Resolve a sandbox path entry: `~/...` against `$HOME`, an absolute path
/// as-is, and anything else (a workspace-relative entry like `.devenv/state/go`)
/// against the workspace root - so configs stay portable across machines.
fn resolve_sb_path(p: &str, workspace_root: &std::path::Path) -> std::path::PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        return match std::env::var_os("HOME") {
            Some(home) => std::path::PathBuf::from(home).join(rest),
            None => std::path::PathBuf::from(p),
        };
    }
    let path = std::path::Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace_root.join(path)
    }
}

/// Resolve env allowlist patterns into concrete names. A trailing `*` matches
/// ambient variable names by prefix; everything else is a literal name.
fn expand_env_patterns(patterns: impl IntoIterator<Item = String>) -> Vec<String> {
    let ambient: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
    let mut names = std::collections::BTreeSet::new();
    for pat in patterns {
        match pat.strip_suffix('*') {
            Some(prefix) => names.extend(ambient.iter().filter(|k| k.starts_with(prefix)).cloned()),
            None => {
                names.insert(pat);
            }
        }
    }
    names.into_iter().collect()
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
const BUILTIN_SUBCOMMANDS: &[&str] = &["session", "completions"];

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
    // No positional that could be a subcommand → clap still renders help. Treat
    // the giant-level flags (--config, --log) as not-a-subcommand.
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
        if a.starts_with('-') {
            continue;
        }
        has_subcommand = true;
        break;
    }
    !has_subcommand
}

/// Scrape the about-line of every porcelain by running `<porcelain> --help`.
/// All children are spawned at once, then collected under a shared deadline: a
/// slow or wedged porcelain can't slow help past the budget - it just shows up
/// without a description. The about is the first non-empty, non-`Usage:` line of
/// stdout (clap convention puts it right at the top).
fn porcelain_abouts(porcelains: &BTreeMap<String, PathBuf>) -> BTreeMap<String, String> {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    // Generous next to a well-behaved `--help` (a few ms), tight enough that help
    // stays snappy even when a porcelain does real work on startup.
    const BUDGET: Duration = Duration::from_millis(100);

    let mut running: Vec<(String, std::process::Child)> = porcelains
        .iter()
        .filter_map(|(name, path)| {
            let child = std::process::Command::new(path)
                .arg("--help")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            Some((name.clone(), child))
        })
        .collect();

    // Wait until all have exited or the budget runs out, whichever is first.
    let deadline = Instant::now() + BUDGET;
    while Instant::now() < deadline
        && running
            .iter_mut()
            .any(|(_, c)| matches!(c.try_wait(), Ok(None)))
    {
        std::thread::sleep(Duration::from_millis(2));
    }

    running
        .into_iter()
        .filter_map(|(name, mut child)| {
            // Still running at the deadline - kill it and show the bare name.
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            let mut stdout = String::new();
            child.stdout.take()?.read_to_string(&mut stdout).ok()?;
            let about = stdout
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with("Usage:"))?;
            Some((name, about.to_string()))
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::resolve_sb_path;
    use std::path::Path;

    #[test]
    fn sb_path_absolute_is_unchanged() {
        let ws = Path::new("/ws");
        assert_eq!(resolve_sb_path("/nix/store", ws), Path::new("/nix/store"));
    }

    #[test]
    fn sb_path_relative_resolves_against_workspace() {
        let ws = Path::new("/ws/proj");
        assert_eq!(
            resolve_sb_path(".devenv/state/go", ws),
            Path::new("/ws/proj/.devenv/state/go")
        );
    }

    #[test]
    fn sb_path_tilde_resolves_against_home() {
        // SAFETY: single-threaded test; we set HOME for the duration.
        unsafe { std::env::set_var("HOME", "/home/u") };
        let ws = Path::new("/ws");
        assert_eq!(
            resolve_sb_path("~/.cache/go", ws),
            Path::new("/home/u/.cache/go")
        );
    }
}
