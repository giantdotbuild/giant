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
//! - No-op `giant build` at 1k targets (everything cached): <500ms
//!
//! These are not micro-benchmarks. They measure the realistic
//! end-to-end shape a real monorepo would see.

use std::process::Command;
use std::time::Instant;

fn giant_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_giant"))
}

fn write_yaml(root: &std::path::Path, body: &str) {
    std::fs::write(root.join("giant.yaml"), body).unwrap();
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
