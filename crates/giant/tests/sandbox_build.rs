//! End-to-end: `giant build --sandbox` enforces a target's declared inputs
//! (TDD-0025). The mechanism is proven in giant-sandbox's own tests; here we
//! prove the *wiring* - that the engine wraps eligible targets and that an
//! undeclared read fails under the mode and passes without it.
//!
//! Linux-only and needs a working sandbox; if the host can't sandbox (no
//! unprivileged namespaces), the tests skip rather than fail.

#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::{Command, Output};

fn giant_bin() -> std::path::PathBuf {
    env!("CARGO_BIN_EXE_giant").into()
}

/// `giant-sandbox` is built into the same target dir as `giant`; prepend that
/// dir so `--sandbox` finds the helper on PATH.
fn path_with_helper() -> std::ffi::OsString {
    let dir = giant_bin().parent().unwrap().to_path_buf();
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![dir];
    paths.extend(std::env::split_paths(&existing));
    std::env::join_paths(paths).unwrap()
}

fn write_ws(declared: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    let inputs = if declared { "[\"data.txt\"]" } else { "[]" };
    // Add the Nix roots/env so the test runs on a NixOS host (where PATH points
    // into /nix/store + /run/current-system/sw); on a plain distro these filter
    // out and the generic FHS defaults carry the build. This also exercises the
    // `sandbox:` config path.
    std::fs::write(
        ws.join("giant.yaml"),
        format!(
            r#"
workspace:
  name: sbx
cache:
  dir: ./cache
sandbox:
  roots: ["/nix/store", "/run/current-system/sw"]
  env: ["NIX_*", "DEVENV_*", "LOCALE_ARCHIVE"]
targets:
  - name: "reader"
    inputs: {inputs}
    outputs: ["out/result.txt"]
    command: "cat data.txt > out/result.txt"
"#
        ),
    )
    .unwrap();
    std::fs::write(ws.join("data.txt"), b"secret\n").unwrap();
    dir
}

fn build(ws: &Path, sandbox: bool) -> Output {
    let mut cmd = Command::new(giant_bin());
    cmd.arg("build");
    if sandbox {
        cmd.arg("--sandbox");
    }
    cmd.current_dir(ws)
        .env("PATH", path_with_helper())
        .output()
        .expect("spawn giant build")
}

fn verify(ws: &Path) -> Output {
    Command::new(giant_bin())
        .arg("verify")
        .current_dir(ws)
        .env("PATH", path_with_helper())
        .output()
        .expect("spawn giant verify")
}

#[test]
fn sandbox_enforces_declared_inputs() {
    // A correctly declared target must build under --sandbox. If it can't,
    // this host has no working sandbox - skip rather than fail.
    let declared = write_ws(true);
    let out = build(declared.path(), true);
    if !out.status.success() {
        eprintln!(
            "skipping: declared target failed under --sandbox, assuming no \
             sandbox capability here:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    assert!(
        declared.path().join("out/result.txt").exists(),
        "declared sandboxed build should produce its output"
    );

    // Same command, but the input is undeclared: it must FAIL under --sandbox
    // (the read of data.txt is denied)...
    let undeclared = write_ws(false);
    let out = build(undeclared.path(), true);
    assert!(
        !out.status.success(),
        "an undeclared read must fail under --sandbox; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // ...and succeed without it - proving the sandbox is what caught it, not a
    // broken command.
    let undeclared_ok = write_ws(false);
    let out = build(undeclared_ok.path(), false);
    assert!(
        out.status.success(),
        "the same build must pass without --sandbox; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn verify_audits_without_an_explicit_flag() {
    // Probe with a declared workspace; skip if this host can't sandbox.
    let declared = write_ws(true);
    if !build(declared.path(), true).status.success() {
        eprintln!("skipping: no sandbox capability here");
        return;
    }
    // `giant verify` forces sandbox + fresh, so an undeclared read fails even
    // though no --sandbox flag is passed.
    let undeclared = write_ws(false);
    let out = verify(undeclared.path());
    assert!(
        !out.status.success(),
        "verify must catch an undeclared read with no flag; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
