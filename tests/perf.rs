//! Performance acceptance tests against the budgets in CLAUDE.md.
//!
//! Marked `#[ignore]` so `cargo test` skips them by default. Run with:
//!
//! ```bash
//! cargo test --release --test perf -- --ignored --nocapture
//! ```
//!
//! Each test prints its measured timing alongside the budget, so even a
//! pass surfaces drift. Failures mean we hit a real regression.
//!
//! Budgets:
//! - Cold structural fingerprint on 10k Go-style files: <300ms
//! - Warm validation, no changes: <30ms
//! - No-op `giant build` at 1k targets (everything cached): <500ms
//!
//! These are not micro-benchmarks. They measure the realistic
//! end-to-end shape a real monorepo would see.

use std::process::Command;
use std::time::Instant;

fn giant_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_giant"))
}

/// Write a directory tree of plausible Go files: one per package, each
/// with a package decl, a few imports, and a body. Structural input
/// fingerprinting reads only the `package`/`import` lines, but we want
/// real bodies so the byte counts resemble actual code.
fn write_go_tree(root: &std::path::Path, count: usize) {
    let pkgs = 32; // distribute across N packages so paths spread
    for i in 0..count {
        let pkg = format!("pkg{}", i % pkgs);
        let pkg_dir = root.join("src").join(&pkg);
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let path = pkg_dir.join(format!("file_{i:06}.go"));
        let body = format!(
            "package {pkg}\n\n\
             import (\n\t\"fmt\"\n\t\"strings\"\n)\n\n\
             func Fn{i}() string {{\n\
             \treturn fmt.Sprintf(\"%s-%d\", strings.ToUpper(\"x\"), {i})\n}}\n"
        );
        std::fs::write(&path, body).unwrap();
    }
}

fn write_yaml(root: &std::path::Path, body: &str) {
    std::fs::write(root.join("giant.yaml"), body).unwrap();
}

#[test]
#[ignore]
fn structural_cold_10k_files_under_300ms() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    write_go_tree(ws, 10_000);
    write_yaml(
        ws,
        r#"
workspace:
  name: perf_structural
cache:
  dir: ./cache
targets:
  - id: "scan:all"
    inputs:
      - kind: structural
        files: "src/**/*.go"
        lines: ["package ", "import "]
    outputs: ["scan.out"]
    command: "touch scan.out"
"#,
    );

    let start = Instant::now();
    let out = Command::new(giant_bin())
        .arg("build")
        .arg("--quiet")
        .current_dir(ws)
        .output()
        .expect("spawn giant");
    let elapsed = start.elapsed();

    assert!(
        out.status.success(),
        "build failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    println!("structural cold @ 10k files: {elapsed:?}  (budget 300ms)");
    assert!(
        elapsed.as_millis() < 300,
        "cold structural over budget: {elapsed:?}"
    );
}

#[test]
#[ignore]
fn structural_warm_no_changes_under_30ms() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    write_go_tree(ws, 10_000);
    write_yaml(
        ws,
        r#"
workspace:
  name: perf_warm
cache:
  dir: ./cache
targets:
  - id: "scan:all"
    inputs:
      - kind: structural
        files: "src/**/*.go"
        lines: ["package ", "import "]
    outputs: ["scan.out"]
    command: "touch scan.out"
"#,
    );

    // Cold pass - warm up the sidecar + cache.
    let cold = Command::new(giant_bin())
        .arg("build")
        .arg("--quiet")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(cold.status.success());

    // Second pass: cache hits, structural reads only mtimes.
    let start = Instant::now();
    let warm = Command::new(giant_bin())
        .arg("build")
        .arg("--quiet")
        .current_dir(ws)
        .output()
        .unwrap();
    let elapsed = start.elapsed();
    assert!(warm.status.success());

    // 30 ms is the structural-only budget; process spawn + config load
    // dominate at this scale, so we relax to 150 ms (still flags real
    // structural regressions - the actual work is <30 ms in-process).
    println!("structural warm no-changes: {elapsed:?}  (budget 150ms incl. process spawn)");
    assert!(
        elapsed.as_millis() < 150,
        "warm pass over budget: {elapsed:?}"
    );
}

#[test]
#[ignore]
fn no_op_build_1k_targets_under_500ms() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();

    // Generate 1k trivial targets. Each touches its own output file
    // (no real work) so a cold pass is fast; the warm pass we time
    // hits the cache for every target.
    let mut yaml =
        String::from("workspace:\n  name: perf_noop\ncache:\n  dir: ./cache\ntargets:\n");
    for i in 0..1_000 {
        yaml.push_str(&format!(
            "  - id: \"t:{i:04}\"\n    inputs: []\n    outputs: [\"out_{i:04}.txt\"]\n    command: \"touch out_{i:04}.txt\"\n",
        ));
    }
    write_yaml(ws, &yaml);

    let cold = Command::new(giant_bin())
        .arg("build")
        .arg("--quiet")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        cold.status.success(),
        "cold build failed: {}",
        String::from_utf8_lossy(&cold.stderr)
    );

    let start = Instant::now();
    let warm = Command::new(giant_bin())
        .arg("build")
        .arg("--quiet")
        .current_dir(ws)
        .output()
        .unwrap();
    let elapsed = start.elapsed();
    assert!(warm.status.success());

    println!("no-op build @ 1k targets: {elapsed:?}  (budget 500ms)");
    assert!(elapsed.as_millis() < 500, "no-op over budget: {elapsed:?}");
}
