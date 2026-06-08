//! `giant-logs` end to end. The porcelain spawns a `giant session`, so the
//! tests point `GIANT_BIN` at the sibling `giant` binary in the same target dir.

use std::process::Command;

fn logs_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_giant-logs"))
}

/// Sibling `giant` binary. Cargo doesn't expose `CARGO_BIN_EXE_giant` to another
/// package's tests, so derive it from our own path (same target dir).
fn giant_bin() -> std::path::PathBuf {
    let mut p = logs_bin();
    p.set_file_name("giant");
    p
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("giant.yaml"),
        r#"
workspace: { name: logs }
cache: { dir: ./cache }
targets:
  - name: "a"
    command: "echo hello-stdout; echo oops-stderr 1>&2; touch marker"
    outputs: ["marker"]
"#,
    )
    .unwrap();
    dir
}

fn build(ws: &std::path::Path) {
    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn logs(ws: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(logs_bin())
        .args(args)
        .env("GIANT_BIN", giant_bin())
        .current_dir(ws)
        .output()
        .unwrap()
}

#[test]
fn replays_stdout_and_stderr_to_their_streams() {
    let dir = workspace();
    let ws = dir.path();
    build(ws);

    let out = logs(ws, &["//:a"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("hello-stdout"),
        "stdout missing the captured stdout line"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("oops-stderr"),
        "stderr missing the captured stderr line"
    );
}

#[test]
fn stdout_only_suppresses_stderr() {
    let dir = workspace();
    let ws = dir.path();
    build(ws);

    let out = logs(ws, &["//:a", "--stdout-only"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("hello-stdout"));
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("oops-stderr"),
        "--stdout-only should not emit stderr"
    );
}

#[test]
fn merged_folds_stderr_into_stdout() {
    let dir = workspace();
    let ws = dir.path();
    build(ws);

    let out = logs(ws, &["//:a", "--merged"]);
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("hello-stdout"),
        "merged stdout missing stdout line"
    );
    assert!(
        s.contains("oops-stderr"),
        "merged stdout missing stderr line"
    );
}
