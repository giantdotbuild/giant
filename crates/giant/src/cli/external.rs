//! Porcelain dispatch - when `giant <name>` isn't a built-in
//! subcommand, look for a `giant-<name>` binary (beside the giant
//! binary first, then on PATH) and exec it. A name with no matching
//! binary is an error; there is no catch-all that hands the name off
//! to a default runner.
//!
//! On unix we use `exec()` so the porcelain replaces our process,
//! letting signals (Ctrl-C, SIGTERM) reach it directly with no parent
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

    // A `giant-<name>` binary handles it (ADR-0010, ADR-0035). Look beside the
    // giant binary first - the suite ships its porcelains in the same directory
    // - then fall back to PATH. There is no catch-all: an unknown name errors
    // (ADR-0035) instead of being quietly handed to a task runner.
    let prog = format!("giant-{name}");
    if let Some(path) = find_sibling(&prog).or_else(|| find_on_path(&prog)) {
        return exec_or_spawn(&path, rest);
    }

    anyhow::bail!(
        "no such subcommand '{name}': not a built-in and no '{prog}' found beside \
         giant or on PATH.\n\
         hint: to run a task named '{name}', use `giant task {name}`."
    )
}

/// Look for `name` next to the running `giant` binary. The first-party
/// porcelains install alongside it (the giant-suite package, or `target/<profile>`
/// in a dev tree), so this resolves them without relying on PATH order.
fn find_sibling(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join(name);
    is_executable(&candidate).then_some(candidate)
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
