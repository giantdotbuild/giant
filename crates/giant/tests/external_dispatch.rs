//! Porcelain dispatch end-to-end (ADR-0010):
//! - `giant <name>` for an unknown name → look up `giant-<name>` on PATH → exec.
//! - When the binary is missing → exit non-zero with a helpful error
//!   that names the expected binary.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn giant_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_giant"))
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
fn missing_porcelain_errors_with_hint() {
    let out = Command::new(giant_bin())
        // PATH set to empty so giant-noexist-9000 can't possibly be found.
        .env("PATH", "")
        .args(["noexist-9000"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected non-zero exit; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no such subcommand 'noexist-9000'"),
        "expected naming of subcommand; got: {stderr}"
    );
    assert!(
        stderr.contains("giant-noexist-9000"),
        "expected naming of expected binary; got: {stderr}"
    );
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
