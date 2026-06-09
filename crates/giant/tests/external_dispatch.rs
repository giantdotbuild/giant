//! Porcelain dispatch end-to-end:
//! - `giant <name>` for a non-built-in → look up `giant-<name>` (beside giant,
//!   then on PATH) → exec.
//! - No such binary → "no such subcommand" error. There is no catch-all; an
//!   unknown name is never silently handed to a task runner.

#![cfg(unix)]

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn giant_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_giant"))
}

/// Prepend `dir` to PATH for a child `giant` invocation.
fn path_with(dir: &Path) -> OsString {
    match std::env::var_os("PATH") {
        Some(p) => {
            let mut v = OsString::from(dir);
            v.push(":");
            v.push(&p);
            v
        }
        None => dir.as_os_str().to_os_string(),
    }
}

/// Write an executable shell script.
fn write_exec(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn dispatches_to_giant_prefixed_binary_on_path() {
    let dir = tempfile::tempdir().unwrap();

    // A tiny "porcelain" that echoes its args. The exec'd binary takes over
    // giant's process, so its stdout becomes ours.
    write_exec(
        &dir.path().join("giant-hello"),
        "#!/bin/sh\necho hello-from-porcelain $#\nfor a in \"$@\"; do echo \"arg: $a\"; done\n",
    );

    let out = Command::new(giant_bin())
        .args(["hello", "alpha", "beta"])
        .env("PATH", path_with(dir.path()))
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "dispatch failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("hello-from-porcelain 2"), "argc wrong; got: {s}");
    assert!(s.contains("arg: alpha"), "missing first arg; got: {s}");
    assert!(s.contains("arg: beta"), "missing second arg; got: {s}");
}

#[test]
fn unknown_name_errors_and_never_falls_back_to_a_task_runner() {
    // Even with `giant-task` right there on PATH, an unknown name must NOT be
    // routed to it: it errors with a helpful "no such subcommand".
    let bindir = tempfile::tempdir().unwrap();
    write_exec(
        &bindir.path().join("giant-task"),
        "#!/bin/sh\necho task-got \"$@\"\n",
    );
    let ws = tempfile::tempdir().unwrap();

    let out = Command::new(giant_bin())
        .args(["zzz-unknown", "alpha"])
        .current_dir(ws.path())
        .env("PATH", path_with(bindir.path()))
        .output()
        .unwrap();

    assert!(!out.status.success(), "unknown name must error");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("task-got"),
        "unknown name must not be handed to giant-task; got stdout: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no such subcommand"),
        "expected a not-found error; got: {stderr}"
    );
    // The error points task-like names at the explicit front door.
    assert!(
        stderr.contains("giant task"),
        "error should suggest `giant task`; got: {stderr}"
    );
}

#[test]
fn builtin_subcommand_still_takes_precedence() {
    // `giant completions --help` must hit the built-in even if a rogue
    // `giant-completions` is on PATH.
    let bindir = tempfile::tempdir().unwrap();
    write_exec(
        &bindir.path().join("giant-completions"),
        "#!/bin/sh\necho rogue-completions\n",
    );

    let out = Command::new(giant_bin())
        .args(["completions", "--help"])
        .env("PATH", path_with(bindir.path()))
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        !s.contains("rogue-completions"),
        "built-in must win over a PATH binary of the same name; got: {s}"
    );
    assert!(
        s.contains("Usage:"),
        "expected built-in help output; got: {s}"
    );
}
