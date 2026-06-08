//! `giant-explain` end to end. The porcelain spawns a `giant session`, so the
//! tests point `GIANT_BIN` at the sibling `giant` binary in the same target dir.

use std::process::Command;

fn explain_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_giant-explain"))
}

/// Sibling `giant` binary. Cargo doesn't expose `CARGO_BIN_EXE_giant` to another
/// package's tests, so derive it from our own path (same target dir).
fn giant_bin() -> std::path::PathBuf {
    let mut p = explain_bin();
    p.set_file_name("giant");
    p
}

fn explain(ws: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(explain_bin())
        .args(args)
        .env("GIANT_BIN", giant_bin())
        .current_dir(ws)
        .output()
        .unwrap()
}

#[test]
fn shows_cache_miss_then_hit() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: explain_test
cache:
  dir: ./cache
targets:
  - name: "demo"
    inputs: ["in.txt"]
    outputs: ["out.txt"]
    env:
      USER_VAR: "hello"
    command: "cp in.txt out.txt"
"#,
    )
    .unwrap();
    std::fs::write(ws.join("in.txt"), "hello world\n").unwrap();

    // Before any build: explain should report 'miss'.
    let out1 = explain(ws, &["//:demo"]);
    assert!(
        out1.status.success(),
        "explain failed: {}",
        String::from_utf8_lossy(&out1.stderr)
    );
    let s1 = String::from_utf8_lossy(&out1.stdout);
    assert!(
        s1.contains("target:      //:demo"),
        "missing target header; got: {s1}"
    );
    assert!(
        s1.contains("cache key:"),
        "missing cache key line; got: {s1}"
    );
    assert!(s1.contains("cache state: miss"), "expected miss; got: {s1}");
    assert!(
        s1.contains("USER_VAR=hello"),
        "user env should appear; got: {s1}"
    );
    assert!(
        s1.contains("GIANT_TARGET_TRIPLE="),
        "built-in env should appear; got: {s1}"
    );
    assert!(s1.contains("in.txt"), "in.txt should be listed; got: {s1}");

    // Build, then re-explain: should report HIT with outputs metadata.
    let build = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(build.status.success());

    let out2 = explain(ws, &["//:demo"]);
    assert!(out2.status.success());
    let s2 = String::from_utf8_lossy(&out2.stdout);
    assert!(s2.contains("cache state: HIT"), "expected HIT; got: {s2}");
    assert!(
        s2.contains("outputs (from cache,"),
        "expected outputs section; got: {s2}"
    );
    assert!(s2.contains("out.txt"), "out.txt should appear; got: {s2}");
    assert!(
        s2.contains("outputs_content_hash:"),
        "expected outputs_content_hash line; got: {s2}"
    );
}

#[test]
fn unknown_target_errors() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: explain_unknown
cache:
  dir: ./cache
targets:
  - name: "real"
    inputs: []
    outputs: ["x"]
    command: "true"
"#,
    )
    .unwrap();
    let out = explain(ws, &["//:ghost"]);
    assert!(!out.status.success(), "unknown target should fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ghost"),
        "stderr should name the target; got: {stderr}"
    );
}

#[test]
fn package_glob_stops_at_subpackage_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        "workspace:\n  name: boundary\ncache:\n  dir: ./cache\n",
    )
    .unwrap();
    std::fs::create_dir_all(ws.join("src/sub")).unwrap();
    std::fs::write(ws.join("src/a.txt"), "a\n").unwrap();
    std::fs::write(ws.join("src/sub/b.txt"), "b\n").unwrap();
    std::fs::write(
        ws.join("src/giant.yaml"),
        r#"
targets:
  - name: gen
    inputs: ["**/*.txt"]
    outputs: ["gen.out"]
    command: "true"
"#,
    )
    .unwrap();
    std::fs::write(
        ws.join("src/sub/giant.yaml"),
        r#"
targets:
  - name: leaf
    inputs: ["b.txt"]
    outputs: ["leaf.out"]
    command: "true"
"#,
    )
    .unwrap();

    let out = explain(ws, &["//src:gen"]);
    assert!(
        out.status.success(),
        "explain failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    // The parent's `**/*.txt` glob hashes its own file but stops at the nested
    // package - it must not claim the child package's b.txt.
    assert!(
        s.contains("src/a.txt"),
        "expected src/a.txt in inputs; got:\n{s}"
    );
    assert!(
        !s.contains("src/sub/b.txt"),
        "parent glob crossed the subpackage boundary; got:\n{s}"
    );
}
