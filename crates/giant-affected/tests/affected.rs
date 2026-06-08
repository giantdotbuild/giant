//! `giant-affected` end to end: drive the binary with `--file` (no git needed)
//! and assert it lists the affected target ids without building anything.

use std::process::Command;

fn bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_giant-affected"))
}

const WS: &str = r#"
workspace:
  name: aff
cache:
  dir: ./cache
targets:
  - name: "a"
    inputs: ["src/a/**/*"]
    outputs: ["a.out"]
    command: "echo a > a.out"
  - name: "b"
    inputs: ["src/b/**/*"]
    outputs: ["b.out"]
    command: "echo b > b.out"
  - name: "c"
    inputs: ["a.out"]
    deps: ["//:a"]
    outputs: ["c.out"]
    command: "cp a.out c.out"
"#;

#[test]
fn lists_affected_targets_without_building() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::create_dir_all(ws.join("src/a")).unwrap();
    std::fs::create_dir_all(ws.join("src/b")).unwrap();
    std::fs::write(ws.join("src/a/main.go"), "package main\n").unwrap();
    std::fs::write(ws.join("src/b/main.go"), "package main\n").unwrap();
    std::fs::write(ws.join("giant.yaml"), WS).unwrap();

    let out = Command::new(bin())
        .args(["--file", "src/a/main.go"])
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "affected failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // a is directly affected; c depends on a (downstream); b is untouched.
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        vec!["//:a", "//:c"],
        "got: {stdout}"
    );
    // Nothing was built.
    assert!(!ws.join("a.out").exists(), "affected must not build");
    assert!(!ws.join("c.out").exists());
}

#[test]
fn no_match_is_clean_empty_exit() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::create_dir_all(ws.join("src/a")).unwrap();
    std::fs::write(ws.join("src/a/main.go"), "package main\n").unwrap();
    std::fs::write(ws.join("giant.yaml"), WS).unwrap();

    let out = Command::new(bin())
        .args(["--file", "README.md"])
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out.status.success(), "no matches should exit 0");
    assert!(
        String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        "expected empty stdout"
    );
}
