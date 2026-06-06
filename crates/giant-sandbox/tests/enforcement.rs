//! End-to-end tests for giant-sandbox (TDD-0025).
//!
//! Two groups:
//! - **Setup-failure** tests run anywhere: a bad/missing spec must exit 125,
//!   the reserved code that lets the engine tell "could not sandbox" from "the
//!   build failed".
//! - **Enforcement** tests need working unprivileged namespaces / Landlock. If
//!   the sandbox cannot be set up on this host, they skip rather than fail.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_giant-sandbox");
const SETUP_FAILURE: i32 = 125;

fn run(spec: &serde_json::Value, dir: &Path, argv: &[&Path]) -> Output {
    let spec_path = dir.join("spec.json");
    fs::write(&spec_path, serde_json::to_vec(spec).unwrap()).unwrap();
    Command::new(BIN)
        .arg("run")
        .arg("--spec")
        .arg(&spec_path)
        .arg("--")
        .args(argv)
        .output()
        .expect("spawn giant-sandbox")
}

// --- setup-failure path (platform-independent) ------------------------------

#[test]
fn missing_spec_file_is_setup_failure() {
    let out = Command::new(BIN)
        .args(["run", "--spec", "/nonexistent/spec.json", "--", "/bin/true"])
        .output()
        .expect("spawn giant-sandbox");
    assert_eq!(out.status.code(), Some(SETUP_FAILURE));
}

#[test]
fn unknown_schema_is_setup_failure() {
    let dir = tempfile::tempdir().unwrap();
    let spec = serde_json::json!({ "schema": 999, "cwd": "/" });
    let out = run(&spec, dir.path(), &[Path::new("/bin/true")]);
    assert_eq!(out.status.code(), Some(SETUP_FAILURE));
}

// --- enforcement path (Linux, needs a working sandbox) ----------------------

/// Standard read+execute roots that hold the interpreter, shared libraries, and
/// coreutils across NixOS and conventional distros. The enforcement variable in
/// these tests is the *data file*, never the toolchain.
fn toolchain() -> Vec<PathBuf> {
    [
        "/nix/store",
        "/run/current-system/sw",
        "/usr",
        "/bin",
        "/lib",
        "/lib64",
        "/etc",
    ]
    .iter()
    .map(PathBuf::from)
    .filter(|p| p.exists())
    .collect()
}

/// Resolve a binary to an absolute path so the child needs no PATH lookup.
fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(name))
            .find(|p| p.exists())
    })
}

/// Probe whether this host can actually sandbox. Runs `true` with only the
/// toolchain granted; a 125 (setup failure) means namespaces/Landlock are
/// unavailable here, so enforcement tests should skip.
fn sandbox_available(dir: &Path) -> bool {
    let Some(true_bin) = which("true") else {
        return false;
    };
    let spec = serde_json::json!({
        "schema": 1, "cwd": "/", "ro": [], "rw": [],
        "toolchain": toolchain(), "network": false,
    });
    let out = run(&spec, dir, &[&true_bin]);
    out.status.success()
}

#[test]
fn undeclared_read_is_denied_declared_read_is_allowed() {
    let dir = tempfile::tempdir().unwrap();
    if !sandbox_available(dir.path()) {
        eprintln!("skipping: no working sandbox on this host");
        return;
    }
    let Some(cat) = which("cat") else {
        eprintln!("skipping: no `cat` on PATH");
        return;
    };

    // A data file in a location the toolchain does not cover.
    let secret = dir.path().join("secret.txt");
    fs::write(&secret, b"classified\n").unwrap();

    // Undeclared: secret.txt is not in `ro` -> the read must fail.
    let denied = serde_json::json!({
        "schema": 1, "cwd": "/", "ro": [], "rw": [],
        "toolchain": toolchain(), "network": false,
    });
    let out = run(&denied, dir.path(), &[&cat, &secret]);
    assert!(
        !out.status.success(),
        "reading an undeclared file should fail under the sandbox"
    );
    assert_ne!(
        out.status.code(),
        Some(SETUP_FAILURE),
        "this is a build failure (denied read), not a sandbox setup failure"
    );

    // Declared: secret.txt in `ro` -> the read succeeds.
    let allowed = serde_json::json!({
        "schema": 1, "cwd": "/", "ro": [secret], "rw": [],
        "toolchain": toolchain(), "network": false,
    });
    let out = run(&allowed, dir.path(), &[&cat, &secret]);
    assert!(
        out.status.success(),
        "reading a declared input should succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"classified\n");
}

#[test]
fn env_allowlist_scrubs_undeclared_vars() {
    let dir = tempfile::tempdir().unwrap();
    if !sandbox_available(dir.path()) {
        eprintln!("skipping: no working sandbox on this host");
        return;
    }
    let Some(sh) = which("sh") else {
        eprintln!("skipping: no `sh` on PATH");
        return;
    };

    // PATH is allowed (sh needs it); FOO is set in the parent but not listed,
    // so the child must not see it.
    let spec = serde_json::json!({
        "schema": 1, "cwd": "/", "ro": [], "rw": [],
        "toolchain": toolchain(), "env": ["PATH"], "network": false,
    });
    let spec_path = dir.path().join("spec.json");
    std::fs::write(&spec_path, serde_json::to_vec(&spec).unwrap()).unwrap();

    let out = Command::new(BIN)
        .arg("run")
        .arg("--spec")
        .arg(&spec_path)
        .arg("--")
        .arg(&sh)
        .arg("-c")
        .arg(r#"printf %s "${FOO:-MISSING}""#)
        .env("FOO", "secret")
        .output()
        .expect("spawn giant-sandbox");

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.stdout, b"MISSING",
        "an env var outside the allowlist must be scrubbed"
    );
}
