//! End-to-end smoke test: build a workspace twice; first is a cache miss,
//! second is a cache hit that restores outputs from CAS.

use std::process::Command;

fn giant_bin() -> std::path::PathBuf {
    // Cargo sets CARGO_BIN_EXE_<name> for the bin of the package under test.
    // `giant build` dispatches to a `giant-build` binary beside this one, and
    // `cargo test` alone does not link other packages' bins - run
    // `cargo build --workspace` first (CI does).
    let path = env!("CARGO_BIN_EXE_giant");
    std::path::PathBuf::from(path)
}

/// True if some line of `out` mentions `verb` and `id` together. Used
/// to decouple assertions from the exact column widths/spacing the
/// renderer happens to produce.
fn line_has(out: &str, verb: &str, id: &str) -> bool {
    out.lines().any(|l| l.contains(verb) && l.contains(id))
}

fn built(out: &str, id: &str) -> bool {
    line_has(out, "BUILD", id)
}
fn cached(out: &str, id: &str) -> bool {
    line_has(out, "CACHE", id)
}

#[test]
fn build_then_cache_hit_restores_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();

    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: smoke
cache:
  dir: ./cache
targets:
  - name: "hello"
    inputs: []
    outputs: ["hello.txt"]
    command: "echo 'hello from giant' > hello.txt"
"#,
    )
    .unwrap();

    // First run - cache miss, target should build and produce hello.txt.
    let output = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .expect("spawn giant");
    assert!(
        output.status.success(),
        "first build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout1 = String::from_utf8_lossy(&output.stdout);
    assert!(stdout1.contains("built"), "expected built, got: {stdout1}");
    assert_eq!(
        std::fs::read_to_string(ws.join("hello.txt"))
            .unwrap()
            .trim(),
        "hello from giant"
    );

    // Delete the output to prove the cache restores it.
    std::fs::remove_file(ws.join("hello.txt")).unwrap();

    // Second run - cache hit.
    let output = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .expect("spawn giant");
    assert!(output.status.success(), "second build failed");
    let stdout2 = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout2.contains("cache"),
        "expected cache hit, got: {stdout2}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("hello.txt"))
            .unwrap()
            .trim(),
        "hello from giant"
    );
}

#[test]
fn glob_output_captures_and_restores_all_matched_files() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();

    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: glob
cache:
  dir: ./cache
targets:
  - name: "many"
    inputs: []
    outputs: ["gen/*.txt"]
    command: "mkdir -p gen && echo 1 > gen/a.txt && echo 2 > gen/b.txt && echo 3 > gen/c.txt"
"#,
    )
    .unwrap();

    // First build - captures all three matched files, not just an anchor.
    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .expect("spawn giant");
    assert!(
        out.status.success(),
        "first build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Delete the whole tree; a cache hit must restore every matched file.
    std::fs::remove_dir_all(ws.join("gen")).unwrap();

    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .expect("spawn giant");
    assert!(out.status.success(), "second build failed");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("cache"),
        "expected cache hit"
    );
    for (f, want) in [("a.txt", "1"), ("b.txt", "2"), ("c.txt", "3")] {
        assert_eq!(
            std::fs::read_to_string(ws.join("gen").join(f))
                .unwrap()
                .trim(),
            want,
            "{f} not restored from cache"
        );
    }
}

#[test]
fn glob_output_matching_zero_files_fails_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();

    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: empty
cache:
  dir: ./cache
targets:
  - name: "none"
    inputs: []
    outputs: ["gen/*.txt"]
    command: "mkdir -p gen"
"#,
    )
    .unwrap();

    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .expect("spawn giant");
    assert!(
        !out.status.success(),
        "build should fail when a glob output matches no files"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("no files"),
        "expected a zero-match error, got: {combined}"
    );
}

#[test]
fn named_and_glob_outputs_compose() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();

    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: compose
cache:
  dir: ./cache
targets:
  - name: "mixed"
    inputs: []
    outputs:
      - "gen/anchor.txt"
      - "gen/*.txt"
    command: "mkdir -p gen && echo anchor > gen/anchor.txt && echo extra > gen/extra.txt"
"#,
    )
    .unwrap();

    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .expect("spawn giant");
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::remove_dir_all(ws.join("gen")).unwrap();

    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .expect("spawn giant");
    assert!(out.status.success(), "second build failed");
    // The named anchor and the glob-captured extra both restore (the
    // anchor is matched by both entries and deduped).
    assert_eq!(
        std::fs::read_to_string(ws.join("gen/anchor.txt"))
            .unwrap()
            .trim(),
        "anchor"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("gen/extra.txt"))
            .unwrap()
            .trim(),
        "extra"
    );
}

#[test]
fn build_with_dep_chain_runs_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();

    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: chain
cache:
  dir: ./cache
targets:
  - name: "a"
    inputs: []
    outputs: ["a.txt"]
    command: "echo a > a.txt"
  - name: "b"
    inputs: ["a.txt"]
    outputs: ["b.txt"]
    deps: [":a"]
    command: "cat a.txt > b.txt && echo b >> b.txt"
"#,
    )
    .unwrap();

    let output = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .expect("spawn giant");
    assert!(
        output.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let content = std::fs::read_to_string(ws.join("b.txt")).unwrap();
    assert_eq!(content, "a\nb\n");
}

#[test]
fn failing_target_propagates_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();

    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: fail
cache:
  dir: ./cache
targets:
  - name: "bad"
    inputs: []
    outputs: []
    cache: false
    command: "exit 7"
"#,
    )
    .unwrap();

    let output = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .expect("spawn giant");
    assert!(!output.status.success(), "expected nonzero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("FAIL") || stderr.contains("failed"),
        "expected failure mention; stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn early_cutoff_byte_identical_upstream_doesnt_invalidate_downstream() {
    // Property under test (early cutoff): if an upstream target
    // rebuilds because its inputs changed, but its outputs come out
    // byte-identical, downstream should *not* rebuild - its cache key is
    // computed from upstream's output content hash, not upstream's cache
    // key.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();

    std::fs::write(ws.join("a.in"), "v1\n").unwrap();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: cutoff
cache:
  dir: ./cache
targets:
  - name: "a"
    inputs: ["a.in"]
    outputs: ["a.out"]
    command: "echo constant > a.out"   # deterministic output regardless of a.in
  - name: "b"
    inputs: ["a.out"]
    outputs: ["b.out"]
    deps: [":a"]
    command: "cat a.out > b.out"
"#,
    )
    .unwrap();

    // First build: both run.
    let out1 = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out1.status.success(), "first build failed");
    let s1 = String::from_utf8_lossy(&out1.stdout);
    assert!(built(&s1, "//:a"), "expected a built; got: {s1}");
    assert!(built(&s1, "//:b"), "expected b built; got: {s1}");

    // Edit a.in. a's cache key will change (its input content changed)
    // and a will rebuild. But a's command is `echo constant > a.out`, so
    // a.out is byte-identical to the previous run. b's dep contribution
    // is a's outputs_content_hash - unchanged - so b cache-hits.
    std::fs::write(ws.join("a.in"), "v2\n").unwrap();

    let out2 = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out2.status.success(), "second build failed");
    let s2 = String::from_utf8_lossy(&out2.stdout);
    assert!(
        built(&s2, "//:a"),
        "expected a to rebuild (its input changed); got: {s2}"
    );
    assert!(
        cached(&s2, "//:b"),
        "expected b to cache-hit (a's output bytes unchanged); got: {s2}"
    );
}

#[test]
fn parallel_dispatch_runs_independent_targets_concurrently() {
    // Two independent targets, each sleeps 300ms. With --jobs 2 the
    // total wall time should be roughly one sleep (~300ms), not two
    // (~600ms). We assert under 500ms to give generous headroom for
    // process spawn + cache write + slow CI.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: par
cache:
  dir: ./cache
targets:
  - name: "a"
    inputs: []
    outputs: ["a.txt"]
    cache: false
    command: "sleep 0.3 && echo a > a.txt"
  - name: "b"
    inputs: []
    outputs: ["b.txt"]
    cache: false
    command: "sleep 0.3 && echo b > b.txt"
"#,
    )
    .unwrap();

    let start = std::time::Instant::now();
    let output = Command::new(giant_bin())
        .args(["build", "-j", "2"])
        .current_dir(ws)
        .output()
        .unwrap();
    let elapsed = start.elapsed();
    assert!(
        output.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        elapsed.as_millis() < 500,
        "expected parallel wall time <500ms (single sleep + overhead), got {elapsed:?}"
    );
}

#[test]
fn serial_dispatch_runs_targets_sequentially() {
    // Same fixture, but --jobs 1. Wall time should be >= 2 × sleep
    // (~600ms). Proves the serial-mode lower bound and validates that
    // parallelism actually scales work (vs the previous test passing for
    // unrelated reasons).
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: ser
cache:
  dir: ./cache
targets:
  - name: "a"
    inputs: []
    outputs: ["a.txt"]
    cache: false
    command: "sleep 0.3 && echo a > a.txt"
  - name: "b"
    inputs: []
    outputs: ["b.txt"]
    cache: false
    command: "sleep 0.3 && echo b > b.txt"
"#,
    )
    .unwrap();

    let start = std::time::Instant::now();
    let output = Command::new(giant_bin())
        .args(["build", "-j", "1"])
        .current_dir(ws)
        .output()
        .unwrap();
    let elapsed = start.elapsed();
    assert!(output.status.success());
    assert!(
        elapsed.as_millis() >= 580,
        "serial wall time should be ≥2× sleep (~600ms), got {elapsed:?}"
    );
}

#[test]
fn exists_check_succeeding_skips_build_command() {
    // `exists:` says "yes, it's already there" → build command must not
    // run. We prove the command didn't run by having it write a marker
    // file and asserting the file's absence.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: existscheck
cache:
  dir: ./cache
targets:
  - name: "img"
    inputs: ["Dockerfile"]
    outputs: []
    cache: false
    command: "echo SHOULD_NOT_RUN > marker.txt"
    exists: "true"
"#,
    )
    .unwrap();
    std::fs::write(ws.join("Dockerfile"), "FROM scratch\n").unwrap();

    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("EXTERNAL"), "expected external hit; got: {s}");
    assert!(
        !ws.join("marker.txt").exists(),
        "build command must not have run when exists: returned 0"
    );
}

#[test]
fn exists_check_failing_falls_through_to_build() {
    // `exists:` says "no, not there" → build command runs normally.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: existsmiss
cache:
  dir: ./cache
targets:
  - name: "img"
    inputs: ["Dockerfile"]
    outputs: ["receipt.txt"]
    command: "echo built > receipt.txt"
    exists: "false"
"#,
    )
    .unwrap();
    std::fs::write(ws.join("Dockerfile"), "FROM scratch\n").unwrap();

    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out.status.success(), "build failed");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(built(&s, "//:img"), "expected build to run; got: {s}");
    assert_eq!(
        std::fs::read_to_string(ws.join("receipt.txt"))
            .unwrap()
            .trim(),
        "built"
    );
}

#[test]
fn exists_check_sees_cache_key_in_env() {
    // The exists command can reference $GIANT_CACHE_KEY (this is the
    // whole point - registry tag the artifact by Giant's identity).
    // We assert succeess when $GIANT_CACHE_KEY is non-empty and 64 hex
    // chars.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: existsenv
cache:
  dir: ./cache
targets:
  - name: "img"
    inputs: []
    outputs: []
    cache: false
    command: "echo SHOULD_NOT_RUN > marker.txt"
    exists: 'test "${#GIANT_CACHE_KEY}" = 64'
"#,
    )
    .unwrap();
    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("EXTERNAL"), "expected external hit; got: {s}");
    assert!(!ws.join("marker.txt").exists());
}

#[test]
fn command_env_exposes_workspace_root_and_package_dir() {
    // A target in a nested package can reference $GIANT_WORKSPACE_ROOT and
    // $GIANT_PACKAGE_DIR. The latter is the package's own directory: the
    // workspace root joined with the package path. We verify the relationship
    // through `exists` (which sets the same env) - equality means an EXTERNAL
    // hit and the build command never runs.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: envpaths
cache:
  dir: ./cache
"#,
    )
    .unwrap();
    std::fs::create_dir_all(ws.join("src/app")).unwrap();
    std::fs::write(
        ws.join("src/app/giant.yaml"),
        r#"
targets:
  - name: "app"
    inputs: []
    outputs: []
    cache: false
    command: "echo SHOULD_NOT_RUN > marker.txt"
    exists: 'test "$GIANT_PACKAGE_DIR" = "$GIANT_WORKSPACE_ROOT/src/app"'
"#,
    )
    .unwrap();
    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("EXTERNAL"),
        "expected external hit (env paths matched); got: {s}"
    );
    assert!(!ws.join("src/app/marker.txt").exists());
}

#[test]
fn affected_with_file_only_runs_matching_targets() {
    // No git involved - pass --file directly. Verifies the
    // selection::affected_targets path: only `a` (whose input matches
    // src/a/main.go) should run; `b` is untouched.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::create_dir_all(ws.join("src/a")).unwrap();
    std::fs::create_dir_all(ws.join("src/b")).unwrap();
    std::fs::write(ws.join("src/a/main.go"), "package main\n").unwrap();
    std::fs::write(ws.join("src/b/main.go"), "package main\n").unwrap();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: affected_file
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
"#,
    )
    .unwrap();

    let out = Command::new(giant_bin())
        .args(["build", "--affected", "--file", "src/a/main.go"])
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(built(&s, "//:a"), "a should build; got: {s}");
    assert!(!s.contains("//:b"), "b should not appear; got: {s}");
    assert!(!ws.join("b.out").exists(), "b.out should not exist");
}

#[test]
fn affected_with_base_uses_git_diff() {
    // Real git workflow: commit baseline, modify one file, run with
    // --base HEAD. Only the affected target should run.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::create_dir_all(ws.join("src/a")).unwrap();
    std::fs::create_dir_all(ws.join("src/b")).unwrap();
    std::fs::write(ws.join("src/a/main.go"), "package main\nfunc f() {}\n").unwrap();
    std::fs::write(ws.join("src/b/main.go"), "package main\nfunc g() {}\n").unwrap();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: affected_git
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
"#,
    )
    .unwrap();

    let run_git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(ws)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git available");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run_git(&["init", "-q", "--initial-branch=main"]);
    run_git(&["add", "."]);
    run_git(&["commit", "-q", "-m", "init"]);

    // Modify src/a only; src/b untouched.
    std::fs::write(
        ws.join("src/a/main.go"),
        "package main\nfunc f() { /* updated */ }\n",
    )
    .unwrap();

    let out = Command::new(giant_bin())
        .args(["build", "--affected", "--base", "HEAD"])
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(built(&s, "//:a"), "a should rebuild; got: {s}");
    assert!(!s.contains("//:b"), "b should not appear; got: {s}");
}

#[test]
fn affected_with_no_changes_exits_cleanly() {
    // git clean + no edits → no affected targets. Should exit 0 with a
    // friendly message, not bail with an error.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("src/main.go"), "package main\n").unwrap();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: affected_nochanges
cache:
  dir: ./cache
targets:
  - name: "a"
    inputs: ["src/**/*"]
    outputs: ["a.out"]
    command: "echo a > a.out"
"#,
    )
    .unwrap();
    let run_git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(ws)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git available");
        assert!(out.status.success(), "git {args:?} failed");
    };
    run_git(&["init", "-q", "--initial-branch=main"]);
    run_git(&["add", "."]);
    run_git(&["commit", "-q", "-m", "init"]);

    let out = Command::new(giant_bin())
        .args(["build", "--affected", "--base", "HEAD"])
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "clean build with no changes should exit 0"
    );
    // The renderer's `note` helper writes to stdout for consistency
    // with the rest of the build output.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("no affected"),
        "expected 'no affected' message; got: {stdout}"
    );
}

#[test]
fn affected_walks_downstream_transitively() {
    // a's output feeds b's input, and b declares the dep. Editing src/a/*
    // should make BOTH a and b run (b is transitively affected).
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::create_dir_all(ws.join("src/a")).unwrap();
    std::fs::write(ws.join("src/a/main.go"), "v1\n").unwrap();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: affected_downstream
cache:
  dir: ./cache
targets:
  - name: "a"
    inputs: ["src/a/**/*"]
    outputs: ["a.out"]
    command: "cp src/a/main.go a.out"
  - name: "b"
    inputs: ["a.out"]
    deps: ["//:a"]
    outputs: ["b.out"]
    command: "cp a.out b.out"
"#,
    )
    .unwrap();

    let out = Command::new(giant_bin())
        .args(["build", "--affected", "--file", "src/a/main.go"])
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(built(&s, "//:a"), "a should build; got: {s}");
    assert!(
        built(&s, "//:b"),
        "b should also build (transitive); got: {s}"
    );
}

#[test]
#[cfg(unix)]
fn watch_rebuilds_on_file_change() {
    // Spawn `giant watch` in the background, edit a file, give it time
    // to rebuild, then SIGINT for a clean exit. Verifies the post-edit
    // file reflects the new content.
    use std::io::Read;
    use std::process::Stdio;
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("src/in.txt"), "v1\n").unwrap();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: watch_test
cache:
  dir: ./cache
targets:
  - name: "demo"
    inputs: ["src/in.txt"]
    outputs: ["out.txt"]
    command: "cp src/in.txt out.txt"
"#,
    )
    .unwrap();

    let mut child = Command::new(giant_bin())
        .args(["build", "--watch"])
        .current_dir(ws)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn giant build --watch");
    let pid = child.id() as i32;

    // Give it time to complete initial build and start watching.
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(
        std::fs::read_to_string(ws.join("out.txt")).unwrap().trim(),
        "v1",
        "initial build should produce out.txt with v1"
    );

    // Edit the input.
    std::fs::write(ws.join("src/in.txt"), "v2_edited\n").unwrap();

    // Wait for the debouncer + rebuild.
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(
        std::fs::read_to_string(ws.join("out.txt")).unwrap().trim(),
        "v2_edited",
        "watch should have rebuilt after edit"
    );

    // Clean shutdown via SIGINT.
    unsafe {
        libc::kill(pid, libc::SIGINT);
    }
    let _ = child.wait();

    // Sanity: stdout should show the initial build's BUILD line for
    // `demo` (the renderer's output, all on stdout now).
    let mut buf = String::new();
    let _ = child.stdout.take().unwrap().read_to_string(&mut buf);
    assert!(
        line_has(&buf, "BUILD", "//:demo"),
        "expected initial build of demo in watch output; got: {buf}"
    );
}

#[test]
#[cfg(unix)]
fn watch_respects_pattern_selection() {
    // Spawn `giant watch //:server` against a fixture with both
    // `//:server` (matches) and `//:util` (excluded). Editing
    // the lib's input should NOT trigger a rebuild of the lib; editing
    // the bin's input should rebuild the bin. Verifies the selection
    // language is the same one watch enforces per-cycle.
    use std::io::Read;
    use std::process::Stdio;
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("src/bin.txt"), "b1\n").unwrap();
    std::fs::write(ws.join("src/lib.txt"), "l1\n").unwrap();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: watch_pat
cache:
  dir: ./cache
targets:
  - name: "server"
    inputs: ["src/bin.txt"]
    outputs: ["bin.out"]
    command: "cp src/bin.txt bin.out"
  - name: "util"
    inputs: ["src/lib.txt"]
    outputs: ["lib.out"]
    command: "cp src/lib.txt lib.out"
"#,
    )
    .unwrap();

    let mut child = Command::new(giant_bin())
        .args(["build", "//:server", "--watch"])
        .current_dir(ws)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn giant build --watch");
    let pid = child.id() as i32;

    // Initial build should only touch the bin target.
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(
        std::fs::read_to_string(ws.join("bin.out")).unwrap().trim(),
        "b1",
        "initial build should produce bin.out"
    );
    assert!(
        !ws.join("lib.out").exists(),
        "lib.out should not be built - //:util is outside the selection"
    );

    // Edit the lib's input. The watcher will see the change and run a
    // cycle, but the pattern filter must drop //:util from the
    // selection, leaving no targets to build (//:util is filtered out).
    std::fs::write(ws.join("src/lib.txt"), "l2\n").unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    assert!(
        !ws.join("lib.out").exists(),
        "lib.out must still not exist after editing src/lib.txt"
    );

    // Edit the bin's input. This one IS in the selection - must rebuild.
    std::fs::write(ws.join("src/bin.txt"), "b2\n").unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(
        std::fs::read_to_string(ws.join("bin.out")).unwrap().trim(),
        "b2",
        "bin should rebuild after its input changed"
    );

    unsafe {
        libc::kill(pid, libc::SIGINT);
    }
    let _ = child.wait();

    let mut buf = String::new();
    let _ = child.stdout.take().unwrap().read_to_string(&mut buf);
    // A `no targets affected` note proves the lib-edit cycle ran with
    // the filter in place - otherwise we'd see a BUILD for //:util.
    assert!(
        buf.contains("no targets affected"),
        "expected a 'no targets affected' cycle after editing the excluded lib input; got: {buf}"
    );
    assert!(
        !line_has(&buf, "BUILD", "//:util"),
        "//:util must never appear as built in watch output; got: {buf}"
    );
}

#[test]
fn cache_hit_replays_captured_stdout() {
    // A target prints "captured output:42" on its first run. The second
    // build should be a cache hit and the renderer should still surface
    // that same line - proving capture (build 1) + replay (build 2)
    // round-trip through the local cache.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: logcap
cache:
  dir: ./cache
targets:
  - name: "logs"
    inputs: []
    outputs: ["out.txt"]
    command: "echo captured-marker-42 && echo out > out.txt"
"#,
    )
    .unwrap();

    let out1 = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        out1.status.success(),
        "first build failed: {}",
        String::from_utf8_lossy(&out1.stderr)
    );
    let s1 = String::from_utf8_lossy(&out1.stdout);
    assert!(
        s1.contains("captured-marker-42"),
        "first build should emit live stdout; got: {s1}"
    );

    let out2 = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out2.status.success(), "second build failed");
    let s2 = String::from_utf8_lossy(&out2.stdout);
    assert!(
        cached(&s2, "//:logs"),
        "second build should cache-hit; got: {s2}"
    );
    assert!(
        s2.contains("captured-marker-42"),
        "cache hit should replay captured stdout; got: {s2}"
    );
}

#[test]
fn cache_hit_replay_disabled_by_config() {
    // With cache.replay_logs: false, the second build still hits the
    // cache but the captured stdout must NOT reappear in renderer
    // output.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: logcap2
cache:
  dir: ./cache
  replay_logs: false
targets:
  - name: "noreplay"
    inputs: []
    outputs: ["out.txt"]
    command: "echo no-replay-marker-99 && echo out > out.txt"
"#,
    )
    .unwrap();

    let out1 = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out1.status.success());
    let s1 = String::from_utf8_lossy(&out1.stdout);
    assert!(s1.contains("no-replay-marker-99"));

    let out2 = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out2.status.success());
    let s2 = String::from_utf8_lossy(&out2.stdout);
    assert!(
        cached(&s2, "//:noreplay"),
        "second build should cache-hit; got: {s2}"
    );
    assert!(
        !s2.contains("no-replay-marker-99"),
        "replay_logs:false should suppress replay; got: {s2}"
    );
}

#[test]
fn cache_capture_disabled_then_no_replay() {
    // With cache.capture_logs: false, no log blobs are stored. The
    // second build still cache-hits, but since nothing was captured,
    // no logs are replayed.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: logcap3
cache:
  dir: ./cache
  capture_logs: false
targets:
  - name: "nocapture"
    inputs: []
    outputs: ["out.txt"]
    command: "echo nocapture-marker-7 && echo out > out.txt"
"#,
    )
    .unwrap();

    let out1 = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out1.status.success());
    assert!(String::from_utf8_lossy(&out1.stdout).contains("nocapture-marker-7"));

    let out2 = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out2.status.success());
    let s2 = String::from_utf8_lossy(&out2.stdout);
    assert!(cached(&s2, "//:nocapture"), "got: {s2}");
    assert!(
        !s2.contains("nocapture-marker-7"),
        "no capture should mean no replay; got: {s2}"
    );
}

#[test]
fn cache_miss_when_command_changes() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();

    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: cmd
cache:
  dir: ./cache
targets:
  - name: "demo"
    inputs: []
    outputs: ["out.txt"]
    command: "echo first > out.txt"
"#,
    )
    .unwrap();

    let out1 = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out1.status.success());

    // Change the command.
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: cmd
cache:
  dir: ./cache
targets:
  - name: "demo"
    inputs: []
    outputs: ["out.txt"]
    command: "echo second > out.txt"
"#,
    )
    .unwrap();

    let out2 = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out2.status.success());
    let stdout = String::from_utf8_lossy(&out2.stdout);
    assert!(
        stdout.contains("built"),
        "command change should miss cache; got: {stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("out.txt")).unwrap().trim(),
        "second"
    );
}

#[test]
fn cache_false_target_reruns_every_build() {
    // `cache: false` opts the target out of the cache entirely: it runs
    // for its side effects on every build, never a cache hit.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();

    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: nocache
cache:
  dir: ./cache
targets:
  - name: "lint"
    inputs: []
    outputs: []
    cache: false
    command: "echo ran >> runs.log"
"#,
    )
    .unwrap();

    for _ in 0..2 {
        let out = Command::new(giant_bin())
            .arg("build")
            .current_dir(ws)
            .output()
            .expect("spawn giant");
        assert!(
            out.status.success(),
            "build failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // Both builds ran the command → two lines. A cached second run would
    // leave only one.
    let log = std::fs::read_to_string(ws.join("runs.log")).unwrap();
    assert_eq!(
        log.lines().count(),
        2,
        "cache:false target should run on every build; runs.log={log:?}"
    );
}

#[test]
fn timeout_secs_kills_long_running_target() {
    // A target that exceeds `timeout_secs` is killed and fails, rather
    // than hanging for the full command duration.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();

    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: timeout
cache:
  dir: ./cache
targets:
  - name: "slow"
    inputs: []
    outputs: ["out.txt"]
    timeout_secs: 1
    command: "sleep 30 && echo done > out.txt"
"#,
    )
    .unwrap();

    let start = std::time::Instant::now();
    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .expect("spawn giant");
    let elapsed = start.elapsed();

    assert!(
        !out.status.success(),
        "timed-out target should fail the build"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "build should abort near the 1s timeout, not wait for sleep 30 (took {elapsed:?})"
    );
    assert!(
        !ws.join("out.txt").exists(),
        "command was killed before producing its output"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("timed out"),
        "expected a timeout message; got: {combined}"
    );
}

#[test]
fn multi_package_scan_resolves_labels_and_package_relative_paths() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    // Root config marks the workspace; it has no targets of its own.
    std::fs::write(
        ws.join("giant.yaml"),
        "workspace:\n  name: multipkg\ncache:\n  dir: ./cache\n",
    )
    .unwrap();
    // A package at src/lib with package-relative input + output.
    std::fs::create_dir_all(ws.join("src/lib")).unwrap();
    std::fs::write(ws.join("src/lib/in.txt"), "data\n").unwrap();
    std::fs::write(
        ws.join("src/lib/giant.yaml"),
        r#"
targets:
  - name: build
    inputs: ["in.txt"]
    outputs: ["out.txt"]
    command: "cat in.txt > out.txt"
"#,
    )
    .unwrap();

    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    // Label is derived from the package directory.
    assert!(
        built(&s, "//src/lib:build"),
        "expected //src/lib:build to build; got: {s}"
    );
    // Output resolved package-relative; cwd defaulted to the package dir,
    // so `cat in.txt > out.txt` read and wrote inside src/lib.
    assert_eq!(
        std::fs::read_to_string(ws.join("src/lib/out.txt"))
            .unwrap()
            .trim(),
        "data"
    );

    // Selecting by the path-derived label builds just that target.
    let out2 = Command::new(giant_bin())
        .arg("build")
        .arg("//src/lib:build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out2.status.success());
    let s2 = String::from_utf8_lossy(&out2.stdout);
    assert!(
        cached(&s2, "//src/lib:build"),
        "expected cache hit; got: {s2}"
    );
}

#[test]
fn package_produces_root_artifact_via_rooted_output_and_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        "workspace:\n  name: rooted\ncache:\n  dir: ./cache\n",
    )
    .unwrap();
    std::fs::create_dir_all(ws.join("src/tool")).unwrap();
    std::fs::write(ws.join("src/tool/main.txt"), "tool-data\n").unwrap();
    // Package-relative input, `//`-rooted output, and `//` cwd (run at the
    // workspace root) - all three resolved by the loader.
    std::fs::write(
        ws.join("src/tool/giant.yaml"),
        r#"
targets:
  - name: build
    inputs: ["main.txt"]
    outputs: ["//bin/tool"]
    cwd: "//"
    command: "mkdir -p bin && cat src/tool/main.txt > bin/tool"
"#,
    )
    .unwrap();

    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(built(&s, "//src/tool:build"), "got: {s}");
    // The root artifact landed at the workspace root, not under the package.
    assert_eq!(
        std::fs::read_to_string(ws.join("bin/tool")).unwrap().trim(),
        "tool-data"
    );

    // Second build is a cache hit (the //-rooted output was captured).
    let out2 = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out2.status.success());
    assert!(cached(
        &String::from_utf8_lossy(&out2.stdout),
        "//src/tool:build"
    ));
}

#[test]
fn failed_last_reselects_only_the_failures() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: faillast
cache:
  dir: ./cache
targets:
  - name: ok
    outputs: ["ok.txt"]
    command: "echo ok > ok.txt"
  - name: bad
    outputs: ["bad.txt"]
    command: "exit 1"
"#,
    )
    .unwrap();

    // First build: //:ok succeeds, //:bad fails (and never produces bad.txt).
    let out1 = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        !out1.status.success(),
        "a build with a failing target should exit non-zero"
    );

    // `failed-last` re-selects only the target that failed.
    let out2 = Command::new(giant_bin())
        .arg("build")
        .arg("failed-last")
        .current_dir(ws)
        .output()
        .unwrap();
    let s2 = String::from_utf8_lossy(&out2.stdout);
    assert!(
        s2.contains("//:bad"),
        "failed-last should rebuild //:bad; got:\n{s2}"
    );
    assert!(
        !s2.contains("//:ok"),
        "failed-last should NOT touch the target that passed; got:\n{s2}"
    );
}

#[test]
fn failed_last_with_no_failures_errors() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        "workspace:\n  name: nofail\ncache:\n  dir: ./cache\ntargets:\n  - name: ok\n    outputs: [\"ok.txt\"]\n    command: \"echo ok > ok.txt\"\n",
    )
    .unwrap();
    // Clean build, then failed-last has nothing to do.
    assert!(
        Command::new(giant_bin())
            .arg("build")
            .current_dir(ws)
            .output()
            .unwrap()
            .status
            .success()
    );
    let out = Command::new(giant_bin())
        .arg("build")
        .arg("failed-last")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no recent failures"),
        "expected a 'no recent failures' message"
    );
}

#[test]
fn high_output_target_does_not_deadlock_on_pipe_buffer() {
    // A target that floods stdout with far more than the OS pipe buffer
    // (~64K). Giant must drain the child's stdout concurrently with waiting
    // on it; draining only after `wait()` deadlocks - the child blocks on
    // write, never exits, and the target hangs until its `timeout_secs`.
    //
    // The regression guard is `status.success()` bounded by `timeout_secs`:
    // a deadlock hits the timeout and fails the target, so success proves
    // it drained. We don't assert a tight wall-clock bound - with log
    // capture on (the `remote` feature) the flood is captured + stored,
    // which is legitimately slow on a busy CI runner without being a hang.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: highout
cache:
  dir: ./cache
targets:
  - name: "flood"
    inputs: []
    outputs: []
    cache: false
    timeout_secs: 60
    command: "seq 1 100000"
"#,
    )
    .unwrap();

    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .expect("spawn giant");
    assert!(
        out.status.success(),
        "high-output target should complete, not deadlock (a hang hits the \
         60s timeout and fails): {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
