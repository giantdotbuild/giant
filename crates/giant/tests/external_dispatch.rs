//! Porcelain dispatch end-to-end (ADR-0010 + ADR-0021):
//! - `giant <name>` for an unknown name → look up `giant-<name>` on PATH → exec.
//! - No such binary → consult the dispatch routing table and exec the
//!   binary it names (default `* -> giant-task`).
//! - A rule list that matches nothing → "no such subcommand".

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

    // A tiny "porcelain" that echoes its args. The exec'd binary
    // takes over giant's process, so its stdout becomes ours.
    let script = dir.path().join("giant-hello");
    std::fs::write(
        &script,
        "#!/bin/sh\necho hello-from-porcelain $#\nfor a in \"$@\"; do echo \"arg: $a\"; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    // Prepend the dir to PATH for the giant subprocess.
    let new_path = match std::env::var_os("PATH") {
        Some(p) => {
            let mut v = std::ffi::OsString::from(dir.path());
            v.push(":");
            v.push(&p);
            v
        }
        None => dir.path().as_os_str().to_os_string(),
    };

    let out = Command::new(giant_bin())
        .args(["hello", "alpha", "beta"])
        .env("PATH", new_path)
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
fn unknown_name_routes_to_giant_task_by_default() {
    // No `giant-<name>` binary and no `dispatch:` config → the default
    // route (`* -> giant-task`) carries the name + args to giant-task.
    let bindir = tempfile::tempdir().unwrap();
    write_exec(
        &bindir.path().join("giant-task"),
        "#!/bin/sh\necho task-got \"$@\"\n",
    );
    let ws = tempfile::tempdir().unwrap(); // no giant.yaml up the tree

    let out = Command::new(giant_bin())
        .args(["zzz-unknown", "alpha", "beta"])
        .current_dir(ws.path())
        .env("PATH", path_with(bindir.path()))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("task-got zzz-unknown alpha beta"),
        "giant-task should receive the name + args; got: {s}"
    );
}

#[test]
fn dispatch_table_routes_to_configured_binary() {
    let bindir = tempfile::tempdir().unwrap();
    write_exec(
        &bindir.path().join("giant-myrouter"),
        "#!/bin/sh\necho router-got \"$@\"\n",
    );
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(
        ws.path().join("giant.yaml"),
        "workspace: { name: t }\ndispatch:\n  unknown: \"giant-myrouter\"\n",
    )
    .unwrap();

    let out = Command::new(giant_bin())
        .args(["foo", "x"])
        .current_dir(ws.path())
        .env("PATH", path_with(bindir.path()))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("router-got foo x"), "got: {s}");
}

#[test]
fn routed_target_missing_errors() {
    // Default route is giant-task; with PATH empty it can't be found.
    let ws = tempfile::tempdir().unwrap();
    let out = Command::new(giant_bin())
        .args(["zzz-unknown"])
        .current_dir(ws.path())
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(!out.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("giant-task"),
        "expected the routed-binary error to name giant-task; got: {stderr}"
    );
}

#[test]
fn unmatched_dispatch_rule_errors_no_such_subcommand() {
    // A rule list with no `*` rule can miss → the helpful not-found error.
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(
        ws.path().join("giant.yaml"),
        "workspace: { name: t }\ndispatch:\n  unknown:\n    - { match: \"db:*\", to: \"giant-db\" }\n",
    )
    .unwrap();
    let out = Command::new(giant_bin())
        .args(["totally-unmatched"])
        .current_dir(ws.path())
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no such subcommand"), "got: {stderr}");
}

#[test]
fn builtin_subcommand_still_takes_precedence() {
    // `giant build --help` should hit the built-in, not try to find
    // `giant-build` on PATH (which might not even exist).
    let out = Command::new(giant_bin())
        .args(["build", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("Build targets") || s.contains("Usage:"),
        "expected built-in help output; got: {s}"
    );
}
