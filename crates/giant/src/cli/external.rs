//! Porcelain dispatch - when `giant <name>` isn't a built-in
//! subcommand, look for `giant-<name>` on PATH and exec it (ADR-0010).
//! If there's no such binary, consult the configurable dispatch routing
//! table and exec whatever binary it names (ADR-0021); the default route
//! is `* -> giant-task`, so bare-name tasks work out of the box. Core
//! never learns what a task is - it just routes.
//!
//! On unix we use `exec()` so the porcelain replaces our process -
//! signals (Ctrl-C, SIGTERM) go directly to the porcelain, no parent
//! to translate. On non-unix we fall back to spawn + wait + propagate
//! exit code.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Entry point. `args[0]` is the subcommand name (with no `giant-`
/// prefix); `args[1..]` is whatever the user passed after it.
pub fn dispatch(args: Vec<OsString>) -> anyhow::Result<()> {
    let Some((name_os, rest)) = args.split_first() else {
        anyhow::bail!("internal: external subcommand with empty args");
    };
    let name = name_os.to_string_lossy();

    // 1. An explicit `giant-<name>` binary wins (ADR-0010).
    let prog = format!("giant-{name}");
    if let Some(path) = find_on_path(&prog) {
        return exec_or_spawn(&path, rest);
    }

    // 2. Otherwise consult the dispatch routing table (ADR-0021). The
    //    routed binary is invoked as `<to> <name> <rest>` - the target
    //    (giant-task by default) decides what `<name>` means and owns the
    //    "no such task" error. Routing degrades to the default table when
    //    there's no config, so it always resolves unless a user's rule
    //    list deliberately excludes the name.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let table = crate::config::load_dispatch(&cwd);
    let Some(to) = table.route(&name) else {
        anyhow::bail!(
            "no such subcommand '{name}': no built-in, no '{prog}' on PATH, \
             and no dispatch rule matches it (see the `dispatch:` section of \
             giant.yaml, ADR-0021)."
        );
    };
    let Some(to_path) = find_on_path(to) else {
        anyhow::bail!(
            "subcommand '{name}' routes to '{to}' per the dispatch table, but \
             '{to}' was not found on PATH.\n\
             hint: install '{to}' (the default route is `giant-task`)."
        );
    };

    // Hand the routed binary `<name> <rest...>`.
    let mut routed: Vec<OsString> = Vec::with_capacity(rest.len() + 1);
    routed.push(name_os.clone());
    routed.extend_from_slice(rest);
    exec_or_spawn(&to_path, &routed)
}

/// Look up `name` in each `PATH` entry. Returns the first executable
/// match. Doesn't follow symlinks specially; trusts the OS to handle
/// them at exec time.
pub(crate) fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    p.metadata()
        .map(|m| m.is_file() && (m.mode() & 0o111) != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    // On Windows the .exe extension carries the executable signal.
    p.is_file()
}

#[cfg(unix)]
fn exec_or_spawn(prog: &Path, args: &[OsString]) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    // `exec` replaces this process; only returns on failure.
    let err = Command::new(prog).args(args).exec();
    anyhow::bail!("failed to exec {}: {err}", prog.display());
}

#[cfg(not(unix))]
fn exec_or_spawn(prog: &Path, args: &[OsString]) -> anyhow::Result<()> {
    let status = Command::new(prog).args(args).status()?;
    let code = status.code().unwrap_or(1);
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn is_executable_says_yes_for_mode_755() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ok");
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_executable(&p));
    }

    #[cfg(unix)]
    #[test]
    fn is_executable_says_no_for_mode_644() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nope");
        std::fs::write(&p, "data\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!is_executable(&p));
    }

    #[test]
    fn is_executable_says_no_for_missing_file() {
        assert!(!is_executable(Path::new(
            "/tmp/definitely-does-not-exist-12345"
        )));
    }

    // PATH-dispatch coverage lives in tests/external_dispatch.rs (it
    // needs a real subprocess so `exec` is exercised end-to-end).
}
